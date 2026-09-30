//! Sharing images through an OCI registry (ghcr.io by default).
//!
//! An image is pushed as an OCI artifact: the compressed qcow2 split into parts plus the
//! firmware vars. Only the disk travels: the RAM snapshot depends on the Mac's chip and
//! QEMU version, so it is recaptured after every pull.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use futures::{StreamExt, TryStreamExt};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::instance::{Image, Os, cache_dir, images_dir};
use crate::ops::run;
use crate::{image, log};

const DEFAULT_REPO: &str = "ghcr.io/pawanpaudel93/agentpc";
const ARTIFACT_TYPE: &str = "application/vnd.agentpc.image.v1";
const CONFIG_TYPE: &str = "application/vnd.agentpc.image.config.v1+json";
const PART_TYPE: &str = "application/vnd.agentpc.disk.qcow2.part";
const VARS_TYPE: &str = "application/vnd.agentpc.efi-vars";
// Small parts: on a flaky uplink a timeout costs one part, and retries skip parts already pushed.
const PART_SIZE: &str = "64m";
const PUSH_ATTEMPTS: u32 = 5;
const PARALLEL_DOWNLOADS: usize = 6;

/// One package for all images, tagged by image name (`<repo>:ubuntu-24.04`).
/// `AGENTPC_IMAGE_REPO` overrides the repo (e.g. a local test registry).
fn reference(tag: &str) -> String {
    let repo = std::env::var("AGENTPC_IMAGE_REPO").unwrap_or_else(|_| DEFAULT_REPO.into());
    format!("{repo}:{tag}")
}

fn plain_http(host: &str) -> bool {
    host.starts_with("localhost") || host.starts_with("127.0.0.1")
}

/// Publish the local image (maintainers). Uses `oras` and its stored login.
pub fn push(image: &Image) -> Result<()> {
    let os = image.os;
    if os == Os::Windows {
        bail!("Windows images can't be redistributed (Microsoft license); users build their own");
    }
    let cmd = crate::setup::cmd_name();
    // Its tag already carries a date; pushing would add a second one.
    if image.pinned() {
        bail!("{image} is a dated published build; push the image it pins instead");
    }
    let oras =
        crate::qemu::which("oras").context("oras not found; install it with: brew install oras")?;
    if !image.exists() {
        bail!("no {image} image to push; run: {cmd} image build {image}");
    }
    // Held while the work dir exists, so `clean` leaves a live push's parts alone.
    let _push = crate::instance::lock(
        &push_lock(image),
        Some(&format!("waiting for another push of {image} to finish")),
    )?;
    let mut info = image::read_info(image).with_context(|| {
        format!("{image} has no version info; run: {cmd} image snapshot {image}")
    })?;
    let work = cache_dir().join(format!("push-{image}"));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;

    log!("compressing the {image} image");
    let disk = work.join("disk.qcow2");
    run(
        "qemu-img",
        &[
            "convert",
            "-c",
            "-O",
            "qcow2",
            &image.disk().to_string_lossy(),
            &disk.to_string_lossy(),
        ],
    )?;
    // Parts keep each blob small enough for reliable uploads and parallel downloads.
    let st = Command::new("split")
        .args(["-b", PART_SIZE, "-a", "3", "disk.qcow2", "disk.qcow2.part-"])
        .current_dir(&work)
        .status()?;
    if !st.success() {
        bail!("split failed");
    }
    std::fs::remove_file(&disk)?;
    std::fs::copy(image.vars(), work.join("vars.fd"))?;
    // Older images have no build date; stamp one so the pinned tag and the info agree.
    if info.built.is_empty() {
        info.built = crate::instance::local_date();
        image::write_info(image, &info)?;
    }
    std::fs::write(work.join("config.json"), serde_json::to_vec(&info)?)?;

    let mut parts: Vec<String> = std::fs::read_dir(&work)?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("disk.qcow2.part-"))
        .collect();
    parts.sort();
    // :<image> (newest build of that version), :<image>-<build date> (pinned), and bare
    // :<os> for the default version.
    let mut tags = vec![image.to_string(), format!("{image}-{}", info.built)];
    if image.version == os.default_version() {
        tags.push(os.to_string());
    }
    let target = reference(&tags.join(","));
    let host = target.split('/').next().unwrap_or_default().to_string();
    let mut cmd = Command::new(oras);
    cmd.current_dir(&work)
        .args(["push", &target, "--artifact-type", ARTIFACT_TYPE]);
    // GitHub shows a ghcr package on the repo page only when the manifest names its source.
    cmd.arg("--annotation")
        .arg("org.opencontainers.image.source=https://github.com/pawanpaudel93/agentpc");
    cmd.arg("--config")
        .arg(format!("config.json:{CONFIG_TYPE}"));
    for p in &parts {
        cmd.arg(format!("{p}:{PART_TYPE}"));
    }
    cmd.arg(format!("vars.fd:{VARS_TYPE}"));
    if plain_http(&host) {
        cmd.arg("--plain-http");
    }
    log!("pushing {} ({} parts)", reference(&tags[0]), parts.len());
    // oras skips blobs the registry already has, so a retry only re-sends what failed.
    let mut attempt = 1;
    while !cmd.status().context("run oras")?.success() {
        if attempt == PUSH_ATTEMPTS {
            bail!(
                "oras push failed {PUSH_ATTEMPTS} times; the parts are kept in {} \
                 (check the network, or log in with: oras login {host})",
                work.display()
            );
        }
        attempt += 1;
        log!("push failed; retrying ({attempt}/{PUSH_ATTEMPTS})");
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
    std::fs::remove_dir_all(&work)?;
    log!("pushed {} as :{}", info.version, tags.join(", :"));
    Ok(())
}

/// Held by a push of `image` (`clean` checks it before removing `cache/push-<image>`).
pub(crate) fn push_lock(image: &Image) -> PathBuf {
    cache_dir().join(format!(".push-{image}.lock"))
}

fn pull_dir(image: &Image) -> PathBuf {
    cache_dir().join(format!("pull-{image}"))
}

/// Download an image, then capture its RAM snapshot locally.
pub fn pull(image: &Image) -> Result<()> {
    let _lock = crate::instance::image_lock(image)?;
    pull_locked(image)
}

/// `pull`, for a caller already holding the image lock.
pub(crate) fn pull_locked(image: &Image) -> Result<()> {
    if image.os == Os::Windows {
        bail!(
            "Windows images aren't published (Microsoft license); run: {} image build {image}",
            crate::setup::cmd_name()
        );
    }
    if !image.instances()?.is_empty() {
        bail!("VMs of {image} depend on its current copy; rm them first");
    }
    let got = tokio::runtime::Runtime::new()?.block_on(download(image));
    if got.is_err() {
        // Drop the half-joined disk, but keep the verified parts: the next pull resumes
        // from them (`clean` removes them when no pull is running).
        let _ = std::fs::remove_file(image.disk().with_extension("qcow2.tmp"));
        if pull_dir(image).is_dir() {
            log!(
                "downloaded parts kept in {}; pull again to resume",
                pull_dir(image).display()
            );
        }
    }
    got?;
    image::snapshot_locked(image)
}

struct Ref {
    host: String,
    repo: String,
    tag: String,
}

fn parse(reference: &str) -> Result<Ref> {
    let (host, rest) = reference.split_once('/').context("bad image reference")?;
    let (repo, tag) = rest
        .rsplit_once(':')
        .context("image reference needs a tag")?;
    Ok(Ref {
        host: host.into(),
        repo: repo.into(),
        tag: tag.into(),
    })
}

async fn download(image: &Image) -> Result<()> {
    let at = reference(&image.to_string());
    let r = parse(&at)?;
    let scheme = if plain_http(&r.host) { "http" } else { "https" };
    let base = format!("{scheme}://{}/v2/{}", r.host, r.repo);
    let http = reqwest::Client::builder()
        .user_agent(concat!("agentpc/", env!("CARGO_PKG_VERSION")))
        // Cap connect and per-read stalls, but set no total timeout: image layers are large
        // and a slow-but-progressing download must not be killed mid-stream.
        .connect_timeout(std::time::Duration::from_secs(30))
        .read_timeout(std::time::Duration::from_secs(60))
        .build()?;
    let token = anonymous_token(&http, &base, &r.repo).await?;
    let auth = |req: reqwest::RequestBuilder| match &token {
        Some(t) => req.bearer_auth(t),
        None => req,
    };

    log!("fetching {at}");
    let manifest: Value = auth(http.get(format!("{base}/manifests/{}", r.tag)))
        .header("Accept", "application/vnd.oci.image.manifest.v1+json")
        .send()
        .await?
        .error_for_status()
        .with_context(|| format!("no image at {at}"))?
        .json()
        .await?;
    if manifest["artifactType"] != ARTIFACT_TYPE {
        bail!("{at} is not an agentpc image");
    }
    let config_digest = manifest["config"]["digest"]
        .as_str()
        .context("manifest has no config")?;
    let mut info: image::ImageInfo = auth(http.get(format!("{base}/blobs/{config_digest}")))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .context("image config is not agentpc image info")?;
    info.pulled_from = Some(at);
    log!("{} (built {} from {})", info.version, info.built, info.base);
    let layers = manifest["layers"]
        .as_array()
        .context("manifest has no layers")?;
    let total: u64 = layers.iter().filter_map(|l| l["size"].as_u64()).sum();

    let blobs: Vec<Blob> = layers.iter().map(Blob::of).collect::<Result<_>>()?;

    // Parts kept from an earlier, interrupted pull are reused when their content still
    // matches this manifest's digest; anything else there (another manifest's parts,
    // partial writes) goes.
    let work = pull_dir(image);
    std::fs::create_dir_all(&work)?;
    for e in std::fs::read_dir(&work)?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !blobs.iter().any(|b| b.title == name) {
            let _ = std::fs::remove_file(e.path());
        }
    }
    let mut have = 0;
    let mut todo = Vec::new();
    for b in blobs {
        let path = work.join(&b.title);
        if path.is_file() && file_sha256(&path).is_some_and(|h| h == b.sha256) {
            have += b.size;
        } else {
            let _ = std::fs::remove_file(&path);
            todo.push(b);
        }
    }
    // Peak use: the parts plus the disk joined from them, less the parts already here.
    crate::ops::ensure_free_space(
        (2 * total).saturating_sub(have).div_ceil(1 << 30) + 1,
        &format!("download {image}"),
    )?;
    let done = Arc::new(AtomicU64::new(have));
    if have > 0 {
        log!(
            "resuming: {:.2} of {:.2} GB already downloaded",
            have as f64 / 1e9,
            total as f64 / 1e9
        );
    } else {
        log!("downloading {:.2} GB", total as f64 / 1e9);
    }
    futures::stream::iter(todo.into_iter().map(|b| {
        let (http, base, work, done, token) = (
            http.clone(),
            base.clone(),
            work.clone(),
            done.clone(),
            token.clone(),
        );
        async move {
            fetch_blob(
                &http,
                &base,
                token.as_deref(),
                &b,
                &work.join(&b.title),
                &done,
                total,
            )
            .await
        }
    }))
    .buffer_unordered(PARALLEL_DOWNLOADS)
    .try_collect::<Vec<()>>()
    .await?;

    // Reassemble the disk from this manifest's parts, in order.
    let mut parts: Vec<PathBuf> = layers
        .iter()
        .filter_map(|l| l["annotations"]["org.opencontainers.image.title"].as_str())
        .filter(|t| t.starts_with("disk.qcow2.part-"))
        .map(|t| work.join(t))
        .collect();
    parts.sort();
    if parts.is_empty() || !work.join("vars.fd").is_file() {
        bail!("image is missing its disk or vars");
    }
    std::fs::create_dir_all(images_dir())?;
    let disk_tmp = image.disk().with_extension("qcow2.tmp");
    let mut out = tokio::fs::File::create(&disk_tmp).await?;
    for p in &parts {
        let mut f = tokio::fs::File::open(p).await?;
        tokio::io::copy(&mut f, &mut out).await?;
    }
    out.flush().await?;
    drop(out);

    // The old snapshot goes with the old disk: it would resume that one's RAM.
    image.remove_snapshot();
    replace_readonly(&disk_tmp, &image.disk())?;
    replace_readonly(&work.join("vars.fd"), &image.vars())?;
    image::write_info(image, &info)?;
    std::fs::remove_dir_all(&work)?;
    log!("{image} downloaded");
    Ok(())
}

/// Public ghcr.io packages still need an anonymous bearer token; local registries don't.
async fn anonymous_token(http: &reqwest::Client, base: &str, repo: &str) -> Result<Option<String>> {
    let probe = http
        .get(format!(
            "{}/",
            base.rsplit_once(&format!("/{repo}")).unwrap().0
        ))
        .send()
        .await?;
    if probe.status() != reqwest::StatusCode::UNAUTHORIZED {
        return Ok(None);
    }
    let challenge = probe
        .headers()
        .get("www-authenticate")
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default();
    let field = |k: &str| {
        challenge
            .split([',', ' '])
            .find_map(|kv| kv.strip_prefix(&format!("{k}=")))
            .map(|v| v.trim_matches('"').to_string())
    };
    let realm = field("realm").context("registry wants auth but sent no realm")?;
    let service = field("service").unwrap_or_default();
    let v: Value = http
        .get(realm)
        .query(&[
            ("service", service.as_str()),
            ("scope", &format!("repository:{repo}:pull")),
        ])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(v["token"]
        .as_str()
        .or(v["access_token"].as_str())
        .map(String::from))
}

/// One layer of an image manifest: a disk part or the vars.
struct Blob {
    title: String,
    digest: String,
    /// Hex sha256 the content must have (the digest without its `sha256:` prefix).
    sha256: String,
    size: u64,
}

impl Blob {
    fn of(l: &Value) -> Result<Self> {
        let title = l["annotations"]["org.opencontainers.image.title"]
            .as_str()
            .context("layer without title")?;
        if title.is_empty() || title.contains('/') || title.starts_with('.') {
            bail!("bad layer title {title}");
        }
        let digest = l["digest"].as_str().context("layer without digest")?;
        let sha256 = digest
            .strip_prefix("sha256:")
            .context("only sha256 digests are supported")?
            .to_ascii_lowercase();
        Ok(Self {
            title: title.into(),
            digest: digest.into(),
            sha256,
            size: l["size"].as_u64().unwrap_or(0),
        })
    }
}

/// Hex sha256 of a file's content, or `None` if it can't be read.
fn file_sha256(p: &Path) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(p).ok()?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        match f.read(&mut buf).ok()? {
            0 => break,
            n => hasher.update(&buf[..n]),
        }
    }
    Some(
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}

/// Download a blob to `dest` through `dest.part`, so a file at `dest` is always whole.
async fn fetch_blob(
    http: &reqwest::Client,
    base: &str,
    token: Option<&str>,
    blob: &Blob,
    dest: &Path,
    done: &AtomicU64,
    total: u64,
) -> Result<()> {
    let want = &blob.sha256;
    let mut req = http.get(format!("{base}/blobs/{}", blob.digest));
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await?.error_for_status()?;
    let mut stream = resp.bytes_stream();
    let part = dest.with_file_name(format!("{}.part", blob.title));
    let mut file = tokio::fs::File::create(&part).await?;
    let mut hasher = Sha256::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
        let before = done.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        let after = before + chunk.len() as u64;
        // A progress line every ~5% of the whole download.
        if total > 0 && (after * 20 / total) > (before * 20 / total) {
            log!("  {:>3}%", after * 100 / total);
        }
    }
    file.flush().await?;
    let got: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    drop(file);
    if &got != want {
        let _ = tokio::fs::remove_file(&part).await;
        bail!("checksum mismatch for {}", dest.display());
    }
    tokio::fs::rename(&part, dest).await?;
    Ok(())
}

fn replace_readonly(from: &Path, to: &Path) -> Result<()> {
    let _ = std::fs::remove_file(to);
    std::fs::rename(from, to)?;
    let mut perm = std::fs::metadata(to)?.permissions();
    perm.set_readonly(true);
    std::fs::set_permissions(to, perm)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_manifest_layers() {
        let l = serde_json::json!({
            "digest": "sha256:ABCDEF",
            "size": 42,
            "annotations": {"org.opencontainers.image.title": "disk.qcow2.part-aaa"}
        });
        let b = Blob::of(&l).unwrap();
        assert_eq!(b.title, "disk.qcow2.part-aaa");
        assert_eq!(b.digest, "sha256:ABCDEF");
        assert_eq!(b.sha256, "abcdef");
        assert_eq!(b.size, 42);
        for title in ["../x", "", ".hidden"] {
            let l = serde_json::json!({
                "digest": "sha256:ab",
                "annotations": {"org.opencontainers.image.title": title}
            });
            assert!(Blob::of(&l).is_err(), "{title}");
        }
        let md5 = serde_json::json!({
            "digest": "md5:ab",
            "annotations": {"org.opencontainers.image.title": "vars.fd"}
        });
        assert!(Blob::of(&md5).is_err());
    }

    #[test]
    fn hashes_files_for_resume() {
        let p = std::env::temp_dir().join(format!("agentpc-sha-test-{}", std::process::id()));
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(
            file_sha256(&p).as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        std::fs::remove_file(&p).unwrap();
        assert_eq!(file_sha256(&p), None);
    }
}

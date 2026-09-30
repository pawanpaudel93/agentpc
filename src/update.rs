//! `agentpc update`: replace this binary with the latest GitHub release.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::log;

const REPO: &str = "pawanpaudel93/agentpc";
const TARGET: &str = "aarch64-apple-darwin";
/// The installer the README and `setup::QEMU_HINT` point to.
const INSTALL_URL: &str = "https://agentpc.pawanpaudel.com.np/install.sh";

pub fn update(check: bool) -> Result<()> {
    tokio::runtime::Runtime::new()?.block_on(run(check))
}

async fn run(check: bool) -> Result<()> {
    let http = reqwest::Client::builder()
        .user_agent(concat!("agentpc/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(30))
        .read_timeout(std::time::Duration::from_secs(60))
        .build()?;
    let current = env!("CARGO_PKG_VERSION");
    let latest = latest_version(&http).await?;
    if !is_newer(&latest, current) {
        log!("agentpc {current} is the latest release");
        return Ok(());
    }
    if check {
        log!(
            "agentpc {latest} is available (you have {current}); run: {} update",
            crate::setup::cmd_name()
        );
        return Ok(());
    }

    let exe = std::env::current_exe()?
        .canonicalize()
        .context("find this binary")?;
    let dir = exe.parent().context("binary has no parent directory")?;
    // The Claude Desktop bundle ships the binary as server/agentpc next to manifest.json;
    // Claude Desktop owns that copy.
    if dir.ends_with("server") && dir.join("../manifest.json").is_file() {
        bail!(
            "this agentpc came with the Claude Desktop bundle; update it there \
             (download agentpc-{latest}.mcpb from https://github.com/{REPO}/releases/latest)"
        );
    }

    let name = format!("agentpc-{latest}-{TARGET}");
    let base = format!("https://github.com/{REPO}/releases/download/v{latest}");
    log!("downloading agentpc {latest}");
    let tarball = fetch(&http, &format!("{base}/{name}.tar.gz")).await?;
    let sums = String::from_utf8(fetch(&http, &format!("{base}/{name}.tar.gz.sha256")).await?)?;
    let want = sums.split_whitespace().next().unwrap_or_default();
    let got = sha256_hex(&tarball);
    if !want.eq_ignore_ascii_case(&got) {
        bail!("checksum mismatch for {name}.tar.gz (expected {want}, got {got})");
    }

    // Unpack next to the binary so the final rename stays on one filesystem (atomic).
    let tmp = dir.join(format!(".agentpc-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir(&tmp).with_context(|| not_writable_hint(dir, &exe))?;
    let result = install(&tarball, &tmp, &name, &latest, &exe);
    let _ = std::fs::remove_dir_all(&tmp);
    result?;
    log!("updated agentpc {current} -> {latest}; restart agent sessions to pick it up");
    Ok(())
}

fn install(tarball: &[u8], tmp: &Path, name: &str, version: &str, exe: &Path) -> Result<()> {
    let archive = tmp.join("agentpc.tar.gz");
    std::fs::write(&archive, tarball)?;
    let st = Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(tmp)
        .status()
        .context("run tar")?;
    if !st.success() {
        bail!("couldn't unpack {name}.tar.gz");
    }
    let new = tmp.join(name).join("agentpc");
    let out = Command::new(&new)
        .arg("--version")
        .output()
        .context("run the downloaded binary")?;
    let reported = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if reported != format!("agentpc {version}") {
        bail!("the downloaded binary reports '{reported}', expected 'agentpc {version}'");
    }
    std::fs::rename(&new, exe).with_context(|| format!("replace {}", exe.display()))
}

/// What to do when the binary's directory isn't writable (e.g. root-owned /usr/local/bin).
/// The update itself works as root (it only writes next to the binary), which keeps it
/// where it is; install.sh can instead reinstall into that same directory, but running
/// the whole installer as root would also register the MCP server for root.
fn not_writable_hint(dir: &Path, exe: &Path) -> String {
    format!(
        "can't write to {dir}; update as its owner: sudo {exe} update\n\
         or reinstall there (as a user who can write to it): \
         curl -fsSL {INSTALL_URL} | AGENTPC_INSTALL_DIR={dir} sh",
        dir = dir.display(),
        exe = exe.display(),
    )
}

/// The latest release's version, from where GitHub's releases/latest page redirects
/// (no API call, so no rate limit).
async fn latest_version(http: &reqwest::Client) -> Result<String> {
    let resp = http
        .head(format!("https://github.com/{REPO}/releases/latest"))
        .send()
        .await
        .context("check the latest release")?;
    let url = resp.url().as_str().to_string();
    match url.rsplit_once("/tag/v") {
        Some((_, v)) if !v.is_empty() => Ok(v.to_string()),
        _ => bail!("no published agentpc release found (got {url})"),
    }
}

async fn fetch(http: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let resp = http.get(url).send().await?.error_for_status()?;
    Ok(resp.bytes().await?.to_vec())
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Whether `latest` is a higher X.Y.Z than `current` (pre-release suffixes ignored).
fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.split(['-', '+'])
            .next()
            .unwrap_or_default()
            .split('.')
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    parse(latest) > parse(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_writable_hint_names_the_same_dir() {
        let h = not_writable_hint(
            Path::new("/usr/local/bin"),
            Path::new("/usr/local/bin/agentpc"),
        );
        assert!(h.contains("sudo /usr/local/bin/agentpc update"));
        assert!(h.contains(&format!(
            "curl -fsSL {INSTALL_URL} | AGENTPC_INSTALL_DIR=/usr/local/bin sh"
        )));
    }

    #[test]
    fn compares_versions_numerically() {
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("1.0.0", "0.99.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.1"));
        assert!(!is_newer("0.1.0-rc1", "0.1.0"));
    }
}

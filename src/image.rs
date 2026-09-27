//! Images: install an OS once into a build VM (`_build-<image>`, slot 0), freeze its disk
//! as a read-only image that `create` clones, and capture its RAM snapshot.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::instance::{Image, Instance, Os, cache_dir, home, images_dir, instances_dir, ssh_key};
use crate::ops::{run, ssh, wait_ready};
use crate::{log, qemu};

// Guest assets ship inside the binary: an installed agentpc has no repo next to it.
const UBUNTU_USER_DATA: &str = include_str!("../guests/ubuntu/user-data");

/// A Windows ARM64 ISO with the checksum and size dockur/windows-arm records for it. The
/// checksum is what makes a third-party mirror safe to use: a tampered file is rejected.
pub(crate) struct WinIso {
    pub version: &'static str,
    pub what: &'static str,
    /// Tried in order; the same file on each.
    urls: &'static [&'static str],
    sha256: &'static str,
    pub size: u64,
}

/// The Windows versions `image build` can download (en-us; others need --iso). Microsoft
/// only serves its current ARM64 ISOs, so older releases come from archive mirrors. Left out:
/// Microsoft's evaluation ISOs (they install already expired and shut down every hour),
/// Windows 10 (its ARM64 build hangs at boot on Apple Silicon) and LTSC (the answer file's
/// generic Pro key doesn't install it).
pub(crate) const WINDOWS_ISOS: [WinIso; 3] = [
    WinIso {
        version: "11-25h2",
        what: "Windows 11 25H2 (Home/Pro)",
        urls: &[
            "https://software-static.download.prss.microsoft.com/dbazure/888969d5-f34g-4e03-ac9d-1f9786c66749/26200.6584.250915-1905.25h2_ge_release_svc_refresh_CLIENT_CONSUMER_a64fre_en-us.iso",
        ],
        sha256: "32cde0071ed8086b29bb6c8c3bf17ba9e3cdf43200537434a811a9b6cc2711a1",
        size: 7_299_147_776,
    },
    WinIso {
        version: "11-24h2",
        what: "Windows 11 24H2 (Home/Pro)",
        urls: &[
            "https://archive.org/download/Windows11_24H2_Arm64_ISO/Win11_24H2_English_Arm64.iso",
        ],
        sha256: "57d1dfb2c6690a99fe99226540333c6c97d3fd2b557a50dfe3d68c3f675ef2b0",
        size: 5_460_387_840,
    },
    WinIso {
        version: "11-23h2",
        what: "Windows 11 23H2 (Home/Pro)",
        urls: &["https://dl.bobpony.com/windows/11/en-us_windows_11_23h2_arm64.iso"],
        sha256: "bde2bcefe470bd19eb6cb810f38478dbd6809f04bac20c26ff27d4c9b864f662",
        size: 6_755_211_264,
    },
];

const UBUNTU_PREPARE: &str = include_str!("../guests/ubuntu/prepare.sh");
const WIN_PREPARE: &str = include_str!("../guests/windows/prepare.ps1");
const WIN_AUTOUNATTEND: &str = include_str!("../guests/windows/Autounattend.xml");
const WIN_SETUP_COMPLETE: &[u8] = include_bytes!("../guests/windows/SetupComplete.cmd");
const WIN_OEM: [(&str, &[u8]); 2] = [
    (
        "install.bat",
        include_bytes!("../guests/windows/oem/install.bat"),
    ),
    (
        "setup.ps1",
        include_bytes!("../guests/windows/oem/setup.ps1"),
    ),
];

pub fn build(image: &Image, iso: Option<PathBuf>) -> Result<()> {
    let _lock = crate::instance::image_lock()?;
    build_locked(image, iso)
}

/// `build`, for a caller already holding the image lock.
/// Delete this image's half-written temp files (`*.qcow2.tmp`, `*.state.tmp`) left by an
/// aborted build or snapshot. Best-effort: it runs on the failure path.
fn remove_image_tmp(image: &Image) {
    let prefix = format!("{image}.");
    for e in std::fs::read_dir(images_dir())
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix) && name.ends_with(".tmp") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Owns a hidden build/snapshot VM for the duration of the work. If it's dropped before the
/// artifacts are safely in place — an early `?` or a panic — it kills the VM, deletes its
/// directory, and clears the image's temp partials, so a failed build never leaves a VM
/// running or half-written files behind. Disarm with `keep()` on success. (A hard Ctrl-C
/// still can't run Drop; `agentpc clean` sweeps whatever an interrupt leaves.)
struct BuildGuard {
    inst: Instance,
    armed: bool,
}

impl BuildGuard {
    fn new(inst: Instance) -> Self {
        Self { inst, armed: true }
    }
    fn keep(mut self) {
        self.armed = false;
    }
}

impl Drop for BuildGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        qemu::quit(&self.inst);
        let _ = std::fs::remove_dir_all(&self.inst.dir);
        remove_image_tmp(&self.inst.image);
    }
}

pub(crate) fn build_locked(image: &Image, iso: Option<PathBuf>) -> Result<()> {
    let os = image.os;
    if !image.instances()?.is_empty() {
        bail!("VMs of {image} depend on it; rm them first");
    }
    for d in [home(), cache_dir(), images_dir(), instances_dir()] {
        std::fs::create_dir_all(&d).with_context(|| format!("create {}", d.display()))?;
    }
    ensure_ssh_key()?;

    let name = format!("_build-{image}");
    if instances_dir().join(&name).is_dir() {
        if let Ok(old) = Instance::load(&name) {
            qemu::stop(&old)?;
        }
        std::fs::remove_dir_all(instances_dir().join(&name))?;
    }
    let iso_path = match os {
        Os::Windows => {
            let iso = windows_iso(&image.version, iso)?;
            log!("installing from {}", iso.display());
            Some(iso)
        }
        Os::Ubuntu => None,
    };
    let guard = BuildGuard::new(Instance::create(&name, image, 0)?);
    match &iso_path {
        Some(iso) => build_windows(&guard.inst, iso)?,
        None => build_ubuntu(&guard.inst)?,
    }
    // Bake the defaults into the image too, so the prepare step at snapshot time (after a
    // pull, say) finds them in place instead of redoing slow work like installing Chrome.
    prepare_guest(&guard.inst)?;
    let mut info = guest_info(&guard.inst)?;
    if let (Os::Windows, Some(p)) = (os, &iso_path) {
        record_iso(&mut info, p)?;
    }
    info.built = crate::instance::local_date();
    promote_image(&guard.inst)?;
    // The image disk is written and the build VM removed; snapshot_locked guards its own VM.
    guard.keep();
    write_info(image, &info)?;
    snapshot_locked(image)
}

/// Capture the image's snapshot: boot the image once, let the desktop
/// settle, then save RAM and flatten the disk as it was at that instant. Clones of it
/// resume in seconds instead of booting.
pub fn snapshot(image: &Image) -> Result<()> {
    let _lock = crate::instance::image_lock()?;
    snapshot_locked(image)
}

/// `snapshot`, for a caller already holding the image lock.
pub(crate) fn snapshot_locked(image: &Image) -> Result<()> {
    let os = image.os;
    if !image.exists() {
        bail!(
            "no {image} image; run: {} image build {image}",
            crate::setup::cmd_name()
        );
    }
    if image.instances()?.iter().any(|i| i.on_snapshot_base()) {
        bail!("VMs of {image} depend on its snapshot; rm them first");
    }
    let name = format!("_snap-{image}");
    if instances_dir().join(&name).is_dir() {
        if let Ok(old) = Instance::load(&name) {
            qemu::quit(&old);
        }
        std::fs::remove_dir_all(instances_dir().join(&name))?;
    }
    let guard = BuildGuard::new(Instance::create(&name, image, 0)?);
    let inst = &guard.inst;
    run(
        "qemu-img",
        &[
            "create",
            "-q",
            "-f",
            "qcow2",
            "-b",
            &format!("../../images/{image}.qcow2"),
            "-F",
            "qcow2",
            &inst.disk().to_string_lossy(),
        ],
    )?;
    std::fs::copy(image.vars(), inst.vars())?;
    crate::ops::set_writable(&inst.vars())?;

    log!("booting {image} to capture its snapshot");
    qemu::start(inst, &[])?;
    let took = wait_ready(inst, Duration::from_secs(os.boot_timeout()))?;
    prepare_guest(inst)?;
    // Let post-logon startup finish so clones don't all redo it after resuming.
    let settle = match os {
        Os::Windows => 45,
        Os::Ubuntu => 10,
    };
    log!("{os} ready in {}s; settling {settle}s", took.as_secs());
    // Guest-reported fields refresh; build-time ones (base, built, ISO checksum) are kept.
    let fresh = guest_info(inst)?;
    let mut info = read_info(image).unwrap_or_default();
    if info.base.is_empty() {
        info.base = fresh.base;
    }
    info.os = fresh.os;
    info.version = fresh.version;
    info.version_id = fresh.version_id;
    info.arch = fresh.arch;
    info.agentpc = fresh.agentpc;
    info.desktop_server = fresh.desktop_server;
    write_info(image, &info)?;
    std::thread::sleep(Duration::from_secs(settle));

    let (disk, vars, state) = (
        image.snapshot_disk(),
        image.snapshot_vars(),
        image.snapshot_state(),
    );
    for p in [&disk, &vars, &state] {
        let _ = std::fs::remove_file(p);
    }
    let state_tmp = state.with_extension("state.tmp");
    qemu::save_state(inst, &state_tmp)?;
    let disk_tmp = disk.with_extension("qcow2.tmp");
    run(
        "qemu-img",
        &[
            "convert",
            "-O",
            "qcow2",
            &inst.disk().to_string_lossy(),
            &disk_tmp.to_string_lossy(),
        ],
    )?;
    std::fs::rename(&disk_tmp, &disk)?;
    std::fs::copy(inst.vars(), &vars)?;
    // Move the machine-type sidecar save_state wrote next to the temp state alongside it.
    let sidecar = |p: &std::path::Path| {
        let mut s = p.as_os_str().to_owned();
        s.push(".machine");
        PathBuf::from(s)
    };
    let _ = std::fs::rename(sidecar(&state_tmp), sidecar(&state));
    std::fs::rename(&state_tmp, &state)?;
    for p in [&disk, &vars, &state] {
        let mut perm = std::fs::metadata(p)?.permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(p, perm)?;
    }
    std::fs::remove_dir_all(&inst.dir)?;
    // Snapshot artifacts are in place and the VM is gone; nothing left to clean up.
    guard.keep();
    let gb = |p: &PathBuf| {
        std::fs::metadata(p)
            .map(|m| m.len() as f64 / 1e9)
            .unwrap_or(0.0)
    };
    log!(
        "snapshot for {image} ready (disk {:.1} GB, memory {:.1} GB)",
        gb(&disk),
        gb(&state)
    );
    Ok(())
}

/// Apply the agent-friendly defaults in `guests/<os>/prepare.*` to a running guest.
/// It runs at every snapshot, so existing and pulled images get it too.
fn prepare_guest(inst: &Instance) -> Result<()> {
    log!("applying agent defaults to {}", inst.os);
    let (script, name, run) = match inst.os {
        Os::Ubuntu => (
            UBUNTU_PREPARE,
            "/tmp/agentpc-prepare.sh",
            "sudo sh /tmp/agentpc-prepare.sh && rm -f /tmp/agentpc-prepare.sh",
        ),
        Os::Windows => (
            WIN_PREPARE,
            "agentpc-prepare.ps1",
            "powershell -NoProfile -ExecutionPolicy Bypass -File \"$env:USERPROFILE\\agentpc-prepare.ps1\"; \
             Remove-Item \"$env:USERPROFILE\\agentpc-prepare.ps1\"",
        ),
    };
    let local = inst.dir.join("prepare-script");
    std::fs::write(&local, script)?;
    crate::ops::upload(inst, &local, name)?;
    let out = ssh(inst, run)?;
    if !out.status.success() {
        bail!(
            "preparing {} failed: {}",
            inst.os,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// What an image contains. Kept next to it as `<image>.json` and published as its OCI config.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImageInfo {
    pub os: String,
    /// As the guest reports it, e.g. "Ubuntu 24.04.5 LTS".
    pub version: String,
    /// Short form used as a registry tag, e.g. "24.04" or "11-24H2".
    pub version_id: String,
    #[serde(default)]
    pub arch: String,
    /// What it was built from: the Ubuntu cloud-image serial or the Windows ISO.
    #[serde(default)]
    pub base: String,
    /// Build date, YYYYMMDD.
    #[serde(default)]
    pub built: String,
    #[serde(default)]
    pub agentpc: String,
    /// The desktop-control server agents drive, e.g. "cua-driver 0.30.1".
    #[serde(default)]
    pub desktop_server: String,
    /// Checksum of the Windows ISO it was built from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iso_sha256: Option<String>,
    /// Registry reference, when the image was pulled rather than built here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pulled_from: Option<String>,
}

pub fn read_info(image: &Image) -> Option<ImageInfo> {
    serde_json::from_slice(&std::fs::read(image.info_file()).ok()?).ok()
}

pub fn write_info(image: &Image, info: &ImageInfo) -> Result<()> {
    std::fs::write(image.info_file(), serde_json::to_vec_pretty(info)?)?;
    Ok(())
}

/// Ask a running guest what it is, as `key=value` lines.
fn guest_info(inst: &Instance) -> Result<ImageInfo> {
    let script = match inst.os {
        Os::Ubuntu => {
            r#". /etc/os-release
echo "version=$PRETTY_NAME"
echo "version_id=$VERSION_ID"
echo "arch=$(dpkg --print-architecture)"
echo "serial=$(sed -n 's/^serial: *//p' /etc/cloud/build.info 2>/dev/null)"
echo "server=$(~/.local/bin/cua-driver --version 2>/dev/null | awk '{print $NF}')""#
        }
        // ProductName still says "Windows 10" on Windows 11; the WMI caption doesn't.
        Os::Windows => {
            r#"$v = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion'
$cua = "$env:LOCALAPPDATA\Programs\Cua\cua-driver\bin\cua-driver.exe"
$mcp = if (Test-Path C:\uv\uv.exe) { & C:\uv\uv.exe tool list 2>$null | Select-String '^windows-mcp v' }
"caption=$((Get-CimInstance Win32_OperatingSystem).Caption -replace '^Microsoft ', '')"
"release=$($v.DisplayVersion)"
"build=$($v.CurrentBuild).$($v.UBR)"
"arch=$($env:PROCESSOR_ARCHITECTURE.ToLower())"
"server=$(if (Test-Path $cua) { & $cua --version } elseif ($mcp) { $mcp.Line -replace '^windows-mcp v', 'Windows-MCP ' })""#
        }
    };
    let out = ssh(inst, script)?;
    if !out.status.success() {
        bail!(
            "reading the {} version failed: {}",
            inst.os,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let kv: std::collections::HashMap<&str, &str> = text
        .lines()
        .filter_map(|l| l.trim().split_once('='))
        .map(|(k, v)| (k, v.trim()))
        .collect();
    let get = |k: &str| kv.get(k).copied().unwrap_or_default().to_string();
    let server = get("server");
    let (version, version_id, base, desktop_server) = match inst.os {
        Os::Ubuntu => (
            get("version"),
            get("version_id"),
            format!(
                "Ubuntu {} cloud image, serial {}",
                get("version_id"),
                get("serial")
            ),
            format!("cua-driver {server}"),
        ),
        Os::Windows => {
            let caption = get("caption");
            let major = if caption.contains("Windows 11") {
                "11"
            } else {
                "10"
            };
            (
                format!("{caption} {} (build {})", get("release"), get("build")),
                format!("{major}-{}", get("release")),
                String::new(), // described from the ISO by build
                server,
            )
        }
    };
    Ok(ImageInfo {
        os: inst.os.to_string(),
        version,
        version_id,
        arch: get("arch"),
        base,
        built: String::new(),
        agentpc: env!("CARGO_PKG_VERSION").into(),
        desktop_server,
        iso_sha256: None,
        pulled_from: None,
    })
}

/// Note which ISO a Windows image was built from: a readable description plus its checksum.
pub fn record_iso(info: &mut ImageInfo, iso: &Path) -> Result<()> {
    let name = iso
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    log!("checksumming {name}");
    let sha = sha256_file(iso)?;
    info.base = match WINDOWS_ISOS.iter().find(|w| w.sha256 == sha) {
        Some(w) => format!("{} ISO, ARM64, en-us", w.what),
        None => describe_iso(&name),
    };
    info.iso_sha256 = Some(sha);
    Ok(())
}

/// Turn Microsoft's ISO file name into words, e.g.
/// `26100.4349.250607-1500.ge_release_svc_refresh_CLIENTCONSUMER_RET_A64FRE_en-us.iso` →
/// "Windows 11 24H2 ISO, ARM64, consumer editions, build 26100.4349, en-us".
fn describe_iso(name: &str) -> String {
    let stem = name.trim_end_matches(".iso");
    let parts: Vec<&str> = stem.split('_').collect();
    let mut nums = parts.first().unwrap_or(&"").split('.');
    let (Some(build), Some(ubr)) = (nums.next(), nums.next()) else {
        return format!("Windows ISO {name}");
    };
    let Ok(build_no) = build.parse::<u32>() else {
        return format!("Windows ISO {name}");
    };
    let release = release_of_build(build_no)
        .map(|r| format!(" {}", r.to_ascii_uppercase()))
        .unwrap_or_default();
    let major = if build_no >= 22000 { "11" } else { "10" };
    let upper = stem.to_ascii_uppercase();
    let arch = if upper.contains("A64FRE") {
        "ARM64"
    } else if upper.contains("X64FRE") {
        "x64"
    } else {
        "unknown arch"
    };
    // "CLIENTCONSUMER" in older names, "CLIENT_CONSUMER" in newer ones.
    let editions = parts
        .iter()
        .position(|p| p.to_ascii_uppercase().starts_with("CLIENT"))
        .and_then(|i| match &parts[i][6..] {
            "" => parts.get(i + 1).copied(),
            rest => Some(rest),
        })
        .map(|e| format!(", {} editions", e.to_lowercase()))
        .unwrap_or_default();
    let lang = parts
        .last()
        .filter(|l| l.contains('-'))
        .map(|l| format!(", {l}"))
        .unwrap_or_default();
    format!("Windows {major}{release} ISO, {arch}{editions}, build {build}.{ubr}{lang}")
}

fn release_of_build(build: u32) -> Option<&'static str> {
    match build {
        26200..=26299 => Some("25h2"),
        26100..=26199 => Some("24h2"),
        22631..=22999 => Some("23h2"),
        22621..=22630 => Some("22h2"),
        22000..=22620 => Some("21h2"),
        _ => None,
    }
}

/// The Windows 11 release an ISO holds, as an image version ("11-24h2"), read from its
/// file name: Microsoft's names lead with the build number, mirrors' spell out "24H2".
fn iso_release(name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    let build = lower.split('.').next().and_then(|b| b.parse::<u32>().ok());
    let release = match build {
        Some(b) => release_of_build(b)?.to_string(),
        None => lower
            .split(|c: char| !c.is_ascii_alphanumeric())
            .find(|t| t.len() == 4 && t.as_bytes()[2] == b'h' && t[..2].parse::<u8>().is_ok())?
            .to_string(),
    };
    Some(format!("11-{release}"))
}

/// SHA-256 of a file via `shasum`, for recording which ISO an image was built from.
fn sha256_file(p: &Path) -> Result<String> {
    let out = Command::new("shasum").args(["-a", "256"]).arg(p).output()?;
    if !out.status.success() {
        bail!("shasum failed for {}", p.display());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string())
}

/// One line per local image: version, size and whether VMs can resume from its snapshot.
pub fn list() -> Result<String> {
    let mut s = String::new();
    for image in Image::all() {
        let size = std::fs::metadata(image.disk())?.len() as f64 / 1e9;
        let snap = if image.has_snapshot() {
            "snapshot: yes"
        } else {
            "snapshot: no"
        };
        let version = read_info(&image).map_or("version unknown".into(), |i| i.version);
        s += &format!("{image:<22} {version:<42} {size:>5.1} GB  {snap}\n");
    }
    if s.is_empty() {
        let cmd = crate::setup::cmd_name();
        s = format!("no images ({cmd} image pull ubuntu, or {cmd} image build <os>)");
    }
    Ok(s)
}

pub fn describe(image: &Image) -> Result<String> {
    if !image.exists() {
        bail!("no {image} image");
    }
    let info = read_info(image).with_context(|| {
        format!(
            "no version info for {image}; run: {} image snapshot {image}",
            crate::setup::cmd_name()
        )
    })?;
    Ok(serde_json::to_string_pretty(&info)?)
}

pub fn remove(image: &Image) -> Result<String> {
    if !image.instances()?.is_empty() {
        bail!("VMs of {image} depend on it; rm them first");
    }
    let files = [
        image.info_file(),
        image.disk(),
        image.vars(),
        image.snapshot_disk(),
        image.snapshot_vars(),
        image.snapshot_state(),
    ];
    if !files.iter().any(|p| p.exists()) {
        bail!("no {image} image");
    }
    for p in files {
        let _ = std::fs::remove_file(p);
    }
    Ok(format!("removed {image}"))
}

pub(crate) fn ensure_ssh_key() -> Result<()> {
    let key = ssh_key();
    if key.exists() {
        return Ok(());
    }
    run(
        "ssh-keygen",
        &[
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            "agentpc",
            "-f",
            &key.to_string_lossy(),
        ],
    )
}

pub(crate) fn public_key() -> PathBuf {
    ssh_key().with_extension("pub")
}

fn create_vars(inst: &Instance) -> Result<()> {
    std::fs::File::create(inst.vars())?.set_len(64 << 20)?;
    Ok(())
}

/// A Windows ISO already on this Mac for `version`: `--iso`, `$WIN_ISO`, an earlier download,
/// or a Home/Pro ISO of that release in `~/Downloads`. An explicitly named path is returned
/// even if it doesn't exist.
pub(crate) fn find_windows_iso(version: &str, iso: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(p) = iso.or_else(|| std::env::var_os("WIN_ISO").map(PathBuf::from)) {
        return Some(p);
    }
    if let Some(p) = windows_iso_entry(version)
        .map(downloaded_iso_path)
        .filter(|p| p.is_file())
    {
        return Some(p);
    }
    let downloads = PathBuf::from(std::env::var_os("HOME")?).join("Downloads");
    let mut isos: Vec<PathBuf> = std::fs::read_dir(downloads)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                let l = n.to_ascii_lowercase();
                // The answer file installs Home/Pro; other editions don't finish.
                l.ends_with(".iso")
                    && (l.contains("a64fre") || l.contains("arm64"))
                    && !["eval", "ltsc", "enterprise", "iot", "server"]
                        .iter()
                        .any(|e| l.contains(e))
                    && iso_release(n).as_deref() == Some(version)
            })
        })
        .collect();
    isos.sort();
    isos.into_iter().next()
}

pub(crate) fn windows_iso_entry(version: &str) -> Option<&'static WinIso> {
    WINDOWS_ISOS.iter().find(|w| w.version == version)
}

fn downloaded_iso_path(w: &WinIso) -> PathBuf {
    cache_dir().join(w.urls[0].rsplit('/').next().unwrap_or(w.version))
}

/// The ISO to install from, downloading Microsoft's official one if none is on this Mac.
fn windows_iso(version: &str, iso: Option<PathBuf>) -> Result<PathBuf> {
    match find_windows_iso(version, iso) {
        Some(p) if p.is_file() => {
            // Keep release names honest: windows-11-25h2 must not hold 24H2.
            let found = p
                .file_name()
                .and_then(|n| iso_release(&n.to_string_lossy()));
            if windows_iso_entry(version).is_some()
                && let Some(found) = found.filter(|f| f != version)
            {
                bail!(
                    "{} is a Windows {} ISO, not windows-{version}; build it as windows-{found}",
                    p.display(),
                    found.replace('-', " ").to_ascii_uppercase()
                );
            }
            Ok(p)
        }
        Some(p) => bail!("Windows ISO {} not found", p.display()),
        None => match windows_iso_entry(version) {
            Some(w) => download_windows_iso(w),
            None => bail!(
                "no download for windows-{version}; pass --iso <ARM64 ISO>, or use one of: {}",
                WINDOWS_ISOS
                    .iter()
                    .map(|w| format!("windows-{}", w.version))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        },
    }
}

fn download_windows_iso(w: &WinIso) -> Result<PathBuf> {
    let dest = downloaded_iso_path(w);
    let part = dest.with_extension("iso.part");
    std::fs::create_dir_all(cache_dir())?;
    // A partial file larger than the ISO can't be resumed; start over.
    if std::fs::metadata(&part).is_ok_and(|m| m.len() > w.size) {
        std::fs::remove_file(&part)?;
    }
    let curl = |url: &str| {
        Command::new("curl")
            // Fail fast on a dead connection or a download that stalls under 1 KB/s for 2 min,
            // rather than hanging an image build forever.
            .args([
                "-fL",
                "--retry",
                "3",
                "-C",
                "-",
                "--connect-timeout",
                "30",
                "--speed-limit",
                "1024",
                "--speed-time",
                "120",
                "-#",
                "-o",
            ])
            .arg(&part)
            .arg(url)
            .status()
            .map(|s| s.code())
            .context("run curl")
    };
    let mut done = false;
    for url in w.urls {
        let host = url.split('/').nth(2).unwrap_or(url);
        log!(
            "downloading {} from {host} ({:.1} GB, resumable)",
            w.what,
            w.size as f64 / 1e9
        );
        let mut code = curl(url)?;
        // 33: the mirror ignores range requests, so the partial file can't be resumed.
        if code == Some(33) {
            let _ = std::fs::remove_file(&part);
            code = curl(url)?;
        }
        if code == Some(0) {
            done = true;
            break;
        }
        log!("download from {host} failed");
    }
    if !done {
        bail!("downloading the Windows ISO failed; pass --iso <Windows ARM64 ISO> instead");
    }
    let size = std::fs::metadata(&part)?.len();
    if size != w.size {
        bail!(
            "Windows ISO download is {size} bytes, expected {}; rerun to resume",
            w.size
        );
    }
    log!("verifying the Windows ISO checksum");
    if sha256_file(&part)? != w.sha256 {
        let _ = std::fs::remove_file(&part);
        bail!("Windows ISO checksum mismatch; the partial file was removed, rerun to retry");
    }
    std::fs::rename(&part, &dest)?;
    Ok(dest)
}

fn build_windows(inst: &Instance, iso: &Path) -> Result<()> {
    let iso = std::fs::canonicalize(iso)?;
    let setup = inst.dir.join("setup.img");
    windows_setup_img(&setup)?;
    run(
        "qemu-img",
        &[
            "create",
            "-q",
            "-f",
            "qcow2",
            &inst.disk().to_string_lossy(),
            "64G",
        ],
    )?;
    create_vars(inst)?;
    let extra = vec![
        "-drive".to_string(),
        format!("file={},id=setup,format=raw,if=none", setup.display()),
        "-device".into(),
        "usb-storage,drive=setup,removable=on".into(),
        "-drive".into(),
        format!(
            "file={},id=boot,format=raw,readonly=on,media=cdrom,if=none",
            iso.display()
        ),
        "-device".into(),
        "usb-storage,drive=boot,bootindex=9,removable=on".into(),
    ];
    qemu::start_windows_installer(inst, &extra)?;
    answer_cd_prompt(inst);
    log!("installing Windows (~12 min)");
    let took = wait_ready(inst, Duration::from_secs(5400))?;
    log!("{} ready in {}s", inst.name, took.as_secs());
    Ok(())
}

/// Answer the ISO's "Press any key to boot from CD" prompt. Keys go only in the few seconds
/// after the firmware starts the ISO: once Setup's window is up, Enter lands on its Cancel
/// button, and a cached ISO gets there in about 10 s.
fn answer_cd_prompt(inst: &Instance) {
    let serial = inst.dir.join("serial.log");
    let started = || {
        std::fs::read(&serial).is_ok_and(|b| {
            String::from_utf8_lossy(&b)
                .lines()
                .any(|l| l.contains("BdsDxe: starting") && l.contains("USB"))
        })
    };
    for _ in 0..120 {
        if started() {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    for _ in 0..6 {
        let _ = qemu::Qmp::connect(inst).and_then(|mut q| q.send_key("ret"));
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Red Hat's virtio-win drivers for ARM64, as packaged (and tested) by dockur/windows-arm.
const VIRTIO_VERSION: &str = "0.1.285";
const VIRTIO_URL: &str =
    "https://github.com/qemus/virtiso-arm/releases/download/v0.1.285-1/virtio-win-0.1.285.tar.xz";
const VIRTIO_SHA256: &str = "c6712f8d5730c09c1212be9fc3baa18b78534f3c8c136cf02b2cca46515ca310";
const VIRTIO_DRIVERS: [&str; 9] = [
    "Balloon",
    "NetKVM",
    "viogpudo",
    "vioinput",
    "viomem",
    "viorng",
    "vioscsi",
    "vioserial",
    "viostor",
];

/// The virtio drivers, downloaded and unpacked once into the cache.
fn virtio_drivers() -> Result<PathBuf> {
    let dir = cache_dir().join(format!("virtio-win-{VIRTIO_VERSION}"));
    if dir.is_dir() {
        return Ok(dir);
    }
    std::fs::create_dir_all(cache_dir())?;
    let archive = dir.with_extension("tar.xz");
    log!("downloading the virtio drivers for Windows");
    run(
        "curl",
        &[
            "-fsSL",
            "--connect-timeout",
            "30",
            "--speed-limit",
            "1024",
            "--speed-time",
            "120",
            "-o",
            &archive.to_string_lossy(),
            VIRTIO_URL,
        ],
    )?;
    if sha256_file(&archive)? != VIRTIO_SHA256 {
        let _ = std::fs::remove_file(&archive);
        bail!("virtio driver checksum mismatch");
    }
    let tmp = dir.with_extension("tmp");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    run(
        "tar",
        &[
            "-xJf",
            &archive.to_string_lossy(),
            "-C",
            &tmp.to_string_lossy(),
        ],
    )?;
    // The archive's folders are read-only, which would stop `rm -rf ~/.agentpc`.
    run("chmod", &["-R", "u+w", &tmp.to_string_lossy()])?;
    std::fs::rename(&tmp, &dir)?;
    std::fs::remove_file(&archive)?;
    Ok(dir)
}

/// Write setup.img, the FAT disk Windows Setup reads next to the ISO: the answer file,
/// the drivers it needs to see the virtio disk and network, and the first-logon payload.
fn windows_setup_img(img: &Path) -> Result<()> {
    let drivers = virtio_drivers()?;
    std::fs::File::create(img)?.set_len(64 << 20)?;
    let out = Command::new("hdiutil")
        .args([
            "attach",
            "-nomount",
            "-imagekey",
            "diskimage-class=CRawDiskImage",
        ])
        .arg(img)
        .output()
        .context("run hdiutil")?;
    if !out.status.success() {
        bail!(
            "hdiutil attach {} failed: {}",
            img.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let dev = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .context("hdiutil attach: no device")?
        .to_string();
    let mnt = img.with_extension("mnt");
    let filled = (|| -> Result<()> {
        let quiet = |cmd: &str, args: &[&str]| -> Result<()> {
            let o = Command::new(cmd)
                .args(args)
                .output()
                .with_context(|| format!("run {cmd}"))?;
            if !o.status.success() {
                bail!(
                    "{cmd} failed: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                );
            }
            Ok(())
        };
        // One sector per cluster keeps a 64 MB volume above FAT32's minimum cluster count.
        quiet("newfs_msdos", &["-F", "32", "-c", "1", "-v", "SETUP", &dev])?;
        std::fs::create_dir_all(&mnt)?;
        quiet(
            "diskutil",
            &["mount", "-mountPoint", &mnt.to_string_lossy(), &dev],
        )?;
        std::fs::write(mnt.join("Autounattend.xml"), WIN_AUTOUNATTEND)?;
        for name in VIRTIO_DRIVERS {
            let src = drivers.join(name).join("w11/ARM64");
            copy_files(&src, &mnt.join("$OEM$/$$/Drivers").join(name))?;
            // The display driver waits for SetupComplete.cmd: installed during setup, it
            // can disrupt the steps that follow.
            if name != "viogpudo" {
                copy_files(&src, &mnt.join("$WinPEDriver$").join(name))?;
            }
        }
        let scripts = mnt.join("$OEM$/$$/Setup/Scripts");
        std::fs::create_dir_all(&scripts)?;
        std::fs::write(scripts.join("SetupComplete.cmd"), WIN_SETUP_COMPLETE)?;
        let oem = mnt.join("$OEM$/$1/OEM");
        std::fs::create_dir_all(&oem)?;
        for (name, body) in WIN_OEM {
            std::fs::write(oem.join(name), body)?;
        }
        std::fs::copy(public_key(), oem.join("authorized_keys")).context("copy SSH public key")?;
        // FAT can't hold macOS xattrs, so they'd land as ._* files and ship to the guest.
        let _ = Command::new("dot_clean").arg("-m").arg(&mnt).status();
        Ok(())
    })();
    let detached = Command::new("hdiutil")
        .args(["detach", &dev])
        .stdout(Stdio::null())
        .status();
    let _ = std::fs::remove_dir(&mnt);
    filled?;
    if !detached.is_ok_and(|s| s.success()) {
        bail!("hdiutil detach {dev} failed");
    }
    Ok(())
}

/// Copy the files (not subdirectories) of `src` into `dst`.
fn copy_files(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src).with_context(|| format!("read {}", src.display()))? {
        let e = e?;
        if e.file_type()?.is_file() {
            std::fs::copy(e.path(), dst.join(e.file_name()))?;
        }
    }
    Ok(())
}

fn build_ubuntu(inst: &Instance) -> Result<()> {
    let version = &inst.image.version;
    let file = format!("ubuntu-{version}-server-cloudimg-arm64.img");
    let base = cache_dir().join(&file);
    if !base.is_file() {
        log!("downloading the Ubuntu {version} cloud image");
        let url = format!("https://cloud-images.ubuntu.com/releases/{version}/release/{file}");
        let part = base.with_extension("img.part");
        if run(
            "curl",
            &[
                "-fsSL",
                "--connect-timeout",
                "30",
                "--speed-limit",
                "1024",
                "--speed-time",
                "120",
                "-o",
                &part.to_string_lossy(),
                &url,
            ],
        )
        .is_err()
        {
            bail!(
                "no Ubuntu {version} cloud image for arm64; releases are listed at \
                 https://cloud-images.ubuntu.com/releases/"
            );
        }
        std::fs::rename(&part, &base)?;
    }
    // Relative backing path so $AGENTPC_HOME can move; promote flattens it anyway.
    run(
        "qemu-img",
        &[
            "create",
            "-q",
            "-f",
            "qcow2",
            "-b",
            &format!("../../cache/{file}"),
            "-F",
            "qcow2",
            &inst.disk().to_string_lossy(),
            "40G",
        ],
    )?;
    create_vars(inst)?;

    let seed = inst.dir.join("seed");
    std::fs::create_dir_all(&seed)?;
    let pubkey = std::fs::read_to_string(public_key()).context("read SSH public key")?;
    std::fs::write(
        seed.join("user-data"),
        UBUNTU_USER_DATA.replace("__SSH_KEY__", &format!("\"{}\"", pubkey.trim())),
    )?;
    std::fs::write(
        seed.join("meta-data"),
        "instance-id: ubuntu-agent\nlocal-hostname: ubuntu-agent\n",
    )?;
    let iso = inst.dir.join("seed.iso");
    run(
        "hdiutil",
        &[
            "makehybrid",
            "-quiet",
            "-iso",
            "-joliet",
            "-default-volume-name",
            "cidata",
            "-o",
            &iso.to_string_lossy(),
            &seed.to_string_lossy(),
        ],
    )?;
    qemu::start(
        inst,
        &[
            "-drive".into(),
            format!("file={},if=virtio,format=raw,readonly=on", iso.display()),
        ],
    )?;
    log!("provisioning Ubuntu (~3 min)");
    let took = wait_ready(inst, Duration::from_secs(2700))?;
    log!("{} ready in {}s", inst.name, took.as_secs());
    Ok(())
}

/// Freeze the cleanly shut-down build disk as the image (flattened, read-only).
fn promote_image(inst: &Instance) -> Result<()> {
    let image = &inst.image;
    qemu::stop(inst)?;
    log!("writing the {image} image");
    let (disk, vars) = (image.disk(), image.vars());
    let _ = std::fs::remove_file(&disk);
    let _ = std::fs::remove_file(&vars);
    let tmp = disk.with_extension("qcow2.tmp");
    run(
        "qemu-img",
        &[
            "convert",
            "-O",
            "qcow2",
            &inst.disk().to_string_lossy(),
            &tmp.to_string_lossy(),
        ],
    )?;
    std::fs::rename(&tmp, &disk)?;
    std::fs::copy(inst.vars(), &vars)?;
    for p in [&disk, &vars] {
        let mut perm = std::fs::metadata(p)?.permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(p, perm)?;
    }
    std::fs::remove_dir_all(&inst.dir)?;
    let size = std::fs::metadata(&disk)?.len() as f64 / 1e9;
    log!("{image} image ready ({size:.1} GB)");
    Ok(())
}

#[cfg(test)]
mod tests {
    /// Downloads the 7.3 GB Windows ISO from Microsoft; run with AGENTPC_HOME set to a scratch dir.
    #[test]
    #[ignore]
    fn downloads_windows_iso() {
        assert!(
            std::env::var_os("AGENTPC_HOME").is_some(),
            "set AGENTPC_HOME to a scratch dir"
        );
        let w = &super::WINDOWS_ISOS[0];
        let iso = super::download_windows_iso(w).unwrap();
        assert_eq!(std::fs::metadata(iso).unwrap().len(), w.size);
    }

    #[test]
    fn describes_microsoft_iso_names() {
        assert_eq!(
            super::describe_iso(
                "26100.4349.250607-1500.ge_release_svc_refresh_CLIENTCONSUMER_RET_A64FRE_en-us.iso"
            ),
            "Windows 11 24H2 ISO, ARM64, consumer editions, build 26100.4349, en-us"
        );
        assert_eq!(
            super::describe_iso(
                "26200.6584.250915-1905.25h2_ge_release_svc_refresh_CLIENT_CONSUMER_a64fre_en-us.iso"
            ),
            "Windows 11 25H2 ISO, ARM64, consumer editions, build 26200.6584, en-us"
        );
        assert_eq!(
            super::describe_iso("my-windows.iso"),
            "Windows ISO my-windows.iso"
        );
    }

    #[test]
    fn reads_the_release_from_iso_names() {
        use super::iso_release;
        for (name, release) in [
            (
                "26200.6584.250915-1905.25h2_ge_release_svc_refresh_CLIENT_CONSUMER_a64fre_en-us.iso",
                Some("11-25h2"),
            ),
            (
                "26100.4349.250607-1500.ge_release_svc_refresh_CLIENTCONSUMER_RET_A64FRE_en-us.iso",
                Some("11-24h2"),
            ),
            ("Win11_24H2_English_Arm64.iso", Some("11-24h2")),
            ("en-us_windows_11_23h2_arm64.iso", Some("11-23h2")),
            ("my-windows.iso", None),
        ] {
            assert_eq!(iso_release(name).as_deref(), release, "{name}");
        }
    }

    /// Downloads the virtio drivers and writes setup.img with hdiutil; run with AGENTPC_HOME
    /// set to a scratch dir holding id_ed25519.pub.
    #[test]
    #[ignore]
    fn writes_setup_img() {
        let home = std::path::PathBuf::from(std::env::var("AGENTPC_HOME").unwrap());
        super::windows_setup_img(&home.join("setup.img")).unwrap();
    }
}

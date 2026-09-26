//! Images: install an OS once into a build VM (`_build-<os>`, slot 0), freeze its disk
//! as a read-only image that `create` clones, and capture its RAM snapshot.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::instance::{Instance, Os, cache_dir, home, images_dir, instances_dir, ssh_key};
use crate::ops::{run, ssh, wait_ready};
use crate::{log, qemu};

const UBUNTU_IMG_URL: &str =
    "https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-arm64.img";

// Guest assets ship inside the binary: an installed agentpc has no repo next to it.
const UBUNTU_USER_DATA: &str = include_str!("../guests/ubuntu/user-data");
/// The official Windows 11 ARM64 ISO on Microsoft's download servers, with the checksum and
/// size dockur/windows-arm records for it. Used when no ISO is given.
const WIN_ISO_URL: &str = "https://software-static.download.prss.microsoft.com/dbazure/888969d5-f34g-4e03-ac9d-1f9786c66749/26200.6584.250915-1905.25h2_ge_release_svc_refresh_CLIENT_CONSUMER_a64fre_en-us.iso";
const WIN_ISO_SHA256: &str = "32cde0071ed8086b29bb6c8c3bf17ba9e3cdf43200537434a811a9b6cc2711a1";
const WIN_ISO_SIZE: u64 = 7_299_147_776;

const UBUNTU_PREPARE: &str = include_str!("../guests/ubuntu/prepare.sh");
const WIN_PREPARE: &str = include_str!("../guests/windows/prepare.ps1");
const WIN_COMPOSE: &str = include_str!("../guests/windows/compose.yaml");
const WIN_COMPOSE_ISO: &str = include_str!("../guests/windows/compose.iso.yaml");
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

const COLIMA_PROFILE: &str = "agentpc";
const DOCKER_CTX: &str = "colima-agentpc";

pub fn build(os: Os, iso: Option<PathBuf>) -> Result<()> {
    if Instance::list()?.iter().any(|i| i.os == os) {
        bail!("instances of {os} depend on its image; rm them first");
    }
    for d in [home(), cache_dir(), images_dir(), instances_dir()] {
        std::fs::create_dir_all(&d).with_context(|| format!("create {}", d.display()))?;
    }
    ensure_ssh_key()?;

    let name = format!("_build-{os}");
    if instances_dir().join(&name).is_dir() {
        if let Ok(old) = Instance::load(&name) {
            qemu::stop(&old)?;
        }
        std::fs::remove_dir_all(instances_dir().join(&name))?;
    }
    let iso_path = match os {
        Os::Windows => Some(windows_iso(iso)?),
        Os::Ubuntu => None,
    };
    let inst = Instance::create(&name, os, 0)?;
    match &iso_path {
        Some(iso) => build_windows(&inst, iso)?,
        None => build_ubuntu(&inst)?,
    }
    // Bake the defaults into the image too, so the prepare step at snapshot time (after a
    // pull, say) finds them in place instead of redoing slow work like installing Chrome.
    prepare_guest(&inst)?;
    let mut info = guest_info(&inst)?;
    if let (Os::Windows, Some(p)) = (os, &iso_path) {
        record_iso(&mut info, p)?;
    }
    info.built = crate::instance::local_date();
    promote_image(&inst)?;
    write_info(os, &info)?;
    snapshot(os)
}

/// Capture the image's snapshot: boot the image once, let the desktop
/// settle, then save RAM and flatten the disk as it was at that instant. Clones of it
/// resume in about a second instead of booting.
pub fn snapshot(os: Os) -> Result<()> {
    if !os.image_disk().is_file() {
        bail!("no {os} image; run: agentpc image build {os}");
    }
    if Instance::list()?
        .iter()
        .any(|i| i.os == os && i.on_snapshot_base())
    {
        bail!("instances of {os} depend on its snapshot; rm them first");
    }
    let name = format!("_snap-{os}");
    if instances_dir().join(&name).is_dir() {
        if let Ok(old) = Instance::load(&name) {
            qemu::quit(&old);
        }
        std::fs::remove_dir_all(instances_dir().join(&name))?;
    }
    let inst = Instance::create(&name, os, 0)?;
    run(
        "qemu-img",
        &[
            "create",
            "-q",
            "-f",
            "qcow2",
            "-b",
            &format!("../../images/{os}.qcow2"),
            "-F",
            "qcow2",
            &inst.disk().to_string_lossy(),
        ],
    )?;
    std::fs::copy(os.image_vars(), inst.vars())?;
    crate::ops::set_writable(&inst.vars())?;

    log!("booting {os} to capture its snapshot");
    qemu::start(&inst, &[])?;
    let took = wait_ready(&inst, Duration::from_secs(os.boot_timeout()))?;
    prepare_guest(&inst)?;
    // Let post-logon startup finish so clones don't all redo it after resuming.
    let settle = match os {
        Os::Windows => 45,
        Os::Ubuntu => 10,
    };
    log!("{os} ready in {}s; settling {settle}s", took.as_secs());
    // Guest-reported fields refresh; build-time ones (base, built, ISO checksum) are kept.
    let fresh = guest_info(&inst)?;
    let mut info = read_info(os).unwrap_or_default();
    if info.base.is_empty() {
        info.base = fresh.base;
    }
    info.os = fresh.os;
    info.version = fresh.version;
    info.version_id = fresh.version_id;
    info.arch = fresh.arch;
    info.agentpc = fresh.agentpc;
    info.desktop_server = fresh.desktop_server;
    write_info(os, &info)?;
    std::thread::sleep(Duration::from_secs(settle));

    let (disk, vars, state) = (os.snapshot_disk(), os.snapshot_vars(), os.snapshot_state());
    for p in [&disk, &vars, &state] {
        let _ = std::fs::remove_file(p);
    }
    let state_tmp = state.with_extension("state.tmp");
    qemu::save_state(&inst, &state_tmp)?;
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
    std::fs::rename(&state_tmp, &state)?;
    for p in [&disk, &vars, &state] {
        let mut perm = std::fs::metadata(p)?.permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(p, perm)?;
    }
    std::fs::remove_dir_all(&inst.dir)?;
    let gb = |p: &PathBuf| {
        std::fs::metadata(p)
            .map(|m| m.len() as f64 / 1e9)
            .unwrap_or(0.0)
    };
    log!(
        "snapshot for {os} ready (disk {:.1} GB, memory {:.1} GB)",
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

/// What an image contains. Kept next to it as `<os>.json` and published as its OCI config.
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
    /// The desktop-control server agents drive, e.g. "Windows-MCP 0.8.5".
    #[serde(default)]
    pub desktop_server: String,
    /// Checksum of the Windows ISO it was built from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iso_sha256: Option<String>,
    /// Registry reference, when the image was pulled rather than built here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pulled_from: Option<String>,
}

pub fn read_info(os: Os) -> Option<ImageInfo> {
    serde_json::from_slice(&std::fs::read(os.image_info()).ok()?).ok()
}

pub fn write_info(os: Os, info: &ImageInfo) -> Result<()> {
    std::fs::write(os.image_info(), serde_json::to_vec_pretty(info)?)?;
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
$mcp = & 'C:\uv\uv.exe' tool list 2>$null | Select-String '^windows-mcp v'
"caption=$((Get-CimInstance Win32_OperatingSystem).Caption -replace '^Microsoft ', '')"
"release=$($v.DisplayVersion)"
"build=$($v.CurrentBuild).$($v.UBR)"
"arch=$($env:PROCESSOR_ARCHITECTURE.ToLower())"
"server=$(if ($mcp) { $mcp.Line -replace '^windows-mcp v', '' })""#
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
                format!("Windows-MCP {server}"),
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
    info.iso_sha256 = Some(sha256_file(iso)?);
    info.base = describe_iso(&name);
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
    let release = match build_no {
        26200.. => " 25H2",
        26100.. => " 24H2",
        22631.. => " 23H2",
        22621.. => " 22H2",
        22000.. => " 21H2",
        _ => "",
    };
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
    for os in Os::ALL {
        if let Ok(m) = std::fs::metadata(os.image_disk()) {
            let snap = if os.has_snapshot() {
                "snapshot: yes"
            } else {
                "snapshot: no"
            };
            let version = read_info(os).map_or("version unknown".into(), |i| i.version);
            s += &format!(
                "{os:<8} {version:<42} {:>5.1} GB  {snap}\n",
                m.len() as f64 / 1e9
            );
        }
    }
    if s.is_empty() {
        s = "no images (agentpc image pull ubuntu, or agentpc image build <os>)".into();
    }
    Ok(s)
}

pub fn describe(os: Os) -> Result<String> {
    if !os.image_disk().is_file() {
        bail!("no {os} image");
    }
    let info = read_info(os).with_context(|| {
        format!("no version info for the {os} image; run: agentpc image snapshot {os}")
    })?;
    Ok(serde_json::to_string_pretty(&info)?)
}

pub fn remove(os: Os) -> Result<String> {
    if Instance::list()?.iter().any(|i| i.os == os) {
        bail!("VMs of {os} depend on its image; rm them first");
    }
    let files = [
        os.image_info(),
        os.image_disk(),
        os.image_vars(),
        os.snapshot_disk(),
        os.snapshot_vars(),
        os.snapshot_state(),
    ];
    if !files.iter().any(|p| p.exists()) {
        bail!("no {os} image");
    }
    for p in files {
        let _ = std::fs::remove_file(p);
    }
    Ok(format!("removed the {os} image"))
}

fn ensure_ssh_key() -> Result<()> {
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

fn public_key() -> PathBuf {
    ssh_key().with_extension("pub")
}

fn create_vars(inst: &Instance) -> Result<()> {
    std::fs::File::create(inst.vars())?.set_len(64 << 20)?;
    Ok(())
}

/// `--iso`, else `$WIN_ISO`, else the first `~/Downloads/*A64FRE*.iso`.
/// A Windows ISO already on this Mac: `--iso`, `$WIN_ISO`, `~/Downloads/*A64FRE*.iso`, or an
/// earlier download. An explicitly named path is returned even if it doesn't exist.
pub(crate) fn find_windows_iso(iso: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(p) = iso.or_else(|| std::env::var_os("WIN_ISO").map(PathBuf::from)) {
        return Some(p);
    }
    let downloads = PathBuf::from(std::env::var_os("HOME")?).join("Downloads");
    let mut isos: Vec<PathBuf> = std::fs::read_dir(downloads)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.to_ascii_lowercase().contains("a64fre") && n.ends_with(".iso"))
        })
        .collect();
    isos.sort();
    isos.into_iter()
        .next()
        .or_else(|| Some(downloaded_iso_path()).filter(|p| p.is_file()))
}

fn downloaded_iso_path() -> PathBuf {
    cache_dir().join(
        WIN_ISO_URL
            .rsplit('/')
            .next()
            .unwrap_or("windows11-arm64.iso"),
    )
}

/// The ISO to install from, downloading Microsoft's official one if none is on this Mac.
fn windows_iso(iso: Option<PathBuf>) -> Result<PathBuf> {
    match find_windows_iso(iso) {
        Some(p) if p.is_file() => Ok(p),
        Some(p) => bail!("Windows ISO {} not found", p.display()),
        None => download_windows_iso(),
    }
}

fn download_windows_iso() -> Result<PathBuf> {
    let dest = downloaded_iso_path();
    let part = dest.with_extension("iso.part");
    std::fs::create_dir_all(cache_dir())?;
    log!(
        "downloading Windows 11 ARM64 from Microsoft ({:.1} GB, resumable)",
        WIN_ISO_SIZE as f64 / 1e9
    );
    let st = Command::new("curl")
        .args(["-fL", "--retry", "3", "-C", "-", "-#", "-o"])
        .arg(&part)
        .arg(WIN_ISO_URL)
        .status()
        .context("run curl")?;
    if !st.success() {
        bail!("downloading the Windows ISO failed; pass --iso <Windows 11 ARM64 ISO> instead");
    }
    let size = std::fs::metadata(&part)?.len();
    if size != WIN_ISO_SIZE {
        bail!("Windows ISO download is {size} bytes, expected {WIN_ISO_SIZE}; rerun to resume");
    }
    log!("verifying the Windows ISO checksum");
    if sha256_file(&part)? != WIN_ISO_SHA256 {
        let _ = std::fs::remove_file(&part);
        bail!("Windows ISO checksum mismatch; the partial file was removed, rerun to retry");
    }
    std::fs::rename(&part, &dest)?;
    Ok(dest)
}

pub(crate) fn windows_setup_img_path() -> PathBuf {
    cache_dir().join("windows-setup.img")
}

fn build_windows(inst: &Instance, iso: &Path) -> Result<()> {
    let iso = std::fs::canonicalize(iso)?;
    windows_setup_img(&iso)?;
    let setup = inst.dir.join("setup.img");
    std::fs::copy(windows_setup_img_path(), &setup)?;
    windows_refresh_oem(&setup)?;
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
    // Answer "Press any key to boot from CD". Keep it short: once setup's UI is up,
    // Enter lands on its focused Cancel button.
    for _ in 0..15 {
        let _ = qemu::Qmp::connect(inst).and_then(|mut q| q.send_key("ret"));
        std::thread::sleep(Duration::from_secs(1));
    }
    log!("installing Windows (~12 min)");
    let took = wait_ready(inst, Duration::from_secs(5400))?;
    log!("{} ready in {}s", inst.name, took.as_secs());
    Ok(())
}

/// dockur builds Autounattend.xml + ARM virtio drivers into setup.img; only needed once.
fn windows_setup_img(iso: &Path) -> Result<()> {
    let out = windows_setup_img_path();
    if out.is_file() {
        return Ok(());
    }
    let status = Command::new("colima")
        .args(["status", "-p", COLIMA_PROFILE])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if !status.is_ok_and(|s| s.success()) {
        // `colima start` switches the default docker context; put the user's back.
        let prev = Command::new("docker")
            .args(["context", "show"])
            .output()
            .context("run docker")?;
        let prev = String::from_utf8_lossy(&prev.stdout).trim().to_string();
        run(
            "colima",
            &[
                "start",
                COLIMA_PROFILE,
                "--vm-type",
                "vz",
                "--nested-virtualization",
                "--cpus",
                "2",
                "--memory",
                "4",
                "--disk",
                "60",
            ],
        )?;
        if !prev.is_empty() {
            let _ = Command::new("docker")
                .args(["context", "use", &prev])
                .stdout(Stdio::null())
                .status();
        }
    }

    // Compose resolves ./oem relative to the compose file, so lay the assets out together.
    let dir = cache_dir().join("guests/windows");
    std::fs::create_dir_all(dir.join("oem"))?;
    std::fs::write(dir.join("compose.yaml"), WIN_COMPOSE)?;
    std::fs::write(dir.join("compose.iso.yaml"), WIN_COMPOSE_ISO)?;
    for (name, body) in WIN_OEM {
        std::fs::write(dir.join("oem").join(name), body)?;
    }
    let compose = |args: &[&str]| {
        let mut c = Command::new("docker");
        c.args(["--context", DOCKER_CTX, "compose", "-f"])
            .arg(dir.join("compose.yaml"))
            .arg("-f")
            .arg(dir.join("compose.iso.yaml"))
            .args(args)
            .env("WIN_ISO", iso);
        c
    };
    let _ = compose(&["down", "-v"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if !compose(&["up", "-d"])
        .status()
        .context("run docker compose")?
        .success()
    {
        bail!("docker compose up failed");
    }
    log!("waiting for dockur to write setup.img");
    loop {
        let st = Command::new("docker")
            .args([
                "--context",
                DOCKER_CTX,
                "exec",
                "agentpc-windows",
                "test",
                "-f",
                "/run/shm/qemu.pid",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if st.is_ok_and(|s| s.success()) {
            break;
        }
        std::thread::sleep(Duration::from_secs(5));
    }
    let part = out.with_extension("img.part");
    run(
        "docker",
        &[
            "--context",
            DOCKER_CTX,
            "cp",
            "agentpc-windows:/storage/setup.img",
            &part.to_string_lossy(),
        ],
    )?;
    std::fs::rename(&part, &out)?;
    let _ = compose(&["down", "-v"]).stdout(Stdio::null()).status();
    run("colima", &["stop", COLIMA_PROFILE])
}

/// Put the embedded OEM payload (+ SSH public key) into setup.img's `C:\OEM`.
fn windows_refresh_oem(img: &Path) -> Result<()> {
    let out = Command::new("hdiutil")
        .args([
            "attach",
            "-nobrowse",
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
    let text = String::from_utf8_lossy(&out.stdout);
    let dev = text
        .split_whitespace()
        .next()
        .context("hdiutil attach: no device")?
        .to_string();
    let mnt = text
        .lines()
        .filter(|l| l.contains("/Volumes"))
        .filter_map(|l| l.split('\t').next_back())
        .map(|m| PathBuf::from(m.trim()))
        .next();
    let copied = (|| -> Result<()> {
        let mnt = mnt.context("hdiutil attach: no mounted volume")?;
        let oem = mnt.join("$OEM$/$1/OEM");
        if !oem.is_dir() {
            bail!("{} missing in setup.img", oem.display());
        }
        for (name, body) in WIN_OEM {
            std::fs::write(oem.join(name), body)?;
        }
        std::fs::copy(public_key(), oem.join("authorized_keys")).context("copy SSH public key")?;
        // FAT can't hold macOS xattrs, so they land as ._* files that would ship to C:\OEM.
        let _ = Command::new("dot_clean").arg("-m").arg(&oem).status();
        Ok(())
    })();
    let detached = Command::new("hdiutil")
        .args(["detach", &dev])
        .stdout(Stdio::null())
        .status();
    copied?;
    if !detached.is_ok_and(|s| s.success()) {
        bail!("hdiutil detach {dev} failed");
    }
    Ok(())
}

fn build_ubuntu(inst: &Instance) -> Result<()> {
    let base = cache_dir().join("ubuntu-base.img");
    if !base.is_file() {
        log!("downloading Ubuntu cloud image");
        let part = cache_dir().join("ubuntu-base.img.part");
        run(
            "curl",
            &["-fsSL", "-o", &part.to_string_lossy(), UBUNTU_IMG_URL],
        )?;
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
            "../../cache/ubuntu-base.img",
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
    let os = inst.os;
    qemu::stop(inst)?;
    log!("writing the {os} image");
    let (disk, vars) = (os.image_disk(), os.image_vars());
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
    log!("{os} image ready ({size:.1} GB)");
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
        let iso = super::download_windows_iso().unwrap();
        assert_eq!(std::fs::metadata(iso).unwrap().len(), super::WIN_ISO_SIZE);
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

    /// Needs hdiutil and a dockur setup.img: set AGENTPC_TEST_SETUP_IMG to a scratch COPY
    /// and AGENTPC_HOME to a dir holding id_ed25519.pub.
    #[test]
    #[ignore]
    fn refresh_oem_on_setup_img_copy() {
        let img = std::path::PathBuf::from(std::env::var("AGENTPC_TEST_SETUP_IMG").unwrap());
        super::windows_refresh_oem(&img).unwrap();
    }
}

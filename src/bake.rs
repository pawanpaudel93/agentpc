//! Baking golden images: install an OS once into a build VM (`_bake-<os>`, slot 0),
//! then freeze its disk as a read-only golden image that `new` clones.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::instance::{Instance, Os, cache_dir, golden_dir, home, instances_dir, ssh_key};
use crate::ops::{run, wait_ready};
use crate::{log, qemu};

const UBUNTU_IMG_URL: &str =
    "https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-arm64.img";

// Guest assets ship inside the binary: an installed agentpc has no repo next to it.
const UBUNTU_USER_DATA: &str = include_str!("../guests/ubuntu/user-data");
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

pub fn bake(os: Os, iso: Option<PathBuf>) -> Result<()> {
    if Instance::list()?.iter().any(|i| i.os == os) {
        bail!("instances of {os} depend on its golden image; rm them first");
    }
    for d in [home(), cache_dir(), golden_dir(), instances_dir()] {
        std::fs::create_dir_all(&d).with_context(|| format!("create {}", d.display()))?;
    }
    ensure_ssh_key()?;

    let name = format!("_bake-{os}");
    if instances_dir().join(&name).is_dir() {
        if let Ok(old) = Instance::load(&name) {
            qemu::stop(&old)?;
        }
        std::fs::remove_dir_all(instances_dir().join(&name))?;
    }
    let inst = Instance::create(&name, os, 0)?;
    match os {
        Os::Windows => bake_windows(&inst, iso)?,
        Os::Ubuntu => bake_ubuntu(&inst)?,
    }
    promote_golden(&inst)?;
    snapshot(os)
}

/// Capture the live golden image: boot the cold golden image once, let the desktop
/// settle, then save RAM and flatten the disk as it was at that instant. Clones of it
/// resume in about a second instead of booting.
pub fn snapshot(os: Os) -> Result<()> {
    if !os.golden_disk().is_file() {
        bail!("no golden image for {os}; run: agentpc bake {os}");
    }
    if Instance::list()?
        .iter()
        .any(|i| i.os == os && i.on_live_base())
    {
        bail!("instances of {os} depend on its live snapshot; rm them first");
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
            &format!("../../golden/{os}.qcow2"),
            "-F",
            "qcow2",
            &inst.disk().to_string_lossy(),
        ],
    )?;
    std::fs::copy(os.golden_vars(), inst.vars())?;
    crate::ops::set_writable(&inst.vars())?;

    log!("booting {os} to capture a live snapshot");
    qemu::start(&inst, &[])?;
    let took = wait_ready(&inst, Duration::from_secs(os.boot_timeout()))?;
    // Let post-logon startup finish so clones don't all redo it after resuming.
    let settle = match os {
        Os::Windows => 45,
        Os::Ubuntu => 10,
    };
    log!("{os} ready in {}s; settling {settle}s", took.as_secs());
    std::thread::sleep(Duration::from_secs(settle));

    let (disk, vars, state) = (os.live_disk(), os.live_vars(), os.live_state());
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
        "live snapshot for {os} ready (disk {:.1} GB, memory {:.1} GB)",
        gb(&disk),
        gb(&state)
    );
    Ok(())
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
pub(crate) fn find_windows_iso(iso: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(p) = iso.or_else(|| std::env::var_os("WIN_ISO").map(PathBuf::from)) {
        return Some(p);
    }
    let downloads = PathBuf::from(std::env::var_os("HOME")?).join("Downloads");
    let mut isos: Vec<PathBuf> = std::fs::read_dir(downloads)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains("A64FRE") && n.ends_with(".iso"))
        })
        .collect();
    isos.sort();
    isos.into_iter().next()
}

pub(crate) fn windows_setup_img_path() -> PathBuf {
    cache_dir().join("windows-setup.img")
}

fn bake_windows(inst: &Instance, iso: Option<PathBuf>) -> Result<()> {
    let iso = find_windows_iso(iso).filter(|p| p.is_file()).context(
        "no Windows 11 ARM64 ISO: pass --iso, set WIN_ISO, or put *A64FRE*.iso in ~/Downloads",
    )?;
    let iso = std::fs::canonicalize(&iso)?;
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
    qemu::start(inst, &extra)?;
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

fn bake_ubuntu(inst: &Instance) -> Result<()> {
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

/// Freeze the cleanly shut-down build disk as the golden image (flattened, read-only).
fn promote_golden(inst: &Instance) -> Result<()> {
    let os = inst.os;
    qemu::stop(inst)?;
    log!("writing golden image for {os}");
    let (disk, vars) = (os.golden_disk(), os.golden_vars());
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
    log!("golden {os} ready ({size:.1} GB)");
    Ok(())
}

#[cfg(test)]
mod tests {
    /// Needs hdiutil and a dockur setup.img: set AGENTPC_TEST_SETUP_IMG to a scratch COPY
    /// and AGENTPC_HOME to a dir holding id_ed25519.pub.
    #[test]
    #[ignore]
    fn refresh_oem_on_setup_img_copy() {
        let img = std::path::PathBuf::from(std::env::var("AGENTPC_TEST_SETUP_IMG").unwrap());
        super::windows_refresh_oem(&img).unwrap();
    }
}

//! Images: install an OS once into a build VM (`_build-<image>`, in a free slot), freeze its disk
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
const ARCH_PREPARE: &str = include_str!("../guests/arch/prepare.sh");
const ARCH_BUILD: &str = include_str!("../guests/arch/build.sh");
const UBUNTU_X86_APPS: &str = include_str!("../guests/ubuntu/x86apps.sh");
const ARCH_X86_APPS: &str = include_str!("../guests/arch/x86apps.sh");
const FEX_PATCH: &str = include_str!("../guests/ubuntu/fex.patch");
const WIN_PREPARE: &str = include_str!("../guests/windows/prepare.ps1");
/// Helpers the Linux guest scripts install (`guests/helpers/`), uploaded to
/// /tmp/agentpc-helpers before they run. Kept as files so CI can test them without a VM.
const LINUX_HELPERS: &[(&str, &str)] = &[
    ("agentpc-fex", include_str!("../guests/helpers/agentpc-fex")),
    (
        "fexbash-sudo",
        include_str!("../guests/helpers/fexbash-sudo"),
    ),
    ("fex-unit", include_str!("../guests/helpers/fex-unit")),
    (
        "maintscript-uname",
        include_str!("../guests/helpers/maintscript-uname"),
    ),
    (
        "maintscript-dpkg",
        include_str!("../guests/helpers/maintscript-dpkg"),
    ),
    (
        "agentpc-install-deb",
        include_str!("../guests/helpers/agentpc-install-deb"),
    ),
];
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
    let _lock = crate::instance::image_lock(image)?;
    build_locked(image, iso)
}

/// Delete this image's half-written temp files (`*.qcow2.tmp`, `*.state.tmp[.machine]`) left by an
/// aborted build or snapshot. Best-effort: it runs on the failure path.
fn remove_image_tmp(image: &Image) {
    let prefix = format!("{image}.");
    for e in std::fs::read_dir(images_dir())
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix) && (name.ends_with(".tmp") || name.ends_with(".tmp.machine")) {
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
    /// The image being made, whose temp files a failure clears (an Arch build runs in a
    /// VM of the Ubuntu image).
    image: Image,
    armed: bool,
}

impl BuildGuard {
    fn new(inst: Instance) -> Self {
        let image = inst.image.clone();
        Self::making(inst, image)
    }
    fn making(inst: Instance, image: Image) -> Self {
        Self {
            inst,
            image,
            armed: true,
        }
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
        remove_image_tmp(&self.image);
    }
}

/// `build`, for a caller already holding the image lock.
pub(crate) fn build_locked(image: &Image, iso: Option<PathBuf>) -> Result<()> {
    let os = image.os;
    if !image.instances()?.is_empty() {
        bail!("VMs of {image} depend on it; rm them first");
    }
    let busy = image.busy_builds();
    if !busy.is_empty() {
        bail!(
            "{image} is in use by a running build ({}); try again when it finishes",
            busy.join(", ")
        );
    }
    // A build is always of today, so it can't stand in for a dated published one.
    if image.pinned() {
        bail!("{}", pinned_build_error(image));
    }
    for d in [home(), cache_dir(), images_dir(), instances_dir()] {
        std::fs::create_dir_all(&d).with_context(|| format!("create {}", d.display()))?;
    }
    ensure_ssh_key()?;
    // Peak use: the build disk plus its flattened copy, then the image plus its snapshot.
    let need_gb = match os {
        Os::Windows => 35,
        Os::Ubuntu | Os::Arch if image.x86_apps() => 16,
        Os::Ubuntu | Os::Arch => 12,
    };
    crate::ops::ensure_free_space(need_gb, &format!("build {image}"))?;

    let name = format!("_build-{image}");
    if instances_dir().join(&name).is_dir() {
        if let Ok(old) = Instance::load(&name) {
            qemu::stop(&old)?;
        }
        std::fs::remove_dir_all(instances_dir().join(&name))?;
    }
    if os == Os::Arch {
        return build_arch(image, &name);
    }
    let iso_path = match os {
        Os::Windows => {
            let iso = windows_iso(&image.version, iso)?;
            log!("installing from {}", iso.display());
            Some(iso)
        }
        Os::Ubuntu | Os::Arch => None,
    };
    let guard = BuildGuard::new(Instance::create_scratch(&name, image)?);
    match &iso_path {
        Some(iso) => build_windows(&guard.inst, iso)?,
        None => build_ubuntu(&guard.inst)?,
    }
    // Bake the defaults into the image too, so the prepare step at snapshot time (after a
    // pull, say) finds them in place instead of redoing slow work like installing Chrome.
    prepare_guest(&guard.inst)?;
    let mut info = guest_info(&guard.inst)?;
    check_desktop_server(image, &info.desktop_server)?;
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

/// Why `image` (a dated published build) can't be built here.
pub(crate) fn pinned_build_error(image: &Image) -> String {
    format!(
        "pinned builds are download-only: {} image pull {image}",
        crate::setup::cmd_name()
    )
}

/// Capture the image's snapshot: boot the image once, let the desktop
/// settle, then save RAM and flatten the disk as it was at that instant. Clones of it
/// resume in seconds instead of booting.
pub fn snapshot(image: &Image) -> Result<()> {
    let _lock = crate::instance::image_lock(image)?;
    snapshot_locked(image)
}

/// Apply this agentpc's guest setup (`prepare_guest`) to the image's base disk: boot a clone,
/// run it, shut down cleanly and make that disk the base. VMs of a non-default size boot from
/// the base rather than the snapshot, so it must have what the snapshot has (and an Arch
/// build's base never ran the guest setup at all). Runs before every snapshot.
fn bake_base(image: &Image) -> Result<()> {
    let name = format!("_build-{image}");
    if instances_dir().join(&name).is_dir() {
        if let Ok(old) = Instance::load(&name) {
            qemu::quit(&old);
        }
        std::fs::remove_dir_all(instances_dir().join(&name))?;
    }
    let guard = BuildGuard::new(Instance::create_scratch(&name, image)?);
    let inst = &guard.inst;
    clone_image(inst, image)?;
    log!("booting {image} to apply this agentpc's guest setup to its disk");
    qemu::start(inst, &[])?;
    wait_ready(inst, Duration::from_secs(image.os.boot_timeout()))?;
    prepare_guest(inst)?;
    qemu::stop(inst)?;
    let (disk, vars) = (image.disk(), image.vars());
    wait_for_space(
        allocated(&disk) + allocated(&inst.disk()) + (1 << 30),
        &format!("write the {image} image"),
    )?;
    let (disk_tmp, vars_tmp) = (
        disk.with_extension("qcow2.tmp"),
        vars.with_extension("fd.tmp"),
    );
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
    std::fs::copy(inst.vars(), &vars_tmp)?;
    // The old snapshot was captured from the old base; it is recaptured next.
    image.remove_snapshot();
    std::fs::rename(&vars_tmp, &vars)?;
    std::fs::rename(&disk_tmp, &disk)?;
    for p in [&disk, &vars] {
        let mut perm = std::fs::metadata(p)?.permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(p, perm)?;
    }
    std::fs::remove_dir_all(&inst.dir)?;
    guard.keep();
    Ok(())
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
    // Both disks are rewritten: the base (custom-size VMs boot from it) and the snapshot.
    let vms = image.instances()?;
    if !vms.is_empty() {
        let names: Vec<_> = vms.iter().map(|i| i.name.as_str()).collect();
        bail!(
            "VMs of {image} use its disks; rm them first: {}",
            names.join(", ")
        );
    }
    bake_base(image)?;
    let name = format!("_snap-{image}");
    if instances_dir().join(&name).is_dir() {
        if let Ok(old) = Instance::load(&name) {
            qemu::quit(&old);
        }
        std::fs::remove_dir_all(instances_dir().join(&name))?;
    }
    let guard = BuildGuard::new(Instance::create_scratch(&name, image)?);
    let inst = &guard.inst;
    clone_image(inst, image)?;

    log!("booting {image} to capture its snapshot");
    qemu::start(inst, &[])?;
    let took = wait_ready(inst, Duration::from_secs(os.boot_timeout()))?;
    prepare_guest(inst)?;
    // Let post-logon startup finish so clones don't all redo it after resuming.
    let settle = match os {
        Os::Windows => 45,
        Os::Ubuntu | Os::Arch => 10,
    };
    log!("{os} ready in {}s; settling {settle}s", took.as_secs());
    // Guest-reported fields refresh; build-time ones (base, built, ISO checksum) are kept.
    let fresh = guest_info(inst)?;
    check_desktop_server(image, &fresh.desktop_server)?;
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
    info.guest_scripts = guest_scripts_id(image);
    write_info(image, &info)?;
    std::thread::sleep(Duration::from_secs(settle));

    let (disk, vars, state) = (
        image.snapshot_disk(),
        image.snapshot_vars(),
        image.snapshot_state(),
    );
    image.remove_snapshot();
    let state_tmp = state.with_extension("state.tmp");
    let mem = u64::from(inst.size().0) << 30;
    // The flattened convert writes the image's data plus the overlay's in full.
    wait_for_space(
        allocated(&image.disk()) + allocated(&inst.disk()) + mem + (1 << 30),
        &format!("save the {} snapshot", inst.image),
    )?;
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

/// Give a scratch VM a disk on top of `image`'s (a qcow2 overlay) and a copy of its vars.
fn clone_image(inst: &Instance, image: &Image) -> Result<()> {
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
    crate::ops::set_writable(&inst.vars())
}

/// Build the Arch Linux ARM image: a helper VM of the default Ubuntu image gets a blank
/// second disk (/dev/vdb), `guests/arch/build.sh` installs Arch onto it, and that disk
/// becomes the image. Then its snapshot is captured like any other image's.
fn build_arch(image: &Image, name: &str) -> Result<()> {
    let helper = Image::new(Os::Ubuntu, None)?;
    // Hold the Ubuntu image only while cloning it: the clone's overlay is all the build uses.
    let guard = {
        let _lock = crate::instance::image_lock(&helper)?;
        crate::ops::fetch_image_locked(&helper)?;
        let guard = BuildGuard::making(Instance::create_scratch(name, &helper)?, image.clone());
        clone_image(&guard.inst, &helper)?;
        guard
    };
    let inst = &guard.inst;
    let target = inst.dir.join("arch.qcow2");
    run(
        "qemu-img",
        &[
            "create",
            "-q",
            "-f",
            "qcow2",
            &target.to_string_lossy(),
            "40G",
        ],
    )?;
    // Added after the helper's own virtio disk (vda), so the guest sees it as /dev/vdb.
    qemu::start(
        inst,
        &[
            "-drive".into(),
            format!(
                "file={},if=virtio,format=qcow2,discard=unmap",
                target.display()
            ),
        ],
    )?;
    // The x86 part comes at the snapshot, from prepare_guest.
    let minutes = if image.x86_apps() { 10 } else { 6 };
    log!("building Arch Linux ARM (~{minutes} min in all)");
    let took = wait_ready(inst, Duration::from_secs(Os::Ubuntu.boot_timeout()))?;
    log!("{} ready in {}s", inst.name, took.as_secs());
    let script = inst.dir.join("build.sh");
    std::fs::write(&script, ARCH_BUILD)?;
    crate::ops::upload(inst, &script, "/tmp/agentpc-arch-build.sh")?;
    let cached = seed_alarm_cache(inst)?;
    let tarball = run_arch_build(inst)?;
    if !cached {
        keep_alarm_tarball(inst);
    }

    qemu::stop(inst)?;
    log!("writing the {image} image");
    let (disk, vars) = (image.disk(), image.vars());
    let _ = std::fs::remove_file(&disk);
    let _ = std::fs::remove_file(&vars);
    // An old snapshot would resume the previous build if the new one fails.
    image.remove_snapshot();
    let tmp = disk.with_extension("qcow2.tmp");
    wait_for_space(
        allocated(&target) + (1 << 30),
        &format!("write the {image} image"),
    )?;
    run(
        "qemu-img",
        &[
            "convert",
            "-O",
            "qcow2",
            &target.to_string_lossy(),
            &tmp.to_string_lossy(),
        ],
    )?;
    // Blank vars: edk2 initializes them at first boot and finds systemd-boot on the disk.
    // In place whole before the disk, which is what makes the image exist.
    let vars_tmp = vars.with_extension("fd.tmp");
    std::fs::File::create(&vars_tmp)?.set_len(64 << 20)?;
    std::fs::rename(&vars_tmp, &vars)?;
    std::fs::rename(&tmp, &disk)?;
    for p in [&disk, &vars] {
        let mut perm = std::fs::metadata(p)?.permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(p, perm)?;
    }
    std::fs::remove_dir_all(&inst.dir)?;
    // The image is in place and the helper VM gone; snapshot_locked guards its own VM.
    guard.keep();
    let size = std::fs::metadata(&disk)?.len() as f64 / 1e9;
    log!("{image} image ready ({size:.1} GB)");
    let built = crate::instance::local_date();
    let info = ImageInfo {
        os: Os::Arch.to_string(),
        version: "Arch Linux ARM".into(),
        version_id: crate::instance::ARCH_VERSION.into(),
        arch: "arm64".into(),
        base: format!(
            "Arch Linux ARM aarch64 tarball, {}",
            tarball.unwrap_or_else(|| built.clone())
        ),
        built,
        agentpc: env!("CARGO_PKG_VERSION").into(),
        ..Default::default()
    };
    write_info(image, &info)?;
    snapshot_locked(image)
}

/// The Arch Linux ARM tarball and its signature. build.sh verifies and uses them when
/// they are in `ALARM_GUEST_DIR` at its start, and leaves the ones it downloads there.
const ALARM_FILES: [&str; 2] = [
    "ArchLinuxARM-aarch64-latest.tar.gz",
    "ArchLinuxARM-aarch64-latest.tar.gz.sig",
];
/// The tarball's Last-Modified date, which build.sh reports; optional, carried along if present.
const ALARM_DATE: &str = "ArchLinuxARM-aarch64-latest.tar.gz.date";
/// Where build.sh looks for (and leaves) the tarball in the helper VM.
const ALARM_GUEST_DIR: &str = "/home/agent/agentpc-alarm";

/// The host's copy of the tarball, reused across Arch builds (`clean` removes it).
fn alarm_cache_dir() -> PathBuf {
    cache_dir().join("alarm")
}

/// Hand the cached tarball to the helper VM, if the host has one. Returns whether it did.
fn seed_alarm_cache(inst: &Instance) -> Result<bool> {
    let dir = alarm_cache_dir();
    if !ALARM_FILES.iter().all(|f| dir.join(f).is_file()) {
        return Ok(false);
    }
    let out = ssh(inst, &format!("mkdir -p {ALARM_GUEST_DIR}"))?;
    if !out.status.success() {
        bail!(
            "creating {ALARM_GUEST_DIR} in {} failed: {}",
            inst.name,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    log!("using the cached Arch Linux ARM tarball");
    for f in ALARM_FILES {
        crate::ops::upload(inst, &dir.join(f), &format!("{ALARM_GUEST_DIR}/{f}"))?;
    }
    if dir.join(ALARM_DATE).is_file() {
        let _ = crate::ops::upload(
            inst,
            &dir.join(ALARM_DATE),
            &format!("{ALARM_GUEST_DIR}/{ALARM_DATE}"),
        );
    }
    Ok(true)
}

/// Copy the tarball build.sh downloaded back to the host cache, for the next build.
/// Best-effort: a failure only means the next build downloads it again.
fn keep_alarm_tarball(inst: &Instance) {
    let dir = alarm_cache_dir();
    // Named per build VM: two Arch builds can finish at once.
    let part = |f: &str| dir.join(format!("{f}.{}.part", inst.name));
    let got = std::fs::create_dir_all(&dir)
        .map_err(anyhow::Error::from)
        .and_then(|()| {
            for f in ALARM_FILES {
                let part = part(f);
                crate::ops::download(inst, &format!("{ALARM_GUEST_DIR}/{f}"), &part)?;
                std::fs::rename(&part, dir.join(f))?;
            }
            // Only the date build.sh reports; without it a reused tarball says "cached".
            let part = part(ALARM_DATE);
            if crate::ops::download(inst, &format!("{ALARM_GUEST_DIR}/{ALARM_DATE}"), &part).is_ok()
            {
                let _ = std::fs::rename(&part, dir.join(ALARM_DATE));
            } else {
                let _ = std::fs::remove_file(&part);
            }
            Ok(())
        });
    if let Err(e) = got {
        for f in ALARM_FILES {
            let _ = std::fs::remove_file(part(f));
        }
        log!("not caching the Arch Linux ARM tarball: {e:#}");
    }
}

/// Lines of build.sh output kept for the error when it fails.
const BUILD_TAIL: usize = 20;

/// Run the uploaded build.sh against /dev/vdb in the helper VM (as root; ~5 min, ~1 GB of
/// downloads). ssh itself has no timeout, only keepalives that notice a dead VM. Its
/// `==> ` lines are logged as progress; returns what its `alarm-tarball: ` line reports.
fn run_arch_build(inst: &Instance) -> Result<Option<String>> {
    use std::io::BufRead;
    let mut child = Command::new("ssh")
        .args(crate::ops::ssh_args(
            inst,
            "sudo bash /tmp/agentpc-arch-build.sh /dev/vdb 2>&1",
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .context("run ssh")?;
    let mut tail = std::collections::VecDeque::with_capacity(BUILD_TAIL);
    let mut tarball = None;
    let out = child.stdout.take().context("ssh stdout")?;
    for line in std::io::BufReader::new(out).lines() {
        let line = line?;
        if let Some(p) = line.strip_prefix("==> ") {
            log!("{p}");
        } else if let Some(t) = line.strip_prefix("alarm-tarball:") {
            tarball = Some(t.trim().to_string()).filter(|t| !t.is_empty());
            if let Some(t) = &tarball {
                log!("Arch Linux ARM tarball: {t}");
            }
        }
        if tail.len() == BUILD_TAIL {
            tail.pop_front();
        }
        tail.push_back(line);
    }
    let st = child.wait()?;
    if !st.success() {
        bail!(
            "building Arch Linux ARM failed ({st}); last output:\n{}",
            Vec::from(tail).join("\n")
        );
    }
    Ok(tarball)
}

/// Apply the agent-friendly defaults in `guests/<os>/prepare.*` to a running guest, after
/// `guests/<os>/x86apps.sh` on an x86apps image (prepare.sh's cleanup then shrinks both).
/// It runs at every snapshot, so existing and pulled images get it too.
fn prepare_guest(inst: &Instance) -> Result<()> {
    if inst.os.is_linux() {
        let dir = inst.dir.join("helpers");
        std::fs::create_dir_all(&dir)?;
        for (name, body) in LINUX_HELPERS {
            std::fs::write(dir.join(name), body)?;
        }
        ssh(inst, "rm -rf /tmp/agentpc-helpers")?;
        crate::ops::upload(inst, &dir, "/tmp/agentpc-helpers")?;
    }
    let res = prepare_scripts(inst);
    if inst.os.is_linux() {
        let _ = ssh(inst, "rm -rf /tmp/agentpc-helpers");
    }
    res
}

fn prepare_scripts(inst: &Instance) -> Result<()> {
    if inst.image.x86_apps() {
        log!("installing FEX for x86 programs");
        // Both OSes build the same FEX, with the same patch.
        let patch = inst.dir.join("fex.patch");
        std::fs::write(&patch, FEX_PATCH)?;
        crate::ops::upload(inst, &patch, "/tmp/agentpc-fex.patch")?;
        let script = match inst.os {
            Os::Arch => ARCH_X86_APPS,
            _ => UBUNTU_X86_APPS,
        };
        run_guest_script(inst, script, "x86apps")?;
    }
    log!("applying agent defaults to {}", inst.os);
    let script = match inst.os {
        Os::Ubuntu => UBUNTU_PREPARE,
        Os::Arch => ARCH_PREPARE,
        Os::Windows => WIN_PREPARE,
    };
    run_guest_script(inst, script, "prepare")
}

/// Upload `script` and run it as root (Linux) or as the agent user (Windows).
fn run_guest_script(inst: &Instance, script: &str, what: &str) -> Result<()> {
    let (name, run) = match inst.os {
        Os::Ubuntu | Os::Arch => (
            format!("/tmp/agentpc-{what}.sh"),
            format!("sudo sh /tmp/agentpc-{what}.sh && rm -f /tmp/agentpc-{what}.sh"),
        ),
        Os::Windows => (
            format!("agentpc-{what}.ps1"),
            format!(
                "powershell -NoProfile -ExecutionPolicy Bypass -File \"$env:USERPROFILE\\agentpc-{what}.ps1\"; \
                 $c = $LASTEXITCODE; Remove-Item \"$env:USERPROFILE\\agentpc-{what}.ps1\"; exit $c"
            ),
        ),
    };
    let local = inst.dir.join(format!("{what}-script"));
    std::fs::write(&local, script)?;
    crate::ops::upload(inst, &local, &name)?;
    let out = ssh(inst, &run)?;
    if !out.status.success() {
        // PowerShell reports a script's failures on stdout as often as on stderr.
        let err = String::from_utf8_lossy(&out.stderr);
        let err = if err.trim().is_empty() {
            String::from_utf8_lossy(&out.stdout)
        } else {
            err
        };
        bail!("{what} on {} failed: {}", inst.os, err.trim());
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
    /// The desktop-control server agents drive, e.g. "cua-driver 0.30.3".
    #[serde(default)]
    pub desktop_server: String,
    /// Checksum of the Windows ISO it was built from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iso_sha256: Option<String>,
    /// Registry reference, when the image was pulled rather than built here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pulled_from: Option<String>,
    /// `guest_scripts_id` of the agentpc that last captured the snapshot.
    #[serde(default)]
    pub guest_scripts: String,
}

/// A fingerprint of the scripts a snapshot runs in an image's guest (`prepare.*`, and
/// `x86apps.sh` with its FEX patch on x86apps images), as embedded in this binary. Each
/// snapshot records it, so an image whose guest fixes predate this agentpc can say so.
pub fn guest_scripts_id(image: &Image) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    if image.x86_apps() {
        h.update(match image.os {
            Os::Arch => ARCH_X86_APPS,
            _ => UBUNTU_X86_APPS,
        });
        h.update(FEX_PATCH);
    }
    if image.os.is_linux() {
        for (name, body) in LINUX_HELPERS {
            h.update(name);
            h.update(body);
        }
    }
    h.update(match image.os {
        Os::Ubuntu => UBUNTU_PREPARE,
        Os::Arch => ARCH_PREPARE,
        Os::Windows => WIN_PREPARE,
    });
    h.finalize()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// When an image's snapshot was captured by an agentpc with other guest scripts (an older
/// one, usually: `agentpc update` doesn't touch images), what brings it up to date.
pub fn outdated(image: &Image) -> Option<String> {
    let info = read_info(image)?;
    if info.guest_scripts == guest_scripts_id(image) {
        return None;
    }
    Some(format!(
        "its guest setup predates this agentpc; `{} image snapshot {image}` applies the \
         current one (a few minutes; delete its VMs first)",
        crate::setup::cmd_name()
    ))
}

pub fn read_info(image: &Image) -> Option<ImageInfo> {
    serde_json::from_slice(&std::fs::read(image.info_file()).ok()?).ok()
}

pub fn write_info(image: &Image, info: &ImageInfo) -> Result<()> {
    std::fs::write(image.info_file(), serde_json::to_vec_pretty(info)?)?;
    Ok(())
}

/// The cua-driver release the guest setups install (`guests/ubuntu/user-data`,
/// `guests/arch/build.sh`, `guests/windows/oem/setup.ps1`); bump it with them.
pub const CUA_DRIVER_VERSION: &str = "0.30.3";

/// The version in a guest's desktop-server string ("cua-driver 0.30.3", "v0.30.3").
fn desktop_server_version(server: &str) -> Option<&str> {
    let v = server.split_whitespace().last()?;
    Some(v.strip_prefix('v').unwrap_or(v))
}

/// Fail an image build or snapshot whose guest doesn't run the pinned cua-driver: agents
/// and the MCP gateway are written against that release's tools.
fn check_desktop_server(image: &Image, server: &str) -> Result<()> {
    match desktop_server_version(server) {
        Some(v) if v == CUA_DRIVER_VERSION => Ok(()),
        found => bail!(
            "{image}: the guest runs {} but this agentpc needs cua-driver {CUA_DRIVER_VERSION}; \
             rebuild the image with: {} image build {image}",
            found.map_or("no cua-driver".to_string(), |v| format!("cua-driver {v}")),
            crate::setup::cmd_name()
        ),
    }
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
        // Arch Linux ARM's os-release has no VERSION_ID (it's rolling); BUILD_ID says so.
        Os::Arch => {
            r#". /etc/os-release
echo "version=$PRETTY_NAME"
echo "version_id=${VERSION_ID:-${BUILD_ID:-}}"
echo "arch=$(uname -m | sed 's/^aarch64$/arm64/')"
echo "server=$(~/.local/bin/cua-driver --version 2>/dev/null | awk '{print $NF}')""#
        }
        // ProductName still says "Windows 10" on Windows 11; the WMI caption doesn't.
        Os::Windows => {
            r#"$v = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion'
$cua = "$env:LOCALAPPDATA\Programs\Cua\cua-driver\bin\cua-driver.exe"
"caption=$((Get-CimInstance Win32_OperatingSystem).Caption -replace '^Microsoft ', '')"
"release=$($v.DisplayVersion)"
"build=$($v.CurrentBuild).$($v.UBR)"
"arch=$($env:PROCESSOR_ARCHITECTURE.ToLower())"
"server=$(if (Test-Path $cua) { & $cua --version })""#
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
        Os::Arch => {
            let id = get("version_id");
            let id = if id.is_empty() {
                crate::instance::ARCH_VERSION.to_string()
            } else {
                id
            };
            (
                get("version"),
                id,
                String::new(), // described by whoever made the disk; kept from the pulled info
                format!("cua-driver {server}"),
            )
        }
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
        guest_scripts: guest_scripts_id(&inst.image),
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
        if let Some(hint) = outdated(&image) {
            s += &format!("  {image}: {hint}\n");
        }
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
    // Held for the removal, so no build, pull or snapshot starts writing it meanwhile.
    let Some(_lock) = crate::instance::try_image_lock(image) else {
        bail!("a build, pull or snapshot of {image} is running; try again when it finishes");
    };
    if !image.instances()?.is_empty() {
        bail!("VMs of {image} depend on it; rm them first");
    }
    let busy = image.busy_builds();
    if !busy.is_empty() {
        bail!(
            "{image} is in use by a running build ({}); try again when it finishes",
            busy.join(", ")
        );
    }
    let files = [image.info_file(), image.disk(), image.vars()];
    if !files
        .iter()
        .chain(&image.snapshot_files())
        .any(|p| p.exists())
    {
        bail!("no {image} image");
    }
    for p in files {
        let _ = std::fs::remove_file(p);
    }
    image.remove_snapshot();
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
    // Every Windows image shares them, and builds of different images run in parallel.
    let _lock = crate::instance::lock(
        &cache_dir().join(".virtio.lock"),
        Some("waiting for another build's virtio driver download"),
    )?;
    if dir.is_dir() {
        return Ok(dir);
    }
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
    let version = inst.image.release();
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

/// Bytes a file occupies on disk (a qcow2 is sparse; its length overstates it).
fn allocated(p: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).map(|m| m.blocks() * 512).unwrap_or(0)
}

/// Wait up to 30 min for `need` free bytes, so a long build isn't thrown away when the
/// disk fills up (macOS can reclaim tens of GB for updates mid-build). Errs if it never frees.
fn wait_for_space(need: u64, what: &str) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(1800);
    let mut told = false;
    loop {
        let free = crate::instance::free_disk_bytes(&home()).unwrap_or(u64::MAX);
        if free >= need {
            return Ok(());
        }
        let (need_gb, free_gb) = (need.div_ceil(1 << 30), free >> 30);
        if std::time::Instant::now() > deadline {
            bail!("not enough disk space to {what}: need ~{need_gb} GB, {free_gb} GB free");
        }
        if !told {
            log!(
                "need ~{need_gb} GB free to {what}, only {free_gb} GB; free some space and it continues (waiting up to 30 min)"
            );
            told = true;
        }
        std::thread::sleep(Duration::from_secs(15));
    }
}

/// Freeze the cleanly shut-down build disk as the image (flattened, read-only).
fn promote_image(inst: &Instance) -> Result<()> {
    let image = &inst.image;
    qemu::stop(inst)?;
    log!("writing the {image} image");
    let (disk, vars) = (image.disk(), image.vars());
    let _ = std::fs::remove_file(&disk);
    let _ = std::fs::remove_file(&vars);
    // An old snapshot would resume the previous build if the new one fails.
    image.remove_snapshot();
    let tmp = disk.with_extension("qcow2.tmp");
    wait_for_space(
        allocated(&inst.disk()) + (1 << 30),
        &format!("write the {image} image"),
    )?;
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
    // The vars go in first, whole (a temp file renamed), then the disk: the image exists
    // once its disk does, so an interruption can't leave one with missing or cut-off vars.
    let vars_tmp = vars.with_extension("fd.tmp");
    std::fs::copy(inst.vars(), &vars_tmp)?;
    std::fs::rename(&vars_tmp, &vars)?;
    std::fs::rename(&tmp, &disk)?;
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
    #[test]
    fn guest_setups_install_the_pinned_cua_driver() {
        let tag = format!("cua-driver-rs-v{}", super::CUA_DRIVER_VERSION);
        for (file, text) in [
            ("guests/ubuntu/user-data", super::UBUNTU_USER_DATA),
            ("guests/arch/build.sh", super::ARCH_BUILD),
        ] {
            assert!(text.contains(&tag), "{file} doesn't install {tag}");
        }
        // setup.ps1 builds its URLs from one variable.
        let setup_ps1 = String::from_utf8_lossy(super::WIN_OEM[1].1).into_owned();
        let pin = format!("$cuaVersion = '{}'", super::CUA_DRIVER_VERSION);
        assert!(
            setup_ps1.contains(&pin),
            "guests/windows/oem/setup.ps1 doesn't pin {pin}"
        );
    }

    #[test]
    fn checks_the_desktop_server_version() {
        use super::{CUA_DRIVER_VERSION, check_desktop_server, desktop_server_version};
        assert_eq!(desktop_server_version("cua-driver 0.30.3"), Some("0.30.3"));
        assert_eq!(desktop_server_version("cua-driver v0.30.3"), Some("0.30.3"));
        assert_eq!(desktop_server_version("0.30.3"), Some("0.30.3"));
        assert_eq!(desktop_server_version(""), None);
        let image: crate::instance::Image = "ubuntu-24.04".parse().unwrap();
        let pinned = format!("cua-driver {CUA_DRIVER_VERSION}");
        assert!(check_desktop_server(&image, &pinned).is_ok());
        let err = check_desktop_server(&image, "cua-driver 0.29.0").unwrap_err();
        assert!(err.to_string().contains("cua-driver 0.29.0"));
        // A guest whose `--version` printed nothing has no driver at all.
        assert!(check_desktop_server(&image, "cua-driver ").is_err());
        assert!(check_desktop_server(&image, "").is_err());
    }

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

//! VM instances, images and the on-disk layout under `$AGENTPC_HOME` (default `~/.agentpc`).

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use serde::Serialize;

pub fn home() -> PathBuf {
    std::env::var_os("AGENTPC_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".agentpc")
        })
}

pub fn cache_dir() -> PathBuf {
    home().join("cache")
}

pub fn images_dir() -> PathBuf {
    home().join("images")
}

pub fn instances_dir() -> PathBuf {
    home().join("instances")
}

pub fn ssh_key() -> PathBuf {
    home().join("id_ed25519")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Os {
    Windows,
    Ubuntu,
    /// Arch Linux ARM: an XFCE desktop set up like the Ubuntu guest, but with pacman and
    /// Chromium.
    Arch,
}

/// Arch is a rolling release, so it has one version: a bare `arch` means `arch-rolling`.
pub const ARCH_VERSION: &str = "rolling";

impl Os {
    pub const ALL: [Os; 3] = [Os::Windows, Os::Ubuntu, Os::Arch];

    /// The version a bare `ubuntu`, `windows` or `arch` means: the newest release agentpc knows.
    pub fn default_version(self) -> &'static str {
        match self {
            Os::Windows => "11-25h2",
            Os::Ubuntu => "24.04",
            Os::Arch => ARCH_VERSION,
        }
    }

    /// A Linux guest: X11 desktop, bash over SSH, the SMBIOS SSH-key service and
    /// cua-driver in `~/.local/bin`.
    pub fn is_linux(self) -> bool {
        matches!(self, Os::Ubuntu | Os::Arch)
    }

    /// Memory (GB) and CPUs a VM gets unless created with others. Image snapshots are
    /// captured at this size, and a VM of another size can't resume them.
    pub fn default_size(self) -> (u32, u32) {
        match self {
            Os::Windows => (8, 4),
            Os::Ubuntu | Os::Arch => (4, 4),
        }
    }

    /// Seconds a cold boot may take before the desktop and its control server answer.
    pub fn boot_timeout(self) -> u64 {
        match self {
            Os::Windows => 300,
            Os::Ubuntu | Os::Arch => 180,
        }
    }
}

/// An OS image VMs are cloned from, named `<os>-<version>` (`ubuntu-24.04`,
/// `windows-11-25h2`). Several versions of an OS can be installed side by side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub os: Os,
    pub version: String,
}

/// The Ubuntu image variant that also runs x86_64 and i386 Linux programs (through FEX):
/// `ubuntu-24.04-x86apps`, or `ubuntu-x86apps` for the default release.
const X86_APPS: &str = "x86apps";

impl Image {
    pub fn new(os: Os, version: Option<&str>) -> Result<Self> {
        let mut version = version.unwrap_or(os.default_version()).to_ascii_lowercase();
        if os == Os::Windows && version == "11" {
            version = os.default_version().into();
        }
        if version == X86_APPS {
            version = format!("{}-{X86_APPS}", os.default_version());
        }
        if os == Os::Windows && version.ends_with(X86_APPS) {
            bail!("Windows runs x64 and x86 apps through Prism already; use a plain windows image");
        }
        if os == Os::Arch && version.ends_with(X86_APPS) {
            bail!("x86 apps are only available on ubuntu images (ubuntu-x86apps)");
        }
        // It becomes part of file names and registry tags.
        if version.is_empty()
            || !version
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        {
            bail!("invalid {os} version '{version}'");
        }
        Ok(Self { os, version })
    }

    /// What a user means by `name`. Like parsing it, except that a bare `windows` or
    /// `windows-11` whose default release isn't installed falls back to the newest installed
    /// Windows 11 image: building one takes 12+ minutes.
    pub fn resolve(name: &str) -> Result<Self> {
        let image: Self = name.parse()?;
        if matches!(name, "windows" | "windows-11") && !image.exists() {
            let newest = Self::all()
                .into_iter()
                .filter(|i| i.os == Os::Windows && i.version.starts_with("11-"))
                .max_by(|a, b| a.version.cmp(&b.version));
            if let Some(i) = newest {
                return Ok(i);
            }
        }
        Ok(image)
    }

    /// Whether VMs of this image run x86 Linux programs (`guests/ubuntu/x86apps.sh`).
    pub fn x86_apps(&self) -> bool {
        self.os == Os::Ubuntu && self.version.ends_with(&format!("-{X86_APPS}"))
    }

    /// The OS release the image installs: its version without a variant suffix.
    pub fn release(&self) -> &str {
        self.version
            .strip_suffix(&format!("-{X86_APPS}"))
            .unwrap_or(&self.version)
    }

    /// Installed images, sorted by name.
    pub fn all() -> Vec<Self> {
        let mut out: Vec<Self> = std::fs::read_dir(images_dir())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let stem = name.strip_suffix(".qcow2")?;
                if stem.ends_with(".snapshot") {
                    return None;
                }
                stem.parse().ok()
            })
            .collect();
        out.sort_by_key(|i| i.to_string());
        out
    }

    fn file(&self, suffix: &str) -> PathBuf {
        images_dir().join(format!("{self}{suffix}"))
    }

    pub fn disk(&self) -> PathBuf {
        self.file(".qcow2")
    }

    pub fn vars(&self) -> PathBuf {
        self.file(".vars.fd")
    }

    /// What the image contains (OS version, source), see `image::ImageInfo`.
    pub fn info_file(&self) -> PathBuf {
        self.file(".json")
    }

    /// The image as captured with the desktop already running, plus its saved RAM,
    /// so clones resume instead of booting.
    pub fn snapshot_disk(&self) -> PathBuf {
        self.file(".snapshot.qcow2")
    }

    pub fn snapshot_vars(&self) -> PathBuf {
        self.file(".snapshot.vars.fd")
    }

    pub fn snapshot_state(&self) -> PathBuf {
        self.file(".snapshot.state")
    }

    pub fn exists(&self) -> bool {
        self.disk().is_file()
    }

    pub fn has_snapshot(&self) -> bool {
        [
            self.snapshot_disk(),
            self.snapshot_vars(),
            self.snapshot_state(),
        ]
        .iter()
        .all(|p| p.is_file())
    }

    /// VMs cloned from this image.
    pub fn instances(&self) -> Result<Vec<Instance>> {
        Ok(Instance::list()?
            .into_iter()
            .filter(|i| &i.image == self)
            .collect())
    }
}

impl fmt::Display for Image {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(&format!("{}-{}", self.os, self.version))
    }
}

/// `ubuntu` (default version) or `ubuntu-22.04`; `windows-11` means the default Windows 11
/// release.
impl FromStr for Image {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.split_once('-') {
            Some((os, version)) => Self::new(os.parse()?, Some(version)),
            None => Self::new(s.parse()?, None),
        }
    }
}

impl fmt::Display for Os {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Os::Windows => "windows",
            Os::Ubuntu => "ubuntu",
            Os::Arch => "arch",
        })
    }
}

impl FromStr for Os {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "windows" => Ok(Os::Windows),
            "ubuntu" => Ok(Os::Ubuntu),
            "arch" => Ok(Os::Arch),
            _ => bail!("unknown os '{s}' (windows|ubuntu|arch)"),
        }
    }
}

/// Per-slot host ports. Kept in an uncommon 47xxx range so they don't collide with the
/// dev servers people run on 22/8000/5900. Slot n (1..=50) gets `<base>+n` in each band.
/// (47100+n was Windows-MCP's, for images built by agentpc 0.1.0; left unused so the other
/// bands keep their numbers.)
const SSH_BASE: u16 = 47000;
const WS_BASE: u16 = 47200;
const VNC_BASE: u16 = 47300;

/// The host ports a slot occupies, for slot-selection bind tests.
fn slot_host_ports(slot: u16) -> [u16; 3] {
    [SSH_BASE + slot, WS_BASE + slot, VNC_BASE + slot]
}

/// A VM instance. Its ports derive from its slot so several can run at once.
#[derive(Debug, Clone)]
pub struct Instance {
    pub name: String,
    pub os: Os,
    pub image: Image,
    pub slot: u16,
    pub dir: PathBuf,
}

impl Instance {
    pub fn load(name: &str) -> Result<Self> {
        let dir = instances_dir().join(name);
        if name.is_empty() || !dir.is_dir() {
            bail!("no instance '{name}' (see: agentpc list)");
        }
        // VMs from before versioned images only recorded their OS.
        let image: Image = match read_trimmed(&dir.join("image")) {
            Ok(i) => i.parse()?,
            Err(_) => Image::new(read_trimmed(&dir.join("os"))?.parse()?, None)?,
        };
        let slot = read_trimmed(&dir.join("slot"))?
            .parse()
            .context("bad slot file")?;
        Ok(Self {
            name: name.to_string(),
            os: image.os,
            image,
            slot,
            dir,
        })
    }

    pub fn create(name: &str, image: &Image, slot: u16) -> Result<Self> {
        let dir = instances_dir().join(name);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("image"), image.to_string())?;
        std::fs::write(dir.join("slot"), slot.to_string())?;
        Ok(Self {
            name: name.to_string(),
            os: image.os,
            image: image.clone(),
            slot,
            dir,
        })
    }

    /// A hidden build or snapshot VM (`_build-*`, `_snap-*`) of `image`, in a free slot so
    /// builds of different images can run side by side.
    pub fn create_scratch(name: &str, image: &Image) -> Result<Self> {
        let _lock = creation_lock()?;
        Self::create(name, image, Self::free_slot()?)
    }

    /// User-visible instances; `_build-*` build VMs are excluded.
    pub fn list() -> Result<Vec<Self>> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(instances_dir()) else {
            return Ok(out);
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if e.path().is_dir()
                && !name.starts_with('_')
                && let Ok(i) = Self::load(&name)
            {
                out.push(i);
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Memory (GB) and CPUs, from `agentpc create --memory/--cpus` or the OS default.
    pub fn size(&self) -> (u32, u32) {
        let (mem, cpus) = self.os.default_size();
        let read = |f: &str, d: u32| {
            read_trimmed(&self.dir.join(f))
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d)
        };
        (read("memory", mem), read("cpus", cpus))
    }

    /// Whether the VM is cut off from the internet and this Mac (`create --offline`). SSH,
    /// the desktop server and forwarded ports still reach it.
    pub fn offline(&self) -> bool {
        self.dir.join("offline").exists()
    }

    /// Record a non-default size (defaults leave no file, so older VMs keep theirs).
    pub fn set_size(&self, memory: Option<u32>, cpus: Option<u32>) -> Result<()> {
        if let Some(m) = memory {
            if !(2..=64).contains(&m) {
                bail!("memory must be 2-64 GB");
            }
            std::fs::write(self.dir.join("memory"), m.to_string())?;
        }
        if let Some(c) = cpus {
            if !(1..=16).contains(&c) {
                bail!("cpus must be 1-16");
            }
            std::fs::write(self.dir.join("cpus"), c.to_string())?;
        }
        Ok(())
    }

    /// A slot no VM uses, build and snapshot VMs included. Call it under `creation_lock`.
    pub fn free_slot() -> Result<u16> {
        let used: Vec<u16> = std::fs::read_dir(instances_dir())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| Self::load(&e.file_name().to_string_lossy()).ok())
            .map(|i| i.slot)
            .collect();
        (1..=50)
            .filter(|s| !used.contains(s))
            // Skip a slot whose host ports something else is already bound to.
            .find(|s| slot_host_ports(*s).iter().all(|p| port_free(*p)))
            .context("no free slot (all taken or their ports are busy)")
    }

    pub fn ssh_port(&self) -> u16 {
        SSH_BASE + self.slot
    }
    /// Actual VNC port (QEMU adds 5900 to the display number).
    pub fn vnc_port(&self) -> u16 {
        VNC_BASE + self.slot
    }
    pub fn vnc_display(&self) -> u16 {
        self.vnc_port() - 5900
    }
    pub fn ws_port(&self) -> u16 {
        WS_BASE + self.slot
    }

    pub fn disk(&self) -> PathBuf {
        self.dir.join("disk.qcow2")
    }
    pub fn vars(&self) -> PathBuf {
        self.dir.join("vars.fd")
    }
    /// Kept under /tmp: a unix socket path must stay under 104 bytes, which a
    /// long `$AGENTPC_HOME` would exceed. The hash is a fixed algorithm (FNV-1a),
    /// not `DefaultHasher`, so the path is stable across builds and processes.
    pub fn qmp_socket(&self) -> PathBuf {
        let hash = fnv1a(self.dir.to_string_lossy().as_bytes());
        runtime_dir().join(format!("{hash:016x}.qmp"))
    }
    /// Present while the clone's next boot should resume the snapshot.
    pub fn resume_marker(&self) -> PathBuf {
        self.dir.join("resume")
    }

    /// Whether the disk is backed by the snapshot disk (vs the plain image).
    pub fn on_snapshot_base(&self) -> bool {
        read_trimmed(&self.dir.join("base")).is_ok_and(|b| b == "snapshot")
    }

    pub fn pid_file(&self) -> PathBuf {
        self.dir.join("qemu.pid")
    }

    /// Our QEMU's pid, or `None` if the pid file is missing or stale. A stale file (the
    /// process is gone, or the pid was recycled by something that isn't our QEMU) is
    /// removed, so an old pid can't make `running()` report a VM that isn't there.
    pub fn pid(&self) -> Option<i32> {
        let pid: i32 = read_trimmed(&self.pid_file()).ok()?.parse().ok()?;
        // SAFETY: signal 0 only checks that the process exists.
        let alive = unsafe { libc_kill(pid, 0) } == 0;
        if alive && self.pid_is_our_qemu(pid) {
            return Some(pid);
        }
        let _ = std::fs::remove_file(self.pid_file());
        None
    }

    /// Confirm `pid` is a QEMU process launched for this instance (its argv names this
    /// instance's dir), guarding signals against a recycled pid.
    fn pid_is_our_qemu(&self, pid: i32) -> bool {
        let Ok(out) = std::process::Command::new("ps")
            .args(["-ww", "-o", "command=", "-p", &pid.to_string()])
            .output()
        else {
            return false;
        };
        let cmd = String::from_utf8_lossy(&out.stdout);
        cmd.contains("qemu-system")
            && (cmd.contains(&*self.dir.to_string_lossy())
                || cmd.contains(&*self.qmp_socket().to_string_lossy()))
    }

    pub fn running(&self) -> bool {
        self.pid().is_some()
    }

    /// Held for the length of a lifecycle op (start/stop/reset/…) so two of them can't
    /// race on this instance's disk. Same helper as `creation_lock`; released on drop.
    pub fn lock(&self) -> Result<std::fs::File> {
        lock(
            &self.dir.join(".lock"),
            Some("waiting for another operation on this VM to finish"),
        )
    }

    /// This VM's VNC password file (0600), created with a fresh password if absent.
    /// Kept out of logs; only the viewer URL carries it.
    pub fn ensure_vnc_password(&self) -> Result<String> {
        if let Ok(p) = read_trimmed(&self.vnc_password_file())
            && !p.is_empty()
        {
            return Ok(p);
        }
        // VNC auth only uses 8 chars, so make each one count: 8 alphanumerics is ~47 bits.
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        let mut buf = [0u8; 64];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
            .context("read /dev/urandom")?;
        // Rejection sampling (bytes >= 248 skipped) keeps the pick unbiased.
        let pass: String = buf
            .iter()
            .filter(|&&b| b < 248)
            .take(8)
            .map(|&b| ALPHABET[b as usize % ALPHABET.len()] as char)
            .collect();
        anyhow::ensure!(pass.len() == 8, "not enough randomness for a VNC password");
        write_private(&self.vnc_password_file(), &pass)?;
        Ok(pass)
    }

    /// The stored VNC password if one exists (no side effects), for building the viewer URL.
    pub fn vnc_password(&self) -> Option<String> {
        read_trimmed(&self.vnc_password_file())
            .ok()
            .filter(|p| !p.is_empty())
    }

    fn vnc_password_file(&self) -> PathBuf {
        self.dir.join("vnc-pass")
    }
}

const IMAGE_FILES: [&str; 6] = [
    ".qcow2",
    ".vars.fd",
    ".json",
    ".snapshot.qcow2",
    ".snapshot.vars.fd",
    ".snapshot.state",
];

/// Held while a VM claims its slot and name, so parallel creates get different ones.
/// Released when the returned file is dropped.
pub fn creation_lock() -> Result<std::fs::File> {
    lock(&instances_dir().join(".lock"), None)
}

/// Held while one image is built, downloaded or snapshotted, since those write its files.
/// A second one of the same image waits; other images proceed in parallel. Not reentrant:
/// take it once per command.
pub fn image_lock(image: &Image) -> Result<std::fs::File> {
    lock(
        &image_lock_path(image),
        Some(&format!(
            "waiting for another build or download of {image} to finish"
        )),
    )
}

fn image_lock_path(image: &Image) -> PathBuf {
    images_dir().join(format!(".{image}.lock"))
}

/// `image`'s lock if it's free right now, else None (a build, pull or snapshot holds it).
/// Never blocks. Used by `clean` to tell a crashed build's leftovers from a live build.
pub fn try_image_lock(image: &Image) -> Option<std::fs::File> {
    use std::os::fd::AsRawFd;
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    let path = image_lock_path(image);
    std::fs::create_dir_all(images_dir()).ok()?;
    let f = std::fs::File::create(&path).ok()?;
    // SAFETY: flock(2) on a descriptor we own.
    (unsafe { flock(f.as_raw_fd(), LOCK_EX | LOCK_NB) } == 0).then_some(f)
}

pub(crate) fn lock(path: &Path, waiting: Option<&str>) -> Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let f = std::fs::File::create(path)?;
    // SAFETY: flock(2) on a descriptor we own.
    if unsafe { flock(f.as_raw_fd(), LOCK_EX | LOCK_NB) } != 0 {
        if let Some(msg) = waiting {
            crate::log!("{msg}");
        }
        // SAFETY: as above.
        if unsafe { flock(f.as_raw_fd(), LOCK_EX) } != 0 {
            bail!(
                "lock {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            );
        }
    }
    Ok(f)
}

/// Rename images from before their names carried the full version, and repoint their clones:
/// `ubuntu.qcow2` → `ubuntu-24.04.qcow2`, and `windows.qcow2` or `windows-11.qcow2` →
/// `windows-11-<release>.qcow2`. The release comes from the image's recorded info. Waits
/// while such a clone is running: its disk is locked.
pub fn migrate_legacy_images() -> Result<()> {
    for stem in ["ubuntu", "windows", "windows-11"] {
        let os: Os = stem.split('-').next().unwrap_or(stem).parse()?;
        let old = |suffix: &str| images_dir().join(format!("{stem}{suffix}"));
        if !old(".qcow2").is_file() {
            continue;
        }
        let version = std::fs::read(old(".json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| v["version_id"].as_str().map(str::to_ascii_lowercase));
        let Some(version) = version else { continue };
        let image = Image::new(os, Some(&version))?;
        if image.exists() {
            continue;
        }
        // Clones name their image in an `image` file; the oldest ones only have `os`.
        let clones: Vec<Instance> = std::fs::read_dir(instances_dir())
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| match read_trimmed(&e.path().join("image")) {
                Ok(name) => name == stem,
                Err(_) => read_trimmed(&e.path().join("os")).is_ok_and(|o| o == stem),
            })
            .filter_map(|e| Instance::load(&e.file_name().to_string_lossy()).ok())
            .collect();
        if clones.iter().any(|i| i.running()) {
            continue;
        }
        for suffix in IMAGE_FILES {
            if old(suffix).exists() {
                std::fs::rename(old(suffix), image.file(suffix))?;
            }
        }
        for inst in clones {
            let base = if inst.on_snapshot_base() {
                image.snapshot_disk()
            } else {
                image.disk()
            };
            let base = format!(
                "../../images/{}",
                base.file_name().unwrap().to_string_lossy()
            );
            let st = std::process::Command::new("qemu-img")
                .args(["rebase", "-u", "-F", "qcow2", "-b", &base])
                .arg(inst.disk())
                .status()
                .context("run qemu-img")?;
            if !st.success() {
                bail!("repointing {} at {image} failed", inst.name);
            }
            std::fs::write(inst.dir.join("image"), image.to_string())?;
        }
        crate::log!("renamed the {stem} image to {image}");
    }
    Ok(())
}

unsafe extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
    fn getuid() -> u32;
    fn flock(fd: i32, op: i32) -> i32;
    fn localtime_r(t: *const i64, out: *mut Tm) -> *mut Tm;
}

/// Leading fields of libc `struct tm`; the buffer is oversized for the rest.
#[repr(C)]
struct Tm {
    sec: i32,
    min: i32,
    hour: i32,
    mday: i32,
    mon: i32,
    year: i32,
    _rest: [u8; 64],
}

fn local_tm() -> Tm {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let mut tm = Tm {
        sec: 0,
        min: 0,
        hour: 0,
        mday: 0,
        mon: 0,
        year: 0,
        _rest: [0; 64],
    };
    // SAFETY: both pointers are valid for the call; tm is large enough for struct tm.
    unsafe { localtime_r(&now, &mut tm) };
    tm
}

/// Local date as YYYYMMDD, for image tags.
pub fn local_date() -> String {
    let tm = local_tm();
    format!("{}{:02}{:02}", tm.year + 1900, tm.mon + 1, tm.mday)
}

/// Local wall-clock time as HH:MM:SS, for log lines.
pub fn local_hms() -> String {
    let tm = local_tm();
    format!("{:02}:{:02}:{:02}", tm.hour, tm.min, tm.sec)
}

pub fn kill(pid: i32, sig: i32) {
    // SAFETY: plain kill(2) on a pid we own.
    unsafe { libc_kill(pid, sig) };
}

pub fn read_trimmed(p: &Path) -> Result<String> {
    Ok(std::fs::read_to_string(p)
        .with_context(|| format!("read {}", p.display()))?
        .trim()
        .to_string())
}

/// FNV-1a (64-bit) over `bytes`: a fixed, process-independent hash for the QMP socket name.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// `/tmp/agentpc-<uid>`, kept 0700. It's short enough for macOS's 104-byte socket paths. A
/// pre-existing path that is a symlink, owned by someone else, or group/world-writable could
/// let another user hijack the QMP socket, so we fall back to `$AGENTPC_HOME/run` rather than
/// trust it.
fn runtime_dir() -> PathBuf {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    // SAFETY: getuid(2) has no preconditions.
    let uid = unsafe { getuid() };
    let dir = PathBuf::from(format!("/tmp/agentpc-{uid}"));
    let _ = std::fs::DirBuilder::new().mode(0o700).create(&dir);
    let trusted = std::fs::symlink_metadata(&dir)
        .is_ok_and(|m| m.is_dir() && m.uid() == uid && m.mode() & 0o022 == 0);
    if trusted {
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        return dir;
    }
    let fallback = home().join("run");
    let _ = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&fallback);
    fallback
}

/// Write `contents` to `path` with 0600 permissions (owner-only), for the VNC password.
fn write_private(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("write {}", path.display()))?;
    f.write_all(contents.as_bytes())?;
    Ok(())
}

/// True if nothing is bound to `port` on 127.0.0.1 (used to skip busy slots).
fn port_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Free space in bytes on the volume holding `path`, via `df -k`. `None` if it can't be read.
pub fn free_disk_bytes(path: &Path) -> Option<u64> {
    let out = std::process::Command::new("df")
        .args(["-k", &path.to_string_lossy()])
        .output()
        .ok()?;
    // Last line, "Avail" column (4th field), in 1024-byte blocks.
    let line = String::from_utf8_lossy(&out.stdout);
    let avail_kb: u64 = line
        .lines()
        .nth(1)?
        .split_whitespace()
        .nth(3)?
        .parse()
        .ok()?;
    Some(avail_kb * 1024)
}

/// Total physical RAM in bytes (`sysctl hw.memsize`). `None` if it can't be read.
pub fn host_mem_bytes() -> Option<u64> {
    let out = std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// The Mac's IANA time zone (e.g. `Asia/Kathmandu`), from `/etc/localtime`'s target.
pub fn mac_timezone() -> Option<String> {
    let target = std::fs::read_link("/etc/localtime").ok()?;
    let s = target.to_string_lossy();
    s.split_once("zoneinfo/").map(|(_, tz)| tz.to_string())
}

#[cfg(test)]
mod tests {
    use super::{Image, Os, fnv1a, slot_host_ports};

    #[test]
    fn fnv1a_matches_known_vectors() {
        // Canonical FNV-1a 64-bit test vectors.
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn slot_ports_dont_overlap_across_slots() {
        // No port a slot uses may collide with any port another slot uses (1..=50).
        let mut seen = std::collections::HashSet::new();
        for s in 1..=50 {
            for p in slot_host_ports(s) {
                assert!(seen.insert(p), "port {p} reused");
            }
        }
    }

    #[test]
    fn parses_image_names() {
        let i: Image = "ubuntu".parse().unwrap();
        assert_eq!((i.os, i.to_string()), (Os::Ubuntu, "ubuntu-24.04".into()));
        let i: Image = "windows-11-23h2".parse().unwrap();
        assert_eq!((i.os, i.version.as_str()), (Os::Windows, "11-23h2"));
        for bare in ["windows", "windows-11"] {
            assert_eq!(
                bare.parse::<Image>().unwrap().to_string(),
                "windows-11-25h2"
            );
        }
        assert_eq!(
            "Ubuntu-22.04".parse::<Image>().err().map(|e| e.to_string()),
            Some("unknown os 'Ubuntu' (windows|ubuntu|arch)".into())
        );
        assert!("ubuntu-../x".parse::<Image>().is_err());
        assert!("macos".parse::<Image>().is_err());

        let i: Image = "ubuntu-x86apps".parse().unwrap();
        assert_eq!(i.to_string(), "ubuntu-24.04-x86apps");
        assert!(i.x86_apps());
        assert_eq!(i.release(), "24.04");
        let i: Image = "ubuntu-22.04-x86apps".parse().unwrap();
        assert_eq!((i.x86_apps(), i.release()), (true, "22.04"));
        let i: Image = "ubuntu-22.04".parse().unwrap();
        assert_eq!((i.x86_apps(), i.release()), (false, "22.04"));
        assert!("windows-x86apps".parse::<Image>().is_err());

        let i: Image = "arch".parse().unwrap();
        assert_eq!((i.os, i.to_string()), (Os::Arch, "arch-rolling".into()));
        assert_eq!("arch-rolling".parse::<Image>().unwrap(), i);
        assert!(!i.x86_apps());
        assert!("arch-x86apps".parse::<Image>().is_err());
        assert!("arch-rolling-x86apps".parse::<Image>().is_err());
    }
}

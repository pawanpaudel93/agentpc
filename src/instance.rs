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
}

impl Os {
    pub const ALL: [Os; 2] = [Os::Windows, Os::Ubuntu];

    /// The version a bare `ubuntu` or `windows` means.
    pub fn default_version(self) -> &'static str {
        match self {
            Os::Windows => "11",
            Os::Ubuntu => "24.04",
        }
    }

    /// Seconds a cold boot may take before the desktop and its control server answer.
    pub fn boot_timeout(self) -> u64 {
        match self {
            Os::Windows => 300,
            Os::Ubuntu => 180,
        }
    }
}

/// An OS image VMs are cloned from, named `<os>-<version>` (`ubuntu-24.04`, `windows-11`).
/// Several versions of an OS can be installed side by side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub os: Os,
    pub version: String,
}

impl Image {
    pub fn new(os: Os, version: Option<&str>) -> Result<Self> {
        let version = version.unwrap_or(os.default_version()).to_ascii_lowercase();
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

/// `ubuntu` (default version) or `ubuntu-22.04`.
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
        })
    }
}

impl FromStr for Os {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "windows" => Ok(Os::Windows),
            "ubuntu" => Ok(Os::Ubuntu),
            _ => bail!("unknown os '{s}' (windows|ubuntu)"),
        }
    }
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

    pub fn free_slot() -> Result<u16> {
        let used: Vec<u16> = Self::list()?.iter().map(|i| i.slot).collect();
        (1..=50)
            .find(|s| !used.contains(s))
            .context("no free slot (50 instances max)")
    }

    pub fn ssh_port(&self) -> u16 {
        2200 + self.slot
    }
    pub fn mcp_port(&self) -> u16 {
        8000 + self.slot
    }
    pub fn vnc_display(&self) -> u16 {
        10 + self.slot
    }
    pub fn ws_port(&self) -> u16 {
        5700 + self.slot
    }

    pub fn disk(&self) -> PathBuf {
        self.dir.join("disk.qcow2")
    }
    pub fn vars(&self) -> PathBuf {
        self.dir.join("vars.fd")
    }
    /// Kept under /tmp: a unix socket path must stay under 104 bytes, which a
    /// long `$AGENTPC_HOME` would exceed.
    pub fn qmp_socket(&self) -> PathBuf {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.dir.hash(&mut h);
        // SAFETY: getuid(2) has no preconditions.
        let dir = PathBuf::from(format!("/tmp/agentpc-{}", unsafe { getuid() }));
        let _ = std::fs::create_dir_all(&dir);
        dir.join(format!("{:016x}.qmp", h.finish()))
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

    pub fn pid(&self) -> Option<i32> {
        let pid: i32 = read_trimmed(&self.pid_file()).ok()?.parse().ok()?;
        // SAFETY: signal 0 only checks that the process exists.
        (unsafe { libc_kill(pid, 0) } == 0).then_some(pid)
    }

    pub fn running(&self) -> bool {
        self.pid().is_some()
    }
}

/// Rename images from before versions were tracked (`ubuntu.qcow2` → `ubuntu-24.04.qcow2`)
/// and repoint their clones. Waits while such a clone is running: its disk is locked.
pub fn migrate_legacy_images() -> Result<()> {
    for os in Os::ALL {
        let legacy = |suffix: &str| images_dir().join(format!("{os}{suffix}"));
        if !legacy(".qcow2").is_file() {
            continue;
        }
        let clones: Vec<Instance> = std::fs::read_dir(instances_dir())
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| !e.path().join("image").exists())
            .filter_map(|e| Instance::load(&e.file_name().to_string_lossy()).ok())
            .filter(|i| i.os == os)
            .collect();
        if clones.iter().any(|i| i.running()) {
            continue;
        }
        // Builds before this recorded the Ubuntu release; Windows ones were all Windows 11.
        let version = std::fs::read(legacy(".json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| v["version_id"].as_str().map(String::from))
            .filter(|_| os == Os::Ubuntu);
        let image = Image::new(os, version.as_deref())?;
        if image.exists() {
            continue;
        }
        for suffix in [
            ".qcow2",
            ".vars.fd",
            ".json",
            ".snapshot.qcow2",
            ".snapshot.vars.fd",
            ".snapshot.state",
        ] {
            if legacy(suffix).exists() {
                std::fs::rename(legacy(suffix), image.file(suffix))?;
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
        crate::log!("renamed the {os} image to {image}");
    }
    Ok(())
}

unsafe extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
    fn getuid() -> u32;
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

#[cfg(test)]
mod tests {
    use super::{Image, Os};

    #[test]
    fn parses_image_names() {
        let i: Image = "ubuntu".parse().unwrap();
        assert_eq!((i.os, i.to_string()), (Os::Ubuntu, "ubuntu-24.04".into()));
        let i: Image = "windows-11-23h2".parse().unwrap();
        assert_eq!((i.os, i.version.as_str()), (Os::Windows, "11-23h2"));
        assert_eq!(
            "Ubuntu-22.04".parse::<Image>().err().map(|e| e.to_string()),
            Some("unknown os 'Ubuntu' (windows|ubuntu)".into())
        );
        assert!("ubuntu-../x".parse::<Image>().is_err());
        assert!("macos".parse::<Image>().is_err());
    }
}

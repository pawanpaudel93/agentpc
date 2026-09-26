//! Instances, golden images and the on-disk layout under `$AGENTPC_HOME` (default `~/.agentpc`).

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

pub fn golden_dir() -> PathBuf {
    home().join("golden")
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

    pub fn golden_disk(self) -> PathBuf {
        golden_dir().join(format!("{self}.qcow2"))
    }

    pub fn golden_vars(self) -> PathBuf {
        golden_dir().join(format!("{self}.vars.fd"))
    }

    /// Golden image captured with the desktop already running, plus its saved RAM,
    /// so clones resume instead of booting.
    pub fn live_disk(self) -> PathBuf {
        golden_dir().join(format!("{self}-live.qcow2"))
    }

    pub fn live_vars(self) -> PathBuf {
        golden_dir().join(format!("{self}-live.vars.fd"))
    }

    pub fn live_state(self) -> PathBuf {
        golden_dir().join(format!("{self}-live.state"))
    }

    pub fn has_live(self) -> bool {
        [self.live_disk(), self.live_vars(), self.live_state()]
            .iter()
            .all(|p| p.is_file())
    }

    /// Seconds a cold boot may take before the desktop and its control server answer.
    pub fn boot_timeout(self) -> u64 {
        match self {
            Os::Windows => 300,
            Os::Ubuntu => 180,
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
    pub slot: u16,
    pub dir: PathBuf,
}

impl Instance {
    pub fn load(name: &str) -> Result<Self> {
        let dir = instances_dir().join(name);
        if name.is_empty() || !dir.is_dir() {
            bail!("no instance '{name}' (see: agentpc list)");
        }
        let os = read_trimmed(&dir.join("os"))?.parse()?;
        let slot = read_trimmed(&dir.join("slot"))?
            .parse()
            .context("bad slot file")?;
        Ok(Self {
            name: name.to_string(),
            os,
            slot,
            dir,
        })
    }

    pub fn create(name: &str, os: Os, slot: u16) -> Result<Self> {
        let dir = instances_dir().join(name);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("os"), os.to_string())?;
        std::fs::write(dir.join("slot"), slot.to_string())?;
        Ok(Self {
            name: name.to_string(),
            os,
            slot,
            dir,
        })
    }

    /// User-visible instances; `_bake-*` build VMs are excluded.
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
    /// Present while the clone's next boot should resume the live snapshot.
    pub fn resume_marker(&self) -> PathBuf {
        self.dir.join("resume")
    }

    /// Whether the disk is backed by the live golden image (vs the cold one).
    pub fn on_live_base(&self) -> bool {
        read_trimmed(&self.dir.join("base")).is_ok_and(|b| b == "live")
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
    _rest: [u8; 64],
}

/// Local wall-clock time as HH:MM:SS, for log lines.
pub fn local_hms() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let mut tm = Tm {
        sec: 0,
        min: 0,
        hour: 0,
        _rest: [0; 64],
    };
    // SAFETY: both pointers are valid for the call; tm is large enough for struct tm.
    unsafe { localtime_r(&now, &mut tm) };
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

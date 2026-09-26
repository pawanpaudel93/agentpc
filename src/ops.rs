//! Instance lifecycle shared by the CLI and the MCP gateway.

use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::instance::{Instance, Os, ssh_key};
use crate::{log, qemu, viewer};

/// Session env for cua-driver: the Ubuntu autologin X session and its AT-SPI bus.
pub const UBUNTU_SESSION_ENV: &str = "DISPLAY=:0 XAUTHORITY=/home/agent/.Xauthority \
     DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus";

pub const SSH_OPTS: [&str; 10] = [
    "-o",
    "StrictHostKeyChecking=no",
    "-o",
    "UserKnownHostsFile=/dev/null",
    "-o",
    "LogLevel=ERROR",
    "-o",
    "ConnectTimeout=5",
    "-o",
    "BatchMode=yes",
];

/// `ssh` argv (without the program) that runs `remote` in the instance.
pub fn ssh_args(inst: &Instance, remote: &str) -> Vec<String> {
    let mut a = vec![
        "-i".to_string(),
        ssh_key().display().to_string(),
        "-p".into(),
        inst.ssh_port().to_string(),
    ];
    a.extend(SSH_OPTS.iter().map(|s| s.to_string()));
    a.push("agent@127.0.0.1".into());
    a.push(remote.into());
    a
}

pub fn ssh(inst: &Instance, remote: &str) -> Result<Output> {
    Command::new("ssh")
        .args(ssh_args(inst, remote))
        .output()
        .context("run ssh")
}

/// Desktop session up and its control server answering.
pub fn ready(inst: &Instance) -> bool {
    match inst.os {
        Os::Windows => {
            ssh(inst, r"Test-Path C:\OEM\done.txt")
                .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("True"))
                && http_responds(inst.mcp_port())
        }
        Os::Ubuntu => ssh(
            inst,
            &format!(
                "test -f /var/lib/cloud/agent-ready && pgrep -x xfce4-session >/dev/null && \
                 {UBUNTU_SESSION_ENV} ~/.local/bin/cua-driver --version"
            ),
        )
        .is_ok_and(|o| o.status.success()),
    }
}

/// Any HTTP reply at all (Windows-MCP answers a bare GET with 4xx).
fn http_responds(port: u16) -> bool {
    use std::io::{Read, Write};
    let Ok(mut s) = std::net::TcpStream::connect_timeout(
        &([127, 0, 0, 1], port).into(),
        Duration::from_secs(2),
    ) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(3)));
    let mut buf = [0u8; 12];
    s.write_all(b"GET /mcp HTTP/1.0\r\n\r\n").is_ok()
        && s.read(&mut buf)
            .is_ok_and(|n| n > 0 && buf.starts_with(b"HTTP/"))
}

pub fn wait_ready(inst: &Instance, timeout: Duration) -> Result<Duration> {
    let start = Instant::now();
    while !ready(inst) {
        if !inst.running() {
            bail!(
                "{}: qemu exited (see {})",
                inst.name,
                inst.dir.join("serial.log").display()
            );
        }
        if start.elapsed() > timeout {
            bail!("{}: not ready after {}s", inst.name, timeout.as_secs());
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    Ok(start.elapsed())
}

/// Copy-on-write clone of the golden image, preferring the live one so the first
/// boot resumes in about a second. The backing path is relative so `$AGENTPC_HOME`
/// can move.
pub fn clone_disk(inst: &Instance) -> Result<()> {
    let live = inst.os.has_live();
    let (base, vars) = if live {
        (inst.os.live_disk(), inst.os.live_vars())
    } else {
        (inst.os.golden_disk(), inst.os.golden_vars())
    };
    let base_name = base.file_name().unwrap().to_string_lossy();
    let _ = std::fs::remove_file(inst.disk());
    run(
        "qemu-img",
        &[
            "create",
            "-q",
            "-f",
            "qcow2",
            "-b",
            &format!("../../golden/{base_name}"),
            "-F",
            "qcow2",
            &inst.disk().to_string_lossy(),
        ],
    )?;
    let _ = std::fs::remove_file(inst.vars());
    std::fs::copy(vars, inst.vars())?;
    set_writable(&inst.vars())?; // golden copies are read-only
    std::fs::write(inst.dir.join("base"), if live { "live" } else { "cold" })?;
    if live {
        std::fs::write(inst.resume_marker(), "")?;
    } else {
        let _ = std::fs::remove_file(inst.resume_marker());
    }
    Ok(())
}

pub fn set_writable(p: &Path) -> Result<()> {
    let mut perm = std::fs::metadata(p)?.permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perm.set_readonly(false);
    std::fs::set_permissions(p, perm)?;
    Ok(())
}

pub fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let st = Command::new(cmd)
        .args(args)
        .status()
        .with_context(|| format!("run {cmd}"))?;
    if !st.success() {
        bail!("{cmd} {} failed ({st})", args.join(" "));
    }
    Ok(())
}

pub fn info(inst: &Instance) -> String {
    format!(
        "{} ({}): viewer {}\n  ssh: agentpc ssh {}    vnc: vnc://127.0.0.1:{}    login: agent/agent",
        inst.name,
        inst.os,
        viewer::url(inst),
        inst.name,
        5900 + inst.vnc_display()
    )
}

pub fn create(os: Os, name: Option<&str>) -> Result<String> {
    if !os.golden_disk().is_file() {
        bail!("no golden image for {os}; run: agentpc bake {os}");
    }
    let slot = Instance::free_slot()?;
    let name = match name {
        Some(n) => n.to_string(),
        None => (1..)
            .map(|i| format!("{os}-{i}"))
            .find(|n| !crate::instance::instances_dir().join(n).exists())
            .unwrap(),
    };
    if name.is_empty() || name.starts_with(['_', '.']) || name.contains('/') {
        bail!("invalid instance name '{name}'");
    }
    if crate::instance::instances_dir().join(&name).exists() {
        bail!("instance '{name}' exists");
    }
    let inst = Instance::create(&name, os, slot)?;
    clone_disk(&inst)?;
    boot(&inst)
}

/// Start a stopped instance and wait until it's usable.
pub fn boot(inst: &Instance) -> Result<String> {
    let mut resumed = false;
    if !inst.running() {
        // Only a clone's first boot can resume: afterwards its disk has moved on
        // from the saved RAM, so later starts are cold boots.
        if inst.resume_marker().exists() && inst.os.has_live() {
            let _ = std::fs::remove_file(inst.resume_marker());
            match qemu::start_resumed(inst, &inst.os.live_state()) {
                Ok(()) => resumed = true,
                Err(e) => {
                    log!("{}: resume failed ({e:#}); booting instead", inst.name);
                    qemu::quit(inst);
                    qemu::start(inst, &[])?;
                }
            }
        } else {
            qemu::start(inst, &[])?;
        }
    }
    viewer::ensure_running()?;
    let took = wait_ready(inst, Duration::from_secs(inst.os.boot_timeout()))?;
    if resumed && inst.os == Os::Windows {
        sync_clock(inst);
    }
    log!("{} ready in {:.1}s", inst.name, took.as_secs_f32());
    Ok(info(inst))
}

/// A resumed Windows guest keeps the clock it had when the snapshot was taken
/// (Linux reads the host-backed arch timer and needs no fix).
fn sync_clock(inst: &Instance) {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let cmd = format!(
        "Set-Date -Date ([DateTimeOffset]::FromUnixTimeMilliseconds({ms}).LocalDateTime) | Out-Null"
    );
    if let Err(e) = ssh(inst, &cmd).and_then(|o| {
        o.status
            .success()
            .then_some(())
            .ok_or_else(|| anyhow::anyhow!(String::from_utf8_lossy(&o.stderr).trim().to_string()))
    }) {
        log!("{}: clock sync failed: {e:#}", inst.name);
    }
}

pub fn stop(inst: &Instance) -> Result<String> {
    qemu::stop(inst)?;
    viewer::stop_if_idle();
    Ok(format!("{} stopped", inst.name))
}

// reset and delete discard the disk, so a clean guest shutdown would be wasted time.
pub fn reset(inst: &Instance) -> Result<String> {
    qemu::quit(inst);
    clone_disk(inst)?;
    boot(inst)
}

pub fn delete(inst: &Instance) -> Result<String> {
    qemu::quit(inst);
    std::fs::remove_dir_all(&inst.dir)?;
    viewer::stop_if_idle();
    Ok(format!("removed {}", inst.name))
}

pub fn list_table() -> Result<String> {
    let mut s = format!(
        "{:<14} {:<8} {:<5} {:<8} {}\n",
        "NAME", "OS", "SLOT", "STATE", "VIEWER"
    );
    for i in Instance::list()? {
        let state = if i.running() { "running" } else { "stopped" };
        s += &format!(
            "{:<14} {:<8} {:<5} {:<8} {}\n",
            i.name,
            i.os,
            i.slot,
            state,
            viewer::url(&i)
        );
    }
    for os in Os::ALL {
        if let Ok(m) = std::fs::metadata(os.golden_disk()) {
            let live = if os.has_live() {
                "live snapshot: yes"
            } else {
                "live snapshot: no (agentpc snapshot)"
            };
            s += &format!("golden: {os} ({:.1} GB, {live})\n", m.len() as f64 / 1e9);
        }
    }
    Ok(s)
}

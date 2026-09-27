//! Instance lifecycle shared by the CLI and the MCP gateway.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::instance::{Image, Instance, Os, ssh_key};
use crate::{log, qemu, viewer};

/// Session env for cua-driver: the Ubuntu autologin X session and its AT-SPI bus.
pub const UBUNTU_SESSION_ENV: &str = "DISPLAY=:0 XAUTHORITY=/home/agent/.Xauthority \
     XDG_RUNTIME_DIR=/run/user/1000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus";

pub const SSH_OPTS: [&str; 12] = [
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
    // Otherwise keys in the user's ssh-agent are offered first and can exhaust the server's
    // MaxAuthTries before ours is tried.
    "-o",
    "IdentitiesOnly=yes",
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

/// Copy a file or directory into the VM. A relative `guest` path is under the agent's home.
pub fn upload(inst: &Instance, host: &Path, guest: &str) -> Result<String> {
    if !host.exists() {
        bail!("{} does not exist", host.display());
    }
    scp(
        inst,
        &host.to_string_lossy(),
        &format!("agent@127.0.0.1:{guest}"),
    )?;
    Ok(format!(
        "copied {} to {}:{guest}",
        host.display(),
        inst.name
    ))
}

/// Copy a file or directory out of the VM.
pub fn download(inst: &Instance, guest: &str, host: &Path) -> Result<String> {
    scp(
        inst,
        &format!("agent@127.0.0.1:{guest}"),
        &host.to_string_lossy(),
    )?;
    Ok(format!(
        "copied {}:{guest} to {}",
        inst.name,
        host.display()
    ))
}

fn scp(inst: &Instance, from: &str, to: &str) -> Result<()> {
    let key = ssh_key().display().to_string();
    let port = inst.ssh_port().to_string();
    let out = Command::new("scp")
        .args(["-r", "-i", &key, "-P", &port])
        .args(SSH_OPTS)
        .args([from, to])
        .output()
        .context("run scp")?;
    if !out.status.success() {
        bail!(
            "copy failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Forward a host port on 127.0.0.1 to a guest port until the VM stops. With no
/// `host_port`, a free one is picked.
pub fn forward(inst: &Instance, guest_port: u16, host_port: Option<u16>) -> Result<String> {
    if !inst.running() {
        bail!("{} is not running", inst.name);
    }
    let host_port = match host_port {
        Some(p) => p,
        None => std::net::TcpListener::bind(("127.0.0.1", 0))?
            .local_addr()?
            .port(),
    };
    let reply = qemu::Qmp::connect(inst)?.execute(
        "human-monitor-command",
        Some(serde_json::json!({
            "command-line": format!("hostfwd_add net0 tcp:127.0.0.1:{host_port}-:{guest_port}")
        })),
    )?;
    let msg = reply.as_str().unwrap_or_default().trim();
    if !msg.is_empty() {
        bail!("forwarding failed: {msg}");
    }
    Ok(format!(
        "127.0.0.1:{host_port} -> {}:{guest_port} (until the VM stops)",
        inst.name
    ))
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

/// Copy-on-write clone of the image, preferring its snapshot disk so the first
/// boot resumes in about a second. The backing path is relative so `$AGENTPC_HOME`
/// can move.
pub fn clone_disk(inst: &Instance) -> Result<()> {
    let image = &inst.image;
    // A RAM snapshot only restores into a VM of the size it was captured at.
    let live = image.has_snapshot() && inst.size() == inst.os.default_size();
    let (base, vars) = if live {
        (image.snapshot_disk(), image.snapshot_vars())
    } else {
        (image.disk(), image.vars())
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
            &format!("../../images/{base_name}"),
            "-F",
            "qcow2",
            &inst.disk().to_string_lossy(),
        ],
    )?;
    let _ = std::fs::remove_file(inst.vars());
    std::fs::copy(vars, inst.vars())?;
    set_writable(&inst.vars())?; // image files are read-only
    std::fs::write(
        inst.dir.join("base"),
        if live { "snapshot" } else { "image" },
    )?;
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
    let cps = checkpoints(inst);
    let cps = if cps.is_empty() {
        String::new()
    } else {
        format!("\n  checkpoints: {}", cps.join(", "))
    };
    format!(
        "{} ({}): viewer {}\n  ssh: agentpc ssh {}    vnc: vnc://127.0.0.1:{}    login: agent/agent{cps}",
        inst.name,
        inst.image,
        viewer::url(inst),
        inst.name,
        5900 + inst.vnc_display()
    )
}

pub fn create(
    image: &Image,
    name: Option<&str>,
    memory: Option<u32>,
    cpus: Option<u32>,
) -> Result<String> {
    let os = image.os;
    if !image.exists() {
        provision_image(image)?;
    }
    let lock = crate::instance::creation_lock()?;
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
    let inst = Instance::create(&name, image, slot)?;
    drop(lock);
    let made = inst.set_size(memory, cpus).and_then(|()| clone_disk(&inst));
    if let Err(e) = made {
        let _ = std::fs::remove_dir_all(&inst.dir);
        return Err(e);
    }
    boot(&inst)
}

/// First `create` from an image: fetch it (Ubuntu), or say how to build it (Windows).
fn provision_image(image: &Image) -> Result<()> {
    match image.os {
        Os::Ubuntu => {
            log!("no {image} image yet; downloading it");
            if let Err(e) = crate::registry::pull(image) {
                log!("download failed ({e:#}); building it locally instead (~3 min)");
                crate::image::build(image, None)?;
            }
            Ok(())
        }
        Os::Windows => bail!(
            "no {image} image yet; build it once (~12 min, downloads the ISO from Microsoft): \
             agentpc image build {image}"
        ),
    }
}

/// Start a stopped instance and wait until it's usable.
pub fn boot(inst: &Instance) -> Result<String> {
    // Only a clone's first boot can resume: afterwards its disk has moved on from the
    // saved RAM, so later starts are cold boots.
    let mut state = None;
    if !inst.running() && inst.resume_marker().exists() {
        let _ = std::fs::remove_file(inst.resume_marker());
        state = Some(inst.image.snapshot_state()).filter(|p| p.is_file());
    }
    boot_from(inst, state.as_deref())
}

/// Start an instance (resuming `state` if given) and wait until it's usable.
fn boot_from(inst: &Instance, state: Option<&Path>) -> Result<String> {
    let mut resumed = false;
    if !inst.running() {
        match state {
            Some(state) => match qemu::start_resumed(inst, state) {
                Ok(()) => resumed = true,
                Err(e) => {
                    log!("{}: resume failed ({e:#}); booting instead", inst.name);
                    qemu::quit(inst);
                    qemu::start(inst, &[])?;
                }
            },
            None => qemu::start(inst, &[])?,
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

fn checkpoint_dir(inst: &Instance, label: &str) -> Result<PathBuf> {
    if label.is_empty()
        || label.starts_with('.')
        || !label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        bail!("invalid checkpoint label '{label}' (letters, digits, . - _)");
    }
    Ok(inst.dir.join("checkpoints").join(label))
}

/// Checkpoint labels of an instance, oldest first.
pub fn checkpoints(inst: &Instance) -> Vec<String> {
    let mut found: Vec<(std::time::SystemTime, String)> =
        std::fs::read_dir(inst.dir.join("checkpoints"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().join("disk.qcow2").is_file())
            .map(|e| {
                let t = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                (t, e.file_name().to_string_lossy().into_owned())
            })
            .collect();
    found.sort();
    found.into_iter().map(|(_, n)| n).collect()
}

/// Save the instance's disk and, if it's running, its RAM under `label` (replacing an
/// older checkpoint of that name). A running VM pauses for a few seconds and carries on.
/// Disk copies are APFS clones, so they take no space until the VM writes more.
pub fn checkpoint(inst: &Instance, label: &str) -> Result<String> {
    let dir = checkpoint_dir(inst, label)?;
    let tmp = dir.with_extension("partial");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    let copy = || -> Result<()> {
        std::fs::copy(inst.disk(), tmp.join("disk.qcow2"))?;
        std::fs::copy(inst.vars(), tmp.join("vars.fd"))?;
        Ok(())
    };
    let saved = if inst.running() {
        qemu::checkpoint(inst, &tmp.join("state"), copy)
    } else {
        copy()
    };
    if let Err(e) = saved {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::rename(&tmp, &dir)?;
    let kind = if dir.join("state").is_file() {
        "disk and memory"
    } else {
        "disk"
    };
    Ok(format!(
        "checkpoint {label} of {} saved ({kind})",
        inst.name
    ))
}

/// Put the instance back exactly as it was at checkpoint `label` and start it: it resumes
/// in seconds if the checkpoint has its memory, else it boots. Port forwards are lost.
pub fn restore(inst: &Instance, label: &str) -> Result<String> {
    let dir = checkpoint_dir(inst, label)?;
    if !dir.join("disk.qcow2").is_file() {
        let have = checkpoints(inst);
        bail!(
            "{} has no checkpoint '{label}'{}",
            inst.name,
            if have.is_empty() {
                String::new()
            } else {
                format!(" (it has: {})", have.join(", "))
            }
        );
    }
    qemu::quit(inst);
    std::fs::copy(dir.join("disk.qcow2"), inst.disk())?;
    std::fs::copy(dir.join("vars.fd"), inst.vars())?;
    let _ = std::fs::remove_file(inst.resume_marker());
    let state = Some(dir.join("state")).filter(|p| p.is_file());
    boot_from(inst, state.as_deref())
}

pub fn delete_checkpoint(inst: &Instance, label: &str) -> Result<String> {
    let dir = checkpoint_dir(inst, label)?;
    if !dir.is_dir() {
        bail!("{} has no checkpoint '{label}'", inst.name);
    }
    std::fs::remove_dir_all(&dir)?;
    Ok(format!("deleted checkpoint {label} of {}", inst.name))
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
        "{:<14} {:<22} {:<5} {:<8} {}\n",
        "NAME", "IMAGE", "SLOT", "STATE", "VIEWER"
    );
    for i in Instance::list()? {
        let state = if i.running() { "running" } else { "stopped" };
        s += &format!(
            "{:<14} {:<22} {:<5} {:<8} {}\n",
            i.name,
            i.image,
            i.slot,
            state,
            viewer::url(&i)
        );
    }
    s += "\nIMAGES\n";
    s += &crate::image::list()?;
    Ok(s)
}

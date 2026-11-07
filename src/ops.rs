//! Instance lifecycle shared by the CLI and the MCP gateway.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::instance::{Image, Instance, Os, ssh_key};
use crate::{log, qemu, viewer};

/// Session env for cua-driver: the Linux guests' autologin X session (the `agent` user,
/// uid 1000) and its AT-SPI bus.
pub const LINUX_SESSION_ENV: &str = "DISPLAY=:0 XAUTHORITY=/home/agent/.Xauthority \
     XDG_RUNTIME_DIR=/run/user/1000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus";

pub const SSH_OPTS: [&str; 24] = [
    "-o",
    "StrictHostKeyChecking=no",
    "-o",
    "UserKnownHostsFile=/dev/null",
    "-o",
    "LogLevel=ERROR",
    "-o",
    "ConnectTimeout=5",
    // Notice a wedged VM or a dropped tunnel instead of blocking forever (4 * 15 s).
    "-o",
    "ServerAliveInterval=15",
    "-o",
    "ServerAliveCountMax=4",
    "-o",
    "BatchMode=yes",
    // Otherwise keys in the user's ssh-agent are offered first and can exhaust the server's
    // MaxAuthTries before ours is tried.
    "-o",
    "IdentitiesOnly=yes",
    // The guest is untrusted: never hand it the user's ssh-agent or X display (a ~/.ssh/config
    // `Host *` might), and never share a connection with the user's own sessions.
    "-o",
    "ForwardAgent=no",
    "-o",
    "ForwardX11=no",
    "-o",
    "ControlMaster=no",
    "-o",
    "ControlPath=none",
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

/// Make a server inside the VM reachable from this Mac. A detached SSH local tunnel
/// (`ssh -N -L`) reaches servers bound to the guest's own 127.0.0.1 (the Vite/Next default),
/// which QEMU's hostfwd cannot. The tunnel lives until the VM stops or it is removed
/// (`remove_forward`/`stop_forwards`); its pid is recorded under
/// `<instance>/forwards/<host_port>.pid`. With no `host_port`, a free one is picked.
/// UDP can't go through an SSH tunnel, so a UDP forward is a QEMU hostfwd instead
/// (`forward_udp`).
pub fn forward(
    inst: &Instance,
    guest_port: u16,
    host_port: Option<u16>,
    protocol: Protocol,
) -> Result<String> {
    if guest_port == 0 || host_port == Some(0) {
        bail!("ports are 1-65535 (leave host_port out for a free one)");
    }
    // Under the VM's lock: two forwards can't both claim one host port, and a reset can't
    // swap QEMU out from under a UDP hostfwd being added.
    let _lock = inst.lock()?;
    if !inst.running() {
        bail!("{} is not running; start it first", inst.name);
    }
    if protocol == Protocol::Udp {
        return forward_udp(inst, guest_port, host_port);
    }
    let host_port = match host_port {
        Some(p) => p,
        None => std::net::TcpListener::bind(("127.0.0.1", 0))?
            .local_addr()?
            .port(),
    };
    let dir = inst.dir.join("forwards");
    std::fs::create_dir_all(&dir)?;
    let pid_file = dir.join(format!("{host_port}.pid"));
    // A live tunnel already on this host port: reuse it rather than start a second one, if
    // it goes where this one should.
    if let Some((pid, to)) = read_forward(&pid_file)
        && tunnel_alive(pid, host_port)
    {
        if to != guest_port {
            bail!(
                "127.0.0.1:{host_port} already forwards to {}:{to}; delete that forward first, \
                 or pick another host port",
                inst.name
            );
        }
        return Ok(forward_url(inst, host_port, guest_port));
    }
    // Something else on that port would answer the readiness check below in the tunnel's place.
    if std::net::TcpListener::bind(("127.0.0.1", host_port)).is_err() {
        bail!(
            "127.0.0.1:{host_port} is in use on this Mac; pick another host port (or leave it out)"
        );
    }
    let log_path = dir.join(format!("{host_port}.log"));
    let log = std::fs::File::create(&log_path)?;
    let mut cmd = Command::new("ssh");
    cmd.args([
        "-i".to_string(),
        ssh_key().display().to_string(),
        "-p".into(),
        inst.ssh_port().to_string(),
    ])
    .args(SSH_OPTS)
    .args([
        "-N",
        "-o",
        "ExitOnForwardFailure=yes",
        "-L",
        &format!("127.0.0.1:{host_port}:127.0.0.1:{guest_port}"),
        "agent@127.0.0.1",
    ])
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::null())
    .stderr(log)
    // Own process group: a Ctrl-C in the CLI must not take the tunnel down.
    .process_group(0);
    let mut child = cmd.spawn().context("spawn ssh tunnel")?;
    let pid = child.id() as i32;
    // ExitOnForwardFailure makes ssh exit fast if it can't bind the local port; otherwise the
    // listener is up within a moment of the connection succeeding.
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        if let Some(status) = child.try_wait()? {
            let err = std::fs::read_to_string(&log_path).unwrap_or_default();
            bail!(
                "forwarding port {host_port} failed ({status}): {}",
                err.trim()
            );
        }
        if std::net::TcpStream::connect_timeout(
            &([127, 0, 0, 1], host_port).into(),
            Duration::from_millis(200),
        )
        .is_ok()
        {
            break;
        }
        if Instant::now() > deadline {
            crate::instance::kill(pid, 9);
            bail!("forwarding port {host_port}: the tunnel did not come up");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    std::fs::write(&pid_file, format!("{pid}\n{guest_port}\n"))?;
    // Reap it when the VM stops and the tunnel dies, so a long-lived MCP server keeps no zombie.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(forward_url(inst, host_port, guest_port))
}

fn forward_url(inst: &Instance, host_port: u16, guest_port: u16) -> String {
    format!(
        "127.0.0.1:{host_port} -> {}:{guest_port} (until the VM stops or the forward is removed)",
        inst.name
    )
}

/// A forward's transport. TCP goes through an SSH tunnel, UDP through a QEMU hostfwd.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Protocol {
    #[default]
    Tcp,
    Udp,
}

impl Protocol {
    fn as_str(self) -> &'static str {
        match self {
            Protocol::Tcp => "tcp",
            Protocol::Udp => "udp",
        }
    }

    /// Extension of the forward's record under `<instance>/forwards/`.
    fn record_ext(self) -> &'static str {
        match self {
            Protocol::Tcp => "pid",
            Protocol::Udp => "udp",
        }
    }
}

/// The netdev every VM gets (`qemu::launch`), which runtime hostfwds attach to.
const NETDEV: &str = "net0";

fn hostfwd_add_cmd(host_port: u16, guest_port: u16) -> String {
    format!("hostfwd_add {NETDEV} udp:127.0.0.1:{host_port}-:{guest_port}")
}

fn hostfwd_remove_cmd(host_port: u16) -> String {
    format!("hostfwd_remove {NETDEV} udp:127.0.0.1:{host_port}")
}

/// Forward UDP on 127.0.0.1:<host_port> to the guest with a QEMU user-net hostfwd added over
/// the monitor. Unlike the TCP tunnel it reaches the guest's network address (10.0.2.15), so
/// the server must listen on 0.0.0.0, not 127.0.0.1. The hostfwd dies with this QEMU; the
/// record, `<instance>/forwards/<host_port>.udp`, holds that QEMU's pid and the guest port,
/// so a record left from an earlier run reads as down.
fn forward_udp(inst: &Instance, guest_port: u16, host_port: Option<u16>) -> Result<String> {
    if inst.offline() {
        // libslirp's restrict=on drops every UDP datagram the guest sends, replies included.
        bail!(
            "{} is offline: its network drops all UDP from the guest, so a UDP forward can't \
             carry replies",
            inst.name
        );
    }
    let qemu_pid = inst
        .pid()
        .with_context(|| format!("{} is not running; start it first", inst.name))?;
    let host_port = match host_port {
        Some(p) => p,
        None => std::net::UdpSocket::bind(("127.0.0.1", 0))?
            .local_addr()?
            .port(),
    };
    let dir = inst.dir.join("forwards");
    std::fs::create_dir_all(&dir)?;
    let record = dir.join(format!("{host_port}.udp"));
    if let Some((pid, guest)) = read_forward(&record)
        && pid == qemu_pid
    {
        if guest != guest_port {
            bail!(
                "udp 127.0.0.1:{host_port} already forwards to {}:{guest}; delete it first",
                inst.name
            );
        }
        return Ok(forward_udp_text(inst, host_port, guest_port));
    }
    let out = qemu::Qmp::connect(inst)?.hmp(&hostfwd_add_cmd(host_port, guest_port))?;
    if !out.is_empty() {
        bail!("forwarding udp port {host_port} failed: {out}");
    }
    std::fs::write(&record, format!("{qemu_pid}\n{guest_port}\n"))?;
    Ok(forward_udp_text(inst, host_port, guest_port))
}

fn forward_udp_text(inst: &Instance, host_port: u16, guest_port: u16) -> String {
    format!(
        "udp 127.0.0.1:{host_port} -> {}:{guest_port} (until the VM stops or the forward is \
         removed; the guest server must listen on 0.0.0.0, not 127.0.0.1)",
        inst.name
    )
}

pub fn ssh(inst: &Instance, remote: &str) -> Result<Output> {
    Command::new("ssh")
        .args(ssh_args(inst, remote))
        .output()
        .context("run ssh")
}

/// Where the cua-driver installer puts the binary on Windows (as the `agent` user).
pub const WINDOWS_CUA_DRIVER: &str =
    r"$env:LOCALAPPDATA\Programs\Cua\cua-driver\bin\cua-driver.exe";

/// Desktop session up and its control server answering. Errs when it never will be.
pub fn ready(inst: &Instance) -> Result<bool> {
    match inst.os {
        Os::Windows => {
            let probe = format!(
                r#"if (Test-Path C:\OEM\failed.txt) {{ 'failed' }}
elseif (-not (Test-Path C:\OEM\done.txt)) {{ 'wait' }}
elseif (Test-Path "{WINDOWS_CUA_DRIVER}") {{ if ((& "{WINDOWS_CUA_DRIVER}" status 2>&1 | Out-String) -match 'daemon is running') {{ 'ready' }} }}
elseif (Test-Path C:\uv\bin\windows-mcp.exe) {{ 'old' }}
else {{ 'missing' }}"#
            );
            let out = ssh_within(inst, &probe, Duration::from_secs(20));
            Ok(
                match out
                    .as_ref()
                    .map(|o| String::from_utf8_lossy(&o.stdout))
                    .as_deref()
                    .map(str::trim)
                {
                    Some("ready") => true,
                    Some("old") => bail!("{}", old_windows_image(inst)),
                    Some("failed") => bail!(
                        "{}: Windows setup failed (see C:\\OEM\\failed.txt and C:\\OEM\\setup.log \
                         in the guest)",
                        inst.name
                    ),
                    Some("missing") => bail!(
                        "{}: setup finished but installed no desktop-control server \
                     (see C:\\OEM\\setup.log in the guest)",
                        inst.name
                    ),
                    _ => false,
                },
            )
        }
        // Bounded like the Windows probe: a guest whose probe hangs while SSH stays up must
        // not hold boot (and the VM's lock) past its timeout.
        Os::Ubuntu | Os::Arch => Ok(ssh_within(
            inst,
            &format!(
                "test -f /var/lib/cloud/agent-ready && pgrep -x xfce4-session >/dev/null && \
                 {LINUX_SESSION_ENV} ~/.local/bin/cua-driver --version"
            ),
            Duration::from_secs(20),
        )
        .is_some_and(|o| o.status.success())),
    }
}

/// `ssh` that gives up after `limit`: a wedged guest command would otherwise block forever.
fn ssh_within(inst: &Instance, remote: &str, limit: Duration) -> Option<Output> {
    let mut child = Command::new("ssh")
        .args(ssh_args(inst, remote))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) if start.elapsed() < limit => std::thread::sleep(Duration::from_millis(100)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Why a Windows VM from an image built by agentpc 0.1.0 (Windows-MCP, no cua-driver) can't be
/// driven, and what to do about it.
pub fn old_windows_image(inst: &Instance) -> String {
    format!(
        "{} comes from an image built by agentpc 0.1.0, whose desktop server (Windows-MCP) is no \
         longer supported. Rebuild the image (agentpc image build {}) and create a new VM",
        inst.name, inst.image
    )
}

pub fn wait_ready(inst: &Instance, timeout: Duration) -> Result<Duration> {
    let start = Instant::now();
    while !ready(inst)? {
        if !inst.running() {
            bail!(
                "{}: qemu exited (see {})",
                inst.name,
                inst.dir.join("qemu.log").display()
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
/// boot resumes in seconds. The backing path is relative so `$AGENTPC_HOME`
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
    let _ = std::fs::remove_file(inst.dir.join("tz"));
    forget_checkpoint_origin(inst);
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

/// Writable by its owner (set_readonly(false) would make it writable by everyone).
pub fn set_writable(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(p)?.permissions().mode();
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode | 0o200))?;
    Ok(())
}

/// Refuse an operation that needs more free space than the home volume has.
pub(crate) fn ensure_free_space(need_gb: u64, action: &str) -> Result<()> {
    if let Some(free) = crate::instance::free_disk_bytes(&crate::instance::home())
        && free < need_gb << 30
    {
        bail!(
            "not enough disk space to {action}: need ~{need_gb} GB, only {} GB free",
            free >> 30
        );
    }
    Ok(())
}

/// Refuse a VM whose RAM exceeds the Mac's, and warn if running VMs would oversubscribe it.
fn check_memory(want_gb: u32) -> Result<()> {
    let Some(total_gb) = crate::instance::host_mem_bytes().map(|b| b >> 30) else {
        return Ok(());
    };
    if u64::from(want_gb) > total_gb {
        bail!("requested {want_gb} GB RAM but the Mac has only {total_gb} GB");
    }
    let running: u64 = Instance::list()
        .unwrap_or_default()
        .iter()
        .filter(|i| i.running())
        .map(|i| u64::from(i.size().0))
        .sum();
    if running + u64::from(want_gb) > total_gb {
        log!(
            "warning: running VMs would use {} GB RAM, over the Mac's {total_gb} GB",
            running + u64::from(want_gb)
        );
    }
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

/// How FEX gets x86's memory ordering (TSO) in an x86apps VM: "hardware" when every vCPU
/// runs in TSO mode, else "emulated".
fn x86_tso(inst: &Instance) -> &'static str {
    if qemu::hardware_tso(inst) {
        "hardware"
    } else {
        "emulated"
    }
}

pub fn info(inst: &Instance) -> String {
    let cps = checkpoints(inst);
    let cps = if cps.is_empty() {
        String::new()
    } else {
        format!("\n  checkpoints: {}", cps.join(", "))
    };
    let x86 = if inst.image.x86_apps() {
        let tso = match x86_tso(inst) {
            "hardware" => "hardware TSO",
            _ => "emulated TSO (slower; hardware TSO needs macOS 15+)",
        };
        format!(
            "\n  x86 programs: FEX, {tso}\
             \n  x86-only installer (uname -m): sudo FEXBash ./install.sh, or paste it into FEXBash; \
             x86 service started by a script: sudo fex-unit <unit>"
        )
    } else {
        String::new()
    };
    format!(
        "{} ({}): viewer {}\n  ssh: {} ssh {}    vnc: vnc://127.0.0.1:{}    login: agent/agent{x86}{cps}",
        inst.name,
        inst.image,
        viewer::url(inst),
        crate::setup::cmd_name(),
        inst.name,
        inst.vnc_port()
    )
}

/// Refuse a VM name that can't be a directory under `instances/` or would pass for a
/// hidden build VM (`_build-*`) or a dotfile.
fn check_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || name.starts_with(['_', '.'])
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        bail!(
            "invalid instance name '{name}' (up to 64 letters, digits, . - _; not starting with . or _)"
        );
    }
    Ok(())
}

pub fn create(
    image: &Image,
    name: Option<&str>,
    memory: Option<u32>,
    cpus: Option<u32>,
    offline: bool,
    owner: Option<&str>,
) -> Result<String> {
    // Checked before a download or build that can take minutes; rechecked below for races.
    Instance::check_size(memory, cpus)?;
    precheck(image.os, name, memory)?;
    if !image.exists() {
        provision_image(image)?;
    }
    let (inst, _instance_lock) = reserve(image, name, memory, cpus, offline)?;
    fill_or_discard(&inst, || {
        clone_image_disk(&inst)?;
        // Before the first boot, so a VM that never becomes ready is still its owner's: a
        // retried create_vm boots it again, and the owner's exit stops it.
        owner.map_or(Ok(()), |o| set_owner(&inst, o))
    })?;
    let note = crate::image::outdated(image).map(|h| format!("\n  note: the {image} image: {h}"));
    boot_locked(&inst)
        .map(|s| s + &note.unwrap_or_default())
        .map_err(|e| not_ready(e, &inst))
}

/// What every create checks before any work: the name, and the Mac's memory and disk.
fn precheck(os: Os, name: Option<&str>, memory: Option<u32>) -> Result<()> {
    if let Some(n) = name {
        check_name(n)?;
        if crate::instance::instances_dir().join(n).exists() {
            bail!("instance '{n}' exists");
        }
    }
    check_memory(memory.unwrap_or(os.default_size().0))?;
    ensure_free_space(4, "create a VM")
}

/// Reserve a new instance of `image`: a free slot and its name under `creation_lock`, which
/// is released before this returns (image builds hold their image lock before reserving a
/// scratch slot, so a caller must not hold creation_lock while taking an image lock), then
/// its size and offline flag. Returns it with its own lock, to hold through the first boot.
fn reserve(
    image: &Image,
    name: Option<&str>,
    memory: Option<u32>,
    cpus: Option<u32>,
    offline: bool,
) -> Result<(Instance, std::fs::File)> {
    let lock = crate::instance::creation_lock()?;
    let slot = Instance::free_slot()?;
    let os = image.os;
    let name = match name {
        Some(n) => n.to_string(),
        None => (1..)
            .map(|i| format!("{os}-{i}"))
            .find(|n| !crate::instance::instances_dir().join(n).exists())
            .unwrap(),
    };
    if crate::instance::instances_dir().join(&name).exists() {
        bail!("instance '{name}' exists");
    }
    let (inst, instance_lock) = Instance::create_locked(&name, image, slot)?;
    drop(lock);
    fill_or_discard(&inst, || {
        inst.set_size(memory, cpus)?;
        if offline {
            std::fs::write(inst.dir.join("offline"), "")?;
        }
        Ok(())
    })?;
    Ok((inst, instance_lock))
}

/// A create whose VM was made but didn't become ready: the VM stays.
fn not_ready(e: anyhow::Error, inst: &Instance) -> anyhow::Error {
    let from = created_from(inst).map_or_else(String::new, |f| format!(" and from_checkpoint {f}"));
    anyhow::anyhow!(
        "{e:#}\nThe VM '{}' was created but didn't become ready (read_vm_log, or {}, shows why). \
         Starting it again retries, as does create_vm with the same name{from} from the same \
         session; deleting it starts over.",
        inst.name,
        inst.dir.join("qemu.log").display()
    )
}

/// Instance file recording the checkpoint a VM was made from (`<vm>/<label>`).
const FROM_CHECKPOINT: &str = "from_checkpoint";
/// Instance file present while the job records a VM's disk came with may still be in its
/// guest. Every boot (`boot`) retries their deletion until it succeeds, and stops the VM when
/// it fails (so no new job's records are there for the next try to delete); a fresh disk
/// (`clone_disk`) removes it. `restore` does not run the cleanup (a later boot does), and a
/// checkpoint of the VM taken while it is pending keeps those records.
const RECORDS_PENDING: &str = "records-pending";
/// How long the guest gets to delete its job records, all attempts together.
const RECORDS_LIMIT: Duration = Duration::from_secs(60);
/// Attempts at the cleanup when the guest answers without confirming it: a Windows guest just
/// booted cold may not have its Task Scheduler answering yet, and its script then fails closed.
const RECORDS_ATTEMPTS: u32 = 4;
/// The pause between those attempts.
const RECORDS_RETRY_PAUSE: Duration = Duration::from_secs(3);

/// A new VM whose disk and firmware vars are APFS clones of checkpoint `from`. The checkpoint's
/// memory is never used: the VM boots cold, so its size is free, and offline, size and owner
/// come from this call. It depends on the image only: deleting the source VM or the
/// checkpoint later doesn't affect it. Its guest's agentpc job records (which came with the
/// disk) are deleted before this returns. Errors before any work start with
/// `invalid_checkpoint_ref` (from `CheckpointRef::parse`), `no_such_vm`, `no_such_checkpoint`
/// or `image_mismatch`.
pub fn create_from_checkpoint(
    from: &CheckpointRef,
    want_image: Option<&str>,
    name: Option<&str>,
    memory: Option<u32>,
    cpus: Option<u32>,
    offline: bool,
    owner: Option<&str>,
) -> Result<String> {
    Instance::check_size(memory, cpus)?;
    let source = Instance::load(&from.vm).map_err(|_| {
        anyhow::anyhow!(
            "no_such_vm: no VM '{}' to take checkpoint {from} from (see list_vms)",
            from.vm
        )
    })?;
    let cp = validate_source(&source, &from.label, want_image)?;
    precheck(source.os, name, memory)?;
    let (inst, _instance_lock) = reserve(&source.image, name, memory, cpus, offline)?;
    fill_or_discard(&inst, || {
        copy_checkpoint_locked(
            &source.lock_path(),
            &crate::instance::image_lock_path(&inst.image),
            || source.dir.is_dir(),
            || clone_checkpoint_files(&cp, &inst),
        )?;
        record_checkpoint_origin(&inst, from)?;
        // Before the first boot, as in `create`.
        owner.map_or(Ok(()), |o| set_owner(&inst, o))
    })?;
    log!(
        "{}: made from checkpoint {from}; booting it fresh",
        inst.name
    );
    let ready = boot_locked(&inst).map_err(|e| not_ready(e, &inst))?;
    clear_pending_records(&inst)?;
    let note = crate::image::outdated(&inst.image)
        .map(|h| format!("\n  note: the {} image: {h}", inst.image))
        .unwrap_or_default();
    Ok(ready + &from_note(&inst) + &note)
}

/// Record that `inst` was made from checkpoint `from`, and that the job records its disk came
/// with are still to be deleted.
fn record_checkpoint_origin(inst: &Instance, from: &CheckpointRef) -> Result<()> {
    std::fs::write(inst.dir.join(FROM_CHECKPOINT), from.to_string())?;
    std::fs::write(inst.dir.join(RECORDS_PENDING), "")?;
    Ok(())
}

/// For a fresh image disk (`clone_disk`, so reset_vm): a clean install did not come from a
/// checkpoint, and has no records from one to delete.
fn forget_checkpoint_origin(inst: &Instance) {
    let _ = std::fs::remove_file(inst.dir.join(FROM_CHECKPOINT));
    let _ = std::fs::remove_file(inst.dir.join(RECORDS_PENDING));
}

/// The checkpoint `inst` was made from, if any.
pub fn created_from(inst: &Instance) -> Option<CheckpointRef> {
    crate::instance::read_trimmed(&inst.dir.join(FROM_CHECKPOINT))
        .ok()
        .and_then(|s| CheckpointRef::parse(&s).ok())
}

/// The reply line saying where a checkpoint-made VM came from; empty for others.
fn from_note(inst: &Instance) -> String {
    created_from(inst).map_or_else(String::new, |f| {
        format!(
            "\n  from checkpoint {f}: booted fresh; its memory, port forwards and job records \
             were not carried over"
        )
    })
}

/// `boot` for create_vm's same-session retry: the VM, plus where it came from.
pub fn boot_created(inst: &Instance) -> Result<String> {
    boot(inst).map(|s| s + &from_note(inst))
}

/// Delete the job records a checkpoint-made VM's disk came with, if not done yet. A VM
/// without the marker (any VM made from an image) is never sent anything.
fn clear_pending_records(inst: &Instance) -> Result<()> {
    let marker = inst.dir.join(RECORDS_PENDING);
    if !marker.exists() {
        return Ok(());
    }
    let from = created_from(inst)
        .map_or_else(|| "a checkpoint".to_string(), |f| format!("checkpoint {f}"));
    log!("{}: deleting the job records copied from {from}", inst.name);
    if let Err(e) = clear_job_records(inst) {
        // Stop it: jobs started now would land in ~/agentpc-bg, which the retry deletes.
        stop_forwards(inst);
        let stopped = qemu::stop(inst);
        viewer::stop_if_idle();
        bail!(
            "{e:#}\nThe VM '{}' was made from {from}, but agentpc could not delete the job \
             records it copied from there, so it {}. Starting it again (start_vm, or create_vm \
             with the same name and from_checkpoint from the same session) retries; delete_vm \
             removes it.",
            inst.name,
            match stopped {
                Ok(()) => "was stopped before anything new could run in it".to_string(),
                Err(s) => format!("is being stopped, but stopping it failed ({s:#})"),
            }
        );
    }
    std::fs::remove_file(&marker).with_context(|| {
        format!(
            "the job records copied from {from} were deleted, but agentpc could not remove its \
             marker {} (the next start of '{}' deletes them again, harmlessly)",
            marker.display(),
            inst.name
        )
    })
}

/// Run the guest's job-record cleanup (crate::guest) over SSH, within RECORDS_LIMIT. An
/// answer without the confirmation is retried a few times (RECORDS_ATTEMPTS), since the
/// script fails closed while a cold-booted guest's services are still starting; no answer at
/// all is not.
fn clear_job_records(inst: &Instance) -> Result<()> {
    let cmd = match inst.os {
        Os::Windows => windows_command(crate::guest::WINDOWS_CLEAR_RECORDS),
        Os::Ubuntu | Os::Arch => crate::guest::LINUX_CLEAR_RECORDS.to_string(),
    };
    retry_cleanup(
        &inst.name,
        |left| ssh_within(inst, &cmd, left),
        RECORDS_LIMIT,
        RECORDS_RETRY_PAUSE,
    )
}

/// `clear_job_records`' attempts: `run` gets the time left and returns the guest's answer, or
/// None for no answer.
fn retry_cleanup(
    name: &str,
    mut run: impl FnMut(Duration) -> Option<Output>,
    limit: Duration,
    pause: Duration,
) -> Result<()> {
    let deadline = Instant::now() + limit;
    let mut attempt = 1;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let Some(out) = run(left) else {
            // An earlier attempt was answered: the guest is up but never confirmed.
            if attempt > 1 {
                bail!(
                    "the guest did not confirm that its job records were deleted (after {attempt} \
                     attempts within {} s; the last got no answer)",
                    limit.as_secs()
                );
            }
            bail!("the guest did not answer within {} s", limit.as_secs());
        };
        match records_cleared(&out) {
            Ok(()) => return Ok(()),
            Err(e)
                if attempt >= RECORDS_ATTEMPTS
                    || deadline.saturating_duration_since(Instant::now()) <= pause =>
            {
                return Err(e.context(format!("after {attempt} attempts")));
            }
            Err(e) => {
                log!("{name}: job-record cleanup not confirmed ({e:#}); retrying");
                std::thread::sleep(pause);
                attempt += 1;
            }
        }
    }
}

/// Cleared only on exit 0 with the script's own CLEARED line (a login banner may precede it,
/// and Windows PowerShell may start its output with a UTF-8 byte-order mark).
fn records_cleared(out: &Output) -> Result<()> {
    let said = String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|l| l.trim_start_matches('\u{feff}').trim() == crate::guest::CLEARED);
    if out.status.success() && said {
        return Ok(());
    }
    bail!(
        "the guest did not confirm that its job records were deleted ({})",
        out.status
    )
}

/// `clone_disk` under the image lock, so a build, pull or snapshot can't replace the files
/// it picks and clones mid-way. Take it after `creation_lock` is dropped: builds hold the
/// image lock and then take `creation_lock` for their scratch VM.
fn clone_image_disk(inst: &Instance) -> Result<()> {
    let _lock = crate::instance::image_lock(&inst.image)?;
    clone_disk(inst)
}

/// First `create` from an image: fetch it (Ubuntu, Arch), or say how to build it (Windows).
fn provision_image(image: &Image) -> Result<()> {
    match image.os {
        Os::Ubuntu | Os::Arch => {
            let _lock = crate::instance::image_lock(image)?;
            fetch_image_locked(image)
        }
        Os::Windows if crate::image::windows_iso_entry(&image.version).is_none() => bail!(
            "no {image} image, and no known download for windows-{}; build one from your own \
             ARM64 ISO ({} image build {image} --iso <path>) or use one of: {}",
            image.version,
            crate::setup::cmd_name(),
            crate::image::WINDOWS_ISOS
                .iter()
                .map(|w| format!("windows-{}", w.version))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Os::Windows => bail!(
            "no {image} image yet; build it once (~12 min, downloads the ISO from Microsoft): \
             {} image build {image}",
            crate::setup::cmd_name()
        ),
    }
}

/// Get a missing Linux image, for a caller holding its image lock: pull it, else build it
/// here. Also how an Arch build gets the Ubuntu image its helper VM runs.
pub(crate) fn fetch_image_locked(image: &Image) -> Result<()> {
    // Another create may have fetched it while this one waited for the lock.
    if image.exists() {
        return Ok(());
    }
    log!("no {image} image yet; downloading it");
    if let Err(e) = crate::registry::pull_locked(image) {
        // A dated build exists only in the registry; building here would make today's.
        if image.pinned() {
            return Err(e.context(format!("downloading the pinned build {image} failed")));
        }
        // A failed snapshot leaves a downloaded but unusable image; rebuild it too.
        let what = if image.exists() {
            "setting up the downloaded image"
        } else {
            "download"
        };
        let minutes = match image.os {
            Os::Arch if image.x86_apps() => 10,
            Os::Arch => 6,
            _ if image.x86_apps() => 8,
            _ => 3,
        };
        log!("{what} failed ({e:#}); building it locally instead (~{minutes} min)");
        crate::image::build_locked(image, None)?;
    }
    Ok(())
}

/// Start a stopped instance and wait until it's usable. A VM made from a checkpoint whose
/// job-record cleanup hasn't succeeded yet gets it now.
pub fn boot(inst: &Instance) -> Result<String> {
    let _lock = inst.lock()?;
    let ready = boot_locked(inst)?;
    clear_pending_records(inst)?;
    Ok(ready)
}

/// `boot`, for a caller already holding this instance's lock (reset).
fn boot_locked(inst: &Instance) -> Result<String> {
    // Only a clone's first boot can resume: afterwards its disk has moved on from the
    // saved RAM, so later starts are cold boots.
    let mut state = None;
    if !inst.running() && inst.resume_marker().exists() {
        let _ = std::fs::remove_file(inst.resume_marker());
        state = Some(inst.image.snapshot_state()).filter(|p| p.is_file());
    }
    boot_from(inst, state.as_deref())
}

/// Start an instance (resuming `state` if given) and wait until it's usable. Assumes the
/// caller holds this instance's lock.
fn boot_from(inst: &Instance, state: Option<&Path>) -> Result<String> {
    if inst.running() {
        // Already up: a checkpoint interrupted mid-save can leave it paused. Nudge it on.
        qemu::resume_if_paused(inst);
    } else {
        match state {
            Some(state) => match qemu::start_resumed(inst, state) {
                Ok(()) => {}
                Err(e) => {
                    log!("{}: resume failed ({e:#}); booting instead", inst.name);
                    qemu::quit(inst);
                    qemu::start(inst, &[])?;
                }
            },
            None => qemu::start(inst, &[])?,
        }
    }
    // The viewer is a convenience; a download or start failure must not fail the boot.
    if let Err(e) = viewer::ensure_running() {
        log!("{}: viewer unavailable ({e:#})", inst.name);
    }
    let took = wait_ready(inst, Duration::from_secs(inst.os.boot_timeout()))?;
    // Set the clock (a resumed guest keeps its stale snapshot clock) and time zone.
    sync_clock(inst);
    if inst.image.x86_apps() {
        set_fex_memory_model(inst);
    }
    log!("{} ready in {:.1}s", inst.name, took.as_secs_f32());
    Ok(info(inst))
}

/// Tell FEX in an x86apps guest whether this run's CPU is in TSO mode (qemu::hardware_tso),
/// so it stops emulating x86 memory ordering only when that's safe. Every boot, since a
/// snapshot or checkpoint may come from a run in the other mode.
fn set_fex_memory_model(inst: &Instance) {
    let mode = x86_tso(inst);
    match ssh(
        inst,
        &format!("sudo /usr/local/sbin/agentpc-fex-tso {mode}"),
    ) {
        Ok(out) if out.status.success() => {}
        Ok(out) => log!(
            "{}: setting FEX's memory model failed: {}",
            inst.name,
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) => log!("{}: setting FEX's memory model failed: {e:#}", inst.name),
    }
}

/// A checkpoint label: letters, digits, `.` `-` `_`, not starting with a dot (in-progress
/// `.partial-*` siblings do).
fn check_label(label: &str) -> Result<()> {
    if label.is_empty()
        || label.starts_with('.')
        || !label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        bail!("invalid checkpoint label '{label}' (letters, digits, . - _)");
    }
    Ok(())
}

fn checkpoint_dir(inst: &Instance, label: &str) -> Result<PathBuf> {
    check_label(label)?;
    Ok(inst.dir.join("checkpoints").join(label))
}

/// A checkpoint of a VM, `<vm>/<label>`: what create_vm's from_checkpoint names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointRef {
    pub vm: String,
    pub label: String,
}

impl CheckpointRef {
    /// `<vm>/<label>`, each part valid by the rules for VM names and checkpoint labels. Errors
    /// start with `invalid_checkpoint_ref`.
    pub fn parse(s: &str) -> Result<Self> {
        let bad = || {
            anyhow::anyhow!(
                "invalid_checkpoint_ref: '{s}' is not <vm>/<label> (a VM name, then one of its \
                 checkpoint labels)"
            )
        };
        let (vm, label) = s.split_once('/').ok_or_else(bad)?;
        if label.contains('/') {
            return Err(bad());
        }
        check_name(vm).map_err(|e| anyhow::anyhow!("invalid_checkpoint_ref: {e:#}"))?;
        check_label(label).map_err(|e| anyhow::anyhow!("invalid_checkpoint_ref: {e:#}"))?;
        Ok(Self {
            vm: vm.to_string(),
            label: label.to_string(),
        })
    }
}

impl std::fmt::Display for CheckpointRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.vm, self.label)
    }
}

/// The directory of `source`'s checkpoint `label`, checked before any work: it holds both
/// disk.qcow2 and vars.fd, and `want_image` (create_vm's os/version, when given) is the
/// source's image.
fn validate_source(source: &Instance, label: &str, want_image: Option<&str>) -> Result<PathBuf> {
    let dir = checkpoint_dir(source, label)?;
    if !(dir.join("disk.qcow2").is_file() && dir.join("vars.fd").is_file()) {
        // Only complete ones: the label just refused may have a disk without its vars.
        let have: Vec<String> = checkpoints(source)
            .into_iter()
            .filter(|l| {
                source
                    .dir
                    .join("checkpoints")
                    .join(l)
                    .join("vars.fd")
                    .is_file()
            })
            .collect();
        bail!(
            "no_such_checkpoint: {} has no checkpoint '{label}' ({})",
            source.name,
            if have.is_empty() {
                "it has none".to_string()
            } else {
                format!("it has: {}", have.join(", "))
            }
        );
    }
    if let Some(want) = want_image
        && !Image::resolve(want).is_ok_and(|i| i == source.image)
    {
        bail!(
            "image_mismatch: checkpoint {}/{label} is of {}, not {want}; omit os and version, or \
             give the source's",
            source.name,
            source.image
        );
    }
    Ok(dir)
}

/// The backing file a qcow2 disk records (relative to the disk's directory), or None for a
/// standalone disk.
fn qcow2_backing(path: &Path) -> Result<Option<String>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut h = [0u8; 20];
    f.read_exact(&mut h)
        .with_context(|| format!("read the qcow2 header of {}", path.display()))?;
    if &h[0..4] != b"QFI\xfb" {
        bail!("{} is not a qcow2 disk", path.display());
    }
    let offset = u64::from_be_bytes(h[8..16].try_into()?);
    let size = u32::from_be_bytes(h[16..20].try_into()?);
    if offset == 0 || size == 0 {
        return Ok(None);
    }
    if size > 1023 {
        bail!("{} names a backing file of {size} bytes", path.display());
    }
    let mut name = vec![0u8; size as usize];
    f.seek(SeekFrom::Start(offset))
        .and_then(|_| f.read_exact(&mut name))
        .with_context(|| format!("read the backing file name of {}", path.display()))?;
    Ok(Some(
        String::from_utf8(name).context("the backing file name is not UTF-8")?,
    ))
}

/// Clone checkpoint `cp`'s disk and firmware vars into a new instance (`fs::copy` is an APFS
/// clone here), owner-writable. The disk keeps its relative backing path
/// (`../../images/<base>`), which resolves from the instance directory; that is checked. No
/// resume marker: a VM made from a checkpoint always boots cold.
fn clone_checkpoint_files(cp: &Path, inst: &Instance) -> Result<()> {
    let (disk, vars) = (cp.join("disk.qcow2"), cp.join("vars.fd"));
    if !(disk.is_file() && vars.is_file()) {
        bail!("no_such_checkpoint: the checkpoint was deleted before it could be copied");
    }
    let _ = std::fs::remove_file(inst.resume_marker());
    let _ = std::fs::remove_file(inst.dir.join("tz"));
    std::fs::copy(&disk, inst.disk()).context("copy the checkpoint's disk")?;
    set_writable(&inst.disk())?;
    std::fs::copy(&vars, inst.vars()).context("copy the checkpoint's firmware vars")?;
    set_writable(&inst.vars())?;
    if let Some(backing) = qcow2_backing(&inst.disk())?
        && !inst.dir.join(&backing).is_file()
    {
        bail!("the checkpoint's disk is backed by {backing}, which is missing");
    }
    std::fs::write(inst.dir.join("base"), "checkpoint")?;
    Ok(())
}

/// Run `copy` holding the source VM's lock (so checkpoint_vm replacing that label, or
/// delete_checkpoint, can't hand over a half-written pair), then the image lock (as
/// `clone_image_disk` takes it, so a build, pull or snapshot can't replace the backing files
/// mid-copy). The order matches reset's (instance lock, then image lock); builds take the
/// image lock and then creation_lock, which this never takes.
fn copy_checkpoint_locked(
    source_lock: &Path,
    image_lock: &Path,
    source_exists: impl FnOnce() -> bool,
    copy: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let _source = crate::instance::lock(
        source_lock,
        Some("waiting for another operation on the source VM to finish"),
    )?;
    if !source_exists() {
        bail!("no_such_vm: the source VM was deleted before its checkpoint could be copied");
    }
    let _image = crate::instance::lock(
        image_lock,
        Some("waiting for another build or download of this image to finish"),
    )?;
    copy()
}

/// Run `fill` on a just-reserved instance; if it fails, remove the instance's directory so
/// nothing half-made remains.
fn fill_or_discard(inst: &Instance, fill: impl FnOnce() -> Result<()>) -> Result<()> {
    fill().inspect_err(|_| {
        let _ = std::fs::remove_dir_all(&inst.dir);
    })
}

/// Checkpoint labels of an instance, oldest first.
pub fn checkpoints(inst: &Instance) -> Vec<String> {
    let mut found: Vec<(std::time::SystemTime, String)> =
        std::fs::read_dir(inst.dir.join("checkpoints"))
            .into_iter()
            .flatten()
            .flatten()
            // Skip in-progress `.partial-*` siblings (labels never start with a dot).
            .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
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

/// How long checkpoint_vm waits for a running guest to write its pending data before pausing it.
const FLUSH_LIMIT: Duration = Duration::from_secs(10);

/// Whether checkpoint_vm got the guest's pending writes onto its disk first.
#[derive(Debug, PartialEq)]
enum Flush {
    /// The VM was stopped: nothing to flush, and the guest is not touched.
    Stopped,
    Flushed,
    /// It failed or timed out (why); the checkpoint is still taken.
    NotFlushed(String),
}

/// Flush only a running VM, and never fail on it.
fn flush_step(running: bool, flush: impl FnOnce() -> Result<()>) -> Flush {
    if !running {
        return Flush::Stopped;
    }
    match flush() {
        Ok(()) => Flush::Flushed,
        Err(e) => Flush::NotFlushed(format!("{e:#}")),
    }
}

fn flush_command(os: Os) -> String {
    match os {
        Os::Ubuntu | Os::Arch => crate::guest::LINUX_FLUSH.to_string(),
        Os::Windows => windows_command(crate::guest::WINDOWS_FLUSH),
    }
}

/// Ask the guest over SSH to write its pending data to disk, within FLUSH_LIMIT.
fn flush_guest(inst: &Instance) -> Result<()> {
    let out = ssh_within(inst, &flush_command(inst.os), FLUSH_LIMIT).with_context(|| {
        format!(
            "the guest did not finish within {} s",
            FLUSH_LIMIT.as_secs()
        )
    })?;
    if !out.status.success() {
        bail!("the guest's flush failed ({})", out.status);
    }
    Ok(())
}

fn checkpoint_saved_text(label: &str, name: &str, kind: &str, flush: &Flush) -> String {
    let saved = format!("checkpoint {label} of {name} saved ({kind})");
    match flush {
        Flush::NotFlushed(why) => format!(
            "{saved}; not flushed: {why}. The guest's latest writes may be missing from its disk, \
             so a VM made from this checkpoint with create_vm(from_checkpoint) could lack them; \
             restore_vm is unaffected (it resumes the saved memory)."
        ),
        Flush::Stopped | Flush::Flushed => saved,
    }
}

/// Save the instance's disk and, if it's running, its RAM under `label` (replacing an
/// older checkpoint of that name). A running VM first writes its pending data to disk (bounded
/// by FLUSH_LIMIT; a failure is reported, not fatal), then pauses for a few seconds and carries
/// on. Disk copies are APFS clones, so they take no space until the VM writes more.
pub fn checkpoint(inst: &Instance, label: &str) -> Result<String> {
    let _lock = inst.lock()?;
    let dir = checkpoint_dir(inst, label)?;
    // A checkpoint clones the disk and, for a running VM, writes a RAM-sized state file.
    let (mem_gb, _) = inst.size();
    ensure_free_space(u64::from(mem_gb) + 2, "checkpoint")?;
    // Before the pause: a VM made from this checkpoint (create_vm from_checkpoint) boots cold
    // from its disk, which then has to hold what the guest still kept in its page cache.
    let flush = flush_step(inst.running(), || flush_guest(inst));
    if let Flush::NotFlushed(why) = &flush {
        log!(
            "{}: the guest did not flush its writes ({why}); checkpointing anyway",
            inst.name
        );
    }
    // Sibling that can't be a valid label (labels never start with a dot), so `v1` and
    // `v1.2` get distinct temps — `with_extension("partial")` would collide them.
    let tmp = inst
        .dir
        .join("checkpoints")
        .join(format!(".partial-{label}"));
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
    Ok(checkpoint_saved_text(label, &inst.name, kind, &flush))
}

/// Put the instance back exactly as it was at checkpoint `label` and start it: it resumes
/// in seconds if the checkpoint has its memory, else it boots. Port forwards (TCP and UDP)
/// are dropped, as on stop and reset.
pub fn restore(inst: &Instance, label: &str) -> Result<String> {
    let _lock = inst.lock()?;
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
    stop_forwards(inst);
    qemu::quit(inst);
    // Clone (APFS) into a temp in the same dir, then rename over the live files, so an
    // interrupted restore can't leave a half-written disk. fs::copy is a clonefile here.
    let disk_tmp = inst.dir.join(".disk.restore.qcow2");
    let vars_tmp = inst.dir.join(".vars.restore.fd");
    let _ = std::fs::remove_file(&disk_tmp);
    let _ = std::fs::remove_file(&vars_tmp);
    std::fs::copy(dir.join("disk.qcow2"), &disk_tmp)?;
    std::fs::copy(dir.join("vars.fd"), &vars_tmp)?;
    set_writable(&disk_tmp)?;
    set_writable(&vars_tmp)?;
    std::fs::rename(&disk_tmp, inst.disk())?;
    std::fs::rename(&vars_tmp, inst.vars())?;
    let _ = std::fs::remove_file(inst.resume_marker());
    let state = Some(dir.join("state")).filter(|p| p.is_file());
    boot_from(inst, state.as_deref())
}

pub fn delete_checkpoint(inst: &Instance, label: &str) -> Result<String> {
    let _lock = inst.lock()?;
    let dir = checkpoint_dir(inst, label)?;
    if !dir.is_dir() {
        bail!("{} has no checkpoint '{label}'", inst.name);
    }
    std::fs::remove_dir_all(&dir)?;
    Ok(format!("deleted checkpoint {label} of {}", inst.name))
}

/// A resumed guest keeps the clock it had when the snapshot was taken, so HTTPS fails
/// ("certificate is not yet valid") until its own time sync catches up, ~20 s later on
/// Ubuntu. Set it from the Mac's clock straight away.
fn sync_clock(inst: &Instance) {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let cmd = match inst.os {
        Os::Windows => format!(
            "Set-Date -Date ([DateTimeOffset]::FromUnixTimeMilliseconds({ms}).LocalDateTime) | Out-Null"
        ),
        Os::Ubuntu | Os::Arch => format!("sudo date -s @{}.{:03} >/dev/null", ms / 1000, ms % 1000),
    };
    if let Err(e) = ssh(inst, &cmd).and_then(|o| {
        o.status
            .success()
            .then_some(())
            .ok_or_else(|| anyhow::anyhow!(String::from_utf8_lossy(&o.stderr).trim().to_string()))
    }) {
        log!("{}: clock sync failed: {e:#}", inst.name);
    }
    sync_timezone(inst);
}

/// Set the guest's time zone to the Mac's (from `/etc/localtime`). Best-effort: unknown
/// zones or an offline guest are ignored. The zone last applied is recorded in the instance
/// dir so a resume skips it (the Windows lookup compiles a helper and costs ~1.5 s); a fresh
/// disk clears the record.
fn sync_timezone(inst: &Instance) {
    let Some(tz) = crate::instance::mac_timezone() else {
        return;
    };
    let marker = inst.dir.join("tz");
    if std::fs::read_to_string(&marker).is_ok_and(|t| t == tz) {
        return;
    }
    let cmd = match inst.os {
        Os::Ubuntu | Os::Arch => format!("sudo timedatectl set-timezone {tz}"),
        // Windows PowerShell 5.1 (.NET Framework) can't map IANA ids, but Windows ships ICU.
        Os::Windows => windows_command(&format!(
            r#"Add-Type -TypeDefinition @"
using System; using System.Runtime.InteropServices; using System.Text;
public static class AgentpcIcu {{
  [DllImport("icu.dll", CharSet = CharSet.Unicode)]
  static extern int ucal_getWindowsTimeZoneID(string id, int len, StringBuilder w, int cap, ref int st);
  public static string Win(string iana) {{
    var sb = new StringBuilder(128); int st = 0;
    int n = ucal_getWindowsTimeZoneID(iana, iana.Length, sb, sb.Capacity, ref st);
    return (st > 0 || n <= 0) ? null : sb.ToString(0, n);
  }}
}}
"@
$w = [AgentpcIcu]::Win('{tz}')
if (-not $w) {{ exit 1 }}
tzutil /s $w"#
        )),
    };
    if ssh(inst, &cmd).is_ok_and(|o| o.status.success()) {
        let _ = std::fs::write(&marker, &tz);
    }
}

/// Wrap a PowerShell command for the guest's SSH shell (Windows PowerShell 5.1), which decodes
/// the command line and its own output with the OEM code page and mangles non-ASCII text. The
/// script travels as base64 UTF-8 and runs dot-sourced, and the exit code keeps `-Command`
/// semantics: an explicit `exit N`, else 1 if the last statement failed.
pub fn windows_command(script: &str) -> String {
    use base64::Engine;
    let b64 =
        base64::engine::general_purpose::STANDARD.encode(format!("{script}\n$__agentpc_ok = $?"));
    format!(
        "[Console]::OutputEncoding = [Text.Encoding]::UTF8; \
         . ([scriptblock]::Create([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{b64}')))); \
         if (-not $__agentpc_ok) {{ exit 1 }}"
    )
}

thread_local! {
    static PROGRESS: std::cell::RefCell<Option<tokio::sync::mpsc::UnboundedSender<String>>> =
        const { std::cell::RefCell::new(None) };
}

/// Pass this thread's `log!` lines to `tx` while `f` runs; the MCP server turns them into
/// progress notifications. Lines from threads `f` starts itself aren't forwarded.
pub fn with_progress<T>(
    tx: tokio::sync::mpsc::UnboundedSender<String>,
    f: impl FnOnce() -> T,
) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            PROGRESS.with(|p| p.borrow_mut().take());
        }
    }
    PROGRESS.with(|p| *p.borrow_mut() = Some(tx));
    let _reset = Reset;
    f()
}

pub fn progress(line: &str) {
    PROGRESS.with(|p| {
        if let Some(tx) = p.borrow().as_ref() {
            let _ = tx.send(line.to_string());
        }
    });
}

pub fn stop(inst: &Instance) -> Result<String> {
    let _lock = inst.lock()?;
    stop_forwards(inst);
    qemu::stop(inst)?;
    viewer::stop_if_idle();
    Ok(format!("{} stopped", inst.name))
}

// reset and delete discard the disk, so a clean guest shutdown would be wasted time.
pub fn reset(inst: &Instance) -> Result<String> {
    let _lock = inst.lock()?;
    stop_forwards(inst);
    qemu::quit(inst);
    clone_image_disk(inst)?;
    boot_locked(inst)
}

pub fn delete(inst: &Instance) -> Result<String> {
    let _lock = inst.lock()?;
    stop_forwards(inst);
    qemu::quit(inst);
    std::fs::remove_dir_all(&inst.dir)?;
    viewer::stop_if_idle();
    Ok(format!("removed {}", inst.name))
}

/// VMs and images as JSON: `agentpc list --json` and the MCP `list_vms` tool.
pub fn list_json() -> Result<String> {
    use serde_json::json;
    let instances: Vec<_> = Instance::list()?
        .iter()
        .map(|i| {
            let (memory_gb, cpus) = i.size();
            let running = i.running();
            let mut obj = json!({
                "name": i.name,
                "os": i.os,
                "image": i.image.to_string(),
                "state": if running { qemu::status(i).unwrap_or_else(|| "running".into()) } else { "stopped".to_string() },
                "slot": i.slot,
                "memory_gb": memory_gb,
                "cpus": cpus,
                "offline": i.offline(),
                "ssh_port": i.ssh_port(),
                "checkpoints": checkpoints(i),
                "owner": owner(i),
            });
            // false: the MCP server that owns it has exited, so the VM is no one's now.
            if let Some(alive) = owner_alive(i) {
                obj["owner_running"] = json!(alive);
            }
            // A stopped VM has no viewer to point at.
            if running {
                obj["viewer"] = json!(viewer::url(i));
                if i.image.x86_apps() {
                    obj["x86_tso"] = json!(x86_tso(i));
                }
            }
            obj
        })
        .collect();
    let images: Vec<_> = Image::all()
        .into_iter()
        .map(|image| {
            let info = crate::image::read_info(&image);
            json!({
                "image": image.to_string(),
                "os": image.os,
                "version": info.as_ref().map(|i| i.version.clone()),
                // `base` matches the ImageInfo field and `agentpc image info` output.
                "base": info.as_ref().map(|i| i.base.clone()),
                "desktop_server": info.as_ref().map(|i| i.desktop_server.clone()),
                "fast_start": image.has_snapshot(),
                // Set when the image's guest setup predates this agentpc: how to refresh it.
                "outdated": crate::image::outdated(&image),
            })
        })
        .collect();
    Ok(serde_json::to_string_pretty(
        &json!({"instances": instances, "images": images}),
    )?)
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

// --- Ownership (who created a VM through MCP) -------------------------------------------

/// Record an owner string (MCP client name + session id) on a VM, with this process's id and
/// start time (`owner.pid`), so a later server can tell whether the owner is still running.
/// "keep" marks an owner run with AGENTPC_KEEP_RUNNING=1, whose VMs outlive it on purpose.
pub fn set_owner(inst: &Instance, owner: &str) -> Result<()> {
    std::fs::write(inst.dir.join("owner"), owner)?;
    let pid = std::process::id();
    let start = process_start(pid).unwrap_or_default();
    let keep = if keep_running() { "\nkeep" } else { "" };
    std::fs::write(
        inst.dir.join("owner.pid"),
        format!("{pid}\n{start}{keep}\n"),
    )?;
    Ok(())
}

/// Make `owner` the VM's owner, under its lock: an orphan stop or another session's exit,
/// which check the owner under the same lock, then see this one.
pub fn claim(inst: &Instance, owner: &str) -> Result<()> {
    let _lock = inst.lock()?;
    set_owner(inst, owner)
}

/// Stop a session's VM within its shutdown deadline, including lock acquisition.
/// Ownership and existence are checked only after the guard is held and remain protected
/// throughout graceful shutdown and any force-quit.
pub fn stop_if_owned(inst: &Instance, tag: &str, deadline: Instant) -> Result<bool> {
    shutdown_locked(
        &inst.lock_path(),
        deadline,
        || inst.dir.is_dir() && inst.running() && owner(inst).is_some_and(|o| o.ends_with(tag)),
        |deadline| {
            stop_forwards(inst);
            qemu::stop_until(inst, deadline)?;
            viewer::stop_if_idle();
            Ok(())
        },
    )
}

fn shutdown_locked(
    path: &Path,
    deadline: Instant,
    still_owned: impl FnOnce() -> bool,
    stop: impl FnOnce(Instant) -> Result<()>,
) -> Result<bool> {
    let Some(_guard) = crate::instance::lock_until(path, deadline)? else {
        return Ok(false);
    };
    if !still_owned() {
        return Ok(false);
    }
    stop(deadline)?;
    Ok(true)
}

/// AGENTPC_KEEP_RUNNING=1: VMs an MCP server starts outlive it.
pub fn keep_running() -> bool {
    std::env::var_os("AGENTPC_KEEP_RUNNING").is_some_and(|v| v == "1")
}

/// A process's start time as `ps` prints it, which tells it apart from a later process that
/// was given the same pid.
fn process_start(pid: u32) -> Option<String> {
    let out = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

/// Whether the MCP server that owns a VM is still running. None when that isn't recorded: a
/// CLI-made VM, or one owned before agentpc recorded it.
pub fn owner_alive(inst: &Instance) -> Option<bool> {
    let s = std::fs::read_to_string(inst.dir.join("owner.pid")).ok()?;
    let mut lines = s.lines();
    let pid: u32 = lines.next()?.trim().parse().ok()?;
    let start = lines.next()?.trim();
    Some(!start.is_empty() && process_start(pid).as_deref() == Some(start))
}

/// Whether a VM's owner asked for its VMs to outlive it (AGENTPC_KEEP_RUNNING=1).
fn owner_keeps(inst: &Instance) -> bool {
    std::fs::read_to_string(inst.dir.join("owner.pid"))
        .is_ok_and(|s| s.lines().any(|l| l.trim() == "keep"))
}

/// Stop the running VMs whose MCP server ended without stopping them (it was killed, or
/// crashed), as that server would have on a normal exit. Their owner tag stays, so
/// `list_vms` shows whose they were. Returns the names stopped.
pub fn stop_orphans() -> Vec<String> {
    let Ok(all) = Instance::list() else {
        return vec![];
    };
    let mut stopped = vec![];
    let orphan = |i: &Instance| i.running() && owner_alive(i) == Some(false) && !owner_keeps(i);
    for inst in all {
        if !orphan(&inst) {
            continue;
        }
        // Again under the VM's lock: a session may have adopted it in between.
        let Ok(_lock) = inst.lock() else { continue };
        if orphan(&inst) {
            stop_forwards(&inst);
            if qemu::stop(&inst).is_ok() {
                stopped.push(inst.name.clone());
            }
        }
    }
    if !stopped.is_empty() {
        viewer::stop_if_idle();
    }
    stopped
}

/// The owner recorded on a VM, if any (CLI-created VMs have none).
pub fn owner(inst: &Instance) -> Option<String> {
    std::fs::read_to_string(inst.dir.join("owner"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

// --- Port forwards ----------------------------------------------------------------------

fn read_forward(pid_file: &Path) -> Option<(i32, u16)> {
    let s = std::fs::read_to_string(pid_file).ok()?;
    let mut lines = s.lines();
    let pid = lines.next()?.trim().parse().ok()?;
    let guest = lines.next().unwrap_or("").trim().parse().unwrap_or(0);
    Some((pid, guest))
}

fn pid_alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { c_kill(pid, 0) == 0 }
}

/// Whether `pid` is still the SSH tunnel for `host_port`. The pid file outlives a tunnel that
/// died with its server, and macOS hands its pid to another process sooner or later; that one
/// must not be reported as the tunnel, or sent its SIGTERM.
fn tunnel_alive(pid: i32, host_port: u16) -> bool {
    if !pid_alive(pid) {
        return false;
    }
    let Ok(out) = Command::new("ps")
        .args(["-ww", "-o", "command=", "-p", &pid.to_string()])
        .output()
    else {
        return false;
    };
    let cmd = String::from_utf8_lossy(&out.stdout);
    cmd.split_whitespace()
        .next()
        .is_some_and(|c| c.ends_with("ssh"))
        && cmd.contains(&format!("127.0.0.1:{host_port}:"))
}

/// Active forwards on a VM as `(host_port, protocol, guest_port, alive)`, sorted by host port.
/// A TCP forward is alive while its tunnel runs, a UDP one while the QEMU that holds it does.
pub fn list_forwards(inst: &Instance) -> Vec<(u16, Protocol, u16, bool)> {
    let mut out = Vec::new();
    let mut qemu_pid = None;
    if let Ok(entries) = std::fs::read_dir(inst.dir.join("forwards")) {
        for e in entries.flatten() {
            let p = e.path();
            let Some((protocol, host)) = forward_record_name(&p) else {
                continue;
            };
            let Some((pid, guest)) = read_forward(&p) else {
                continue;
            };
            let alive = match protocol {
                Protocol::Tcp => tunnel_alive(pid, host),
                Protocol::Udp => *qemu_pid.get_or_insert_with(|| inst.pid()) == Some(pid),
            };
            out.push((host, protocol, guest, alive));
        }
    }
    out.sort();
    out
}

/// The protocol and host port of a forward record (`<host_port>.pid` or `<host_port>.udp`).
fn forward_record_name(p: &Path) -> Option<(Protocol, u16)> {
    let protocol = match p.extension()?.to_str()? {
        "pid" => Protocol::Tcp,
        "udp" => Protocol::Udp,
        _ => return None,
    };
    Some((protocol, p.file_stem()?.to_str()?.parse().ok()?))
}

/// One line per forward, for the CLI and the MCP `list_forwards` tool.
pub fn forwards_text(inst: &Instance) -> String {
    let list = list_forwards(inst);
    if list.is_empty() {
        return format!("{}: no forwarded ports", inst.name);
    }
    list.into_iter()
        .map(|(h, protocol, g, alive)| {
            format!(
                "{} 127.0.0.1:{h} -> {}:{g}{}",
                protocol.as_str(),
                inst.name,
                if alive { "" } else { " (down)" }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Tear down the forward on a host port: the given protocol's, or with `None` every
/// protocol's (TCP and UDP ports are separate, so both can exist).
pub fn remove_forward(
    inst: &Instance,
    host_port: u16,
    protocol: Option<Protocol>,
) -> Result<String> {
    let _lock = inst.lock()?;
    let dir = inst.dir.join("forwards");
    let mut removed = Vec::new();
    for p in [Protocol::Tcp, Protocol::Udp] {
        if protocol.is_some_and(|want| want != p) {
            continue;
        }
        let record = dir.join(format!("{host_port}.{}", p.record_ext()));
        let Some((pid, _)) = read_forward(&record) else {
            continue;
        };
        match p {
            Protocol::Tcp => {
                if tunnel_alive(pid, host_port) {
                    crate::instance::kill(pid, 15);
                }
                let _ = std::fs::remove_file(dir.join(format!("{host_port}.log")));
            }
            // Only the QEMU that added the hostfwd holds it; an older record is already dead.
            // "not found" means it's gone too, so any reply still lets the record go.
            Protocol::Udp if inst.pid() == Some(pid) => {
                qemu::Qmp::connect(inst)?.hmp(&hostfwd_remove_cmd(host_port))?;
            }
            Protocol::Udp => {}
        }
        let _ = std::fs::remove_file(&record);
        removed.push(p.as_str());
    }
    if removed.is_empty() {
        bail!(
            "{}: no {}forward on port {host_port}",
            inst.name,
            protocol
                .map(|p| format!("{} ", p.as_str()))
                .unwrap_or_default()
        );
    }
    Ok(format!(
        "removed the {} forward on 127.0.0.1:{host_port}",
        removed.join(" and ")
    ))
}

/// Tear down every forward of a VM. Call on stop/delete/reset/restore: the tunnels and
/// UDP hostfwds die with the VM anyway, but their records should not linger. UDP hostfwds
/// are left to die with QEMU, which every caller is about to stop.
pub fn stop_forwards(inst: &Instance) {
    if let Ok(entries) = std::fs::read_dir(inst.dir.join("forwards")) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "pid")
                && let Some((pid, _)) = read_forward(&p)
                && let Some((_, host)) = forward_record_name(&p)
                && tunnel_alive(pid, host)
            {
                crate::instance::kill(pid, 15);
            }
            let _ = std::fs::remove_file(&p);
        }
    }
}

// --- VM logs ----------------------------------------------------------------------------

/// The tail of a VM's `qemu.log` or `serial.log`.
pub fn read_log(inst: &Instance, which: &str, tail: usize) -> Result<String> {
    let file = match which {
        "qemu" => "qemu.log",
        "serial" => "serial.log",
        _ => bail!("log must be 'qemu' or 'serial'"),
    };
    let path = inst.dir.join(file);
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(tail);
    Ok(lines[start..].join("\n"))
}

unsafe extern "C" {
    #[link_name = "kill"]
    fn c_kill(pid: i32, sig: i32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_flushes_only_a_running_guest_and_never_fails_on_it() {
        let mut called = false;
        assert_eq!(
            flush_step(false, || {
                called = true;
                Ok(())
            }),
            Flush::Stopped
        );
        assert!(!called, "a stopped VM is not touched");
        assert_eq!(flush_step(true, || Ok(())), Flush::Flushed);
        assert_eq!(
            flush_step(true, || Err(anyhow::anyhow!(
                "the guest did not finish within 10 s"
            ))),
            Flush::NotFlushed("the guest did not finish within 10 s".into())
        );
    }

    #[test]
    fn a_checkpoint_that_was_not_flushed_says_so() {
        assert_eq!(
            checkpoint_saved_text("v1", "vm", "disk and memory", &Flush::Flushed),
            "checkpoint v1 of vm saved (disk and memory)"
        );
        assert_eq!(
            checkpoint_saved_text("v1", "vm", "disk", &Flush::Stopped),
            "checkpoint v1 of vm saved (disk)"
        );
        let t = checkpoint_saved_text(
            "v1",
            "vm",
            "disk and memory",
            &Flush::NotFlushed("timed out".into()),
        );
        assert!(
            t.starts_with("checkpoint v1 of vm saved (disk and memory); not flushed: timed out"),
            "{t}"
        );
    }

    #[test]
    fn the_flush_command_matches_the_guest() {
        assert_eq!(flush_command(Os::Ubuntu), "sync");
        assert_eq!(flush_command(Os::Arch), "sync");
        assert_eq!(
            flush_command(Os::Windows),
            windows_command(crate::guest::WINDOWS_FLUSH)
        );
        assert_eq!(FLUSH_LIMIT, Duration::from_secs(10));
    }

    /// A fresh directory under the system temp dir. The counter keeps roots apart within a
    /// process (the clock alone ticks once per microsecond on macOS, so parallel tests collided);
    /// the time keeps them apart across runs that reuse a pid.
    fn shutdown_fixture() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "agentpc-shutdown-test-{}-{unique}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// Parallel tests each get their own root, even ones that start in the same clock tick.
    #[test]
    fn fixture_roots_are_unique_within_a_process() {
        let start = std::sync::Arc::new(std::sync::Barrier::new(16));
        let roots: Vec<PathBuf> = (0..16)
            .map(|_| {
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    shutdown_fixture()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|t| t.join().unwrap())
            .collect();
        let distinct: std::collections::HashSet<&PathBuf> = roots.iter().collect();
        for r in &roots {
            let _ = std::fs::remove_dir_all(r);
        }
        assert_eq!(distinct.len(), roots.len(), "fixture roots collide");
    }

    #[test]
    fn shutdown_retains_guard_through_owner_check_and_force_work() {
        let root = shutdown_fixture();
        let path = root.join("vm.lock");
        assert!(
            shutdown_locked(
                &path,
                Instant::now() + Duration::from_secs(1),
                || {
                    assert!(crate::instance::try_lock(&path).is_none());
                    true
                },
                |_| {
                    assert!(crate::instance::try_lock(&path).is_none());
                    Ok(())
                },
            )
            .unwrap()
        );
        assert!(crate::instance::try_lock(&path).is_some());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shutdown_rechecks_owner_after_waiting_and_skips_a_deleted_instance() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let root = shutdown_fixture();
        let path = root.join("vm.lock");
        let guard = crate::instance::lock(&path, None).unwrap();
        let owned = Arc::new(AtomicBool::new(true));
        let stop_called = Arc::new(AtomicBool::new(false));
        let worker_path = path.clone();
        let worker_owned = owned.clone();
        let worker_stop = stop_called.clone();
        let worker = std::thread::spawn(move || {
            shutdown_locked(
                &worker_path,
                Instant::now() + Duration::from_secs(1),
                || worker_owned.load(Ordering::SeqCst),
                |_| {
                    worker_stop.store(true, Ordering::SeqCst);
                    Ok(())
                },
            )
        });
        owned.store(false, Ordering::SeqCst);
        drop(guard);
        assert!(!worker.join().unwrap().unwrap());
        assert!(!stop_called.load(Ordering::SeqCst));
        assert!(
            !shutdown_locked(
                &path,
                Instant::now() + Duration::from_secs(1),
                || false,
                |_| panic!("deleted VM must not be stopped")
            )
            .unwrap()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shutdown_deadline_includes_time_waiting_for_another_operation() {
        let root = shutdown_fixture();
        let path = root.join("vm.lock");
        let guard = crate::instance::lock(&path, None).unwrap();
        let start = Instant::now();
        assert!(
            !shutdown_locked(
                &path,
                start + Duration::from_millis(30),
                || panic!("must not check without guard"),
                |_| panic!("must not stop without guard")
            )
            .unwrap()
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        drop(guard);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn udp_hostfwd_commands() {
        assert_eq!(
            hostfwd_add_cmd(5000, 53),
            "hostfwd_add net0 udp:127.0.0.1:5000-:53"
        );
        assert_eq!(
            hostfwd_remove_cmd(5000),
            "hostfwd_remove net0 udp:127.0.0.1:5000"
        );
    }

    #[test]
    fn forward_record_names() {
        let p = |s: &str| forward_record_name(Path::new(s));
        assert_eq!(p("/x/forwards/8080.pid"), Some((Protocol::Tcp, 8080)));
        assert_eq!(p("/x/forwards/5000.udp"), Some((Protocol::Udp, 5000)));
        assert_eq!(p("/x/forwards/8080.log"), None);
        assert_eq!(p("/x/forwards/nope.udp"), None);
        assert_eq!(p("/x/forwards/70000.udp"), None);
    }

    #[test]
    fn reads_forward_records() {
        let dir = std::env::temp_dir().join(format!("agentpc-fwd-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let rec = dir.join("5000.udp");
        std::fs::write(&rec, "4242\n53\n").unwrap();
        assert_eq!(read_forward(&rec), Some((4242, 53)));
        std::fs::write(&rec, "garbage\n").unwrap();
        assert_eq!(read_forward(&rec), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn checks_instance_names() {
        for ok in ["ubuntu-1", "my.vm_2", "A", &"x".repeat(64)] {
            assert!(check_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "_build-ubuntu",
            ".hidden",
            "a/b",
            "..",
            "has space",
            "semi;colon",
            "ünïcode",
            &"x".repeat(65),
        ] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
    }

    /// A qcow2 header (version 3) recording `backing` as its backing file, as qemu-img writes it.
    fn fake_qcow2(path: &Path, backing: Option<&str>) {
        let mut h = vec![0u8; 512];
        h[0..4].copy_from_slice(b"QFI\xfb");
        h[4..8].copy_from_slice(&3u32.to_be_bytes());
        if let Some(b) = backing {
            h[8..16].copy_from_slice(&512u64.to_be_bytes());
            h[16..20].copy_from_slice(&(b.len() as u32).to_be_bytes());
            h.extend_from_slice(b.as_bytes());
        }
        std::fs::write(path, h).unwrap();
    }

    /// `<root>/images/ubuntu-24.04.qcow2`, source VM `src` with checkpoint `clean` (a disk backed
    /// by `../../images/ubuntu-24.04.qcow2` and read-only vars, as image files are) and a new,
    /// empty instance `new`, laid out as under $AGENTPC_HOME. Returns (root, source, new, cp).
    fn checkpoint_fixture() -> (PathBuf, Instance, Instance, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let root = shutdown_fixture();
        std::fs::create_dir_all(root.join("images")).unwrap();
        std::fs::write(root.join("images/ubuntu-24.04.qcow2"), b"base").unwrap();
        let cp = root.join("instances/src/checkpoints/clean");
        std::fs::create_dir_all(&cp).unwrap();
        fake_qcow2(
            &cp.join("disk.qcow2"),
            Some("../../images/ubuntu-24.04.qcow2"),
        );
        std::fs::write(cp.join("vars.fd"), b"vars").unwrap();
        std::fs::set_permissions(cp.join("vars.fd"), std::fs::Permissions::from_mode(0o444))
            .unwrap();
        let image: Image = "ubuntu".parse().unwrap();
        let at = |name: &str, slot| Instance {
            name: name.into(),
            os: Os::Ubuntu,
            image: image.clone(),
            slot,
            dir: root.join("instances").join(name),
        };
        let (source, new) = (at("src", 8), at("new", 9));
        std::fs::create_dir_all(&new.dir).unwrap();
        (root, source, new, cp)
    }

    #[test]
    fn checkpoint_refs_are_a_vm_and_a_label() {
        let r = CheckpointRef::parse("my.vm_2/v1.2").unwrap();
        assert_eq!((r.vm.as_str(), r.label.as_str()), ("my.vm_2", "v1.2"));
        assert_eq!(r.to_string(), "my.vm_2/v1.2");
        for bad in [
            "",
            "src",
            "src/",
            "/clean",
            "a/b/c",
            "_build/clean",
            ".hidden/clean",
            "src/.partial-x",
            "src/has space",
            "../x/clean",
            &format!("{}/clean", "x".repeat(65)),
        ] {
            let e = CheckpointRef::parse(bad).unwrap_err();
            assert!(
                format!("{e:#}").starts_with("invalid_checkpoint_ref"),
                "{bad}: {e:#}"
            );
        }
    }

    #[test]
    fn labels_follow_the_checkpoint_rules() {
        for ok in ["clean", "deps-installed", "v1.2", "a_b"] {
            assert!(check_label(ok).is_ok(), "{ok}");
        }
        for bad in ["", ".x", "a/b", "a b", "ü"] {
            assert!(check_label(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_source_checkpoint_and_image_are_checked_before_any_work() {
        let (root, source, _new, cp) = checkpoint_fixture();
        assert_eq!(validate_source(&source, "clean", None).unwrap(), cp);
        assert_eq!(
            validate_source(&source, "clean", Some("ubuntu")).unwrap(),
            cp
        );
        assert_eq!(
            validate_source(&source, "clean", Some("ubuntu-24.04")).unwrap(),
            cp
        );
        let e = format!(
            "{:#}",
            validate_source(&source, "absent", None).unwrap_err()
        );
        assert!(
            e.starts_with("no_such_checkpoint") && e.contains("clean"),
            "{e}"
        );
        // A disk without its vars is no checkpoint to copy.
        let half = source.dir.join("checkpoints/half");
        std::fs::create_dir_all(&half).unwrap();
        fake_qcow2(&half.join("disk.qcow2"), None);
        let e = format!("{:#}", validate_source(&source, "half", None).unwrap_err());
        assert!(e.starts_with("no_such_checkpoint"), "{e}");
        assert!(
            e.contains("it has: clean)"),
            "only complete checkpoints are offered: {e}"
        );
        for other in ["ubuntu-22.04", "windows-11-24h2", "arch", "nonsense!!"] {
            let e = format!(
                "{:#}",
                validate_source(&source, "clean", Some(other)).unwrap_err()
            );
            assert!(e.starts_with("image_mismatch"), "{other}: {e}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reads_a_qcow2_backing_file_name() {
        let root = shutdown_fixture();
        fake_qcow2(&root.join("a.qcow2"), Some("../../images/x.qcow2"));
        fake_qcow2(&root.join("b.qcow2"), None);
        std::fs::write(root.join("c.raw"), vec![0u8; 64]).unwrap();
        assert_eq!(
            qcow2_backing(&root.join("a.qcow2")).unwrap().as_deref(),
            Some("../../images/x.qcow2")
        );
        assert_eq!(qcow2_backing(&root.join("b.qcow2")).unwrap(), None);
        assert!(qcow2_backing(&root.join("c.raw")).is_err());
        // A backing name past the end of a truncated disk is refused with a reason.
        let mut cut = std::fs::read(root.join("a.qcow2")).unwrap();
        cut.truncate(512);
        std::fs::write(root.join("d.qcow2"), cut).unwrap();
        let e = format!("{:#}", qcow2_backing(&root.join("d.qcow2")).unwrap_err());
        assert!(e.contains("read the backing file name of"), "{e}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn clones_a_checkpoint_into_a_new_instance_that_boots_cold() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let (root, _source, new, cp) = checkpoint_fixture();
        // Left by nothing in practice; a cold boot must not find them.
        std::fs::write(new.resume_marker(), "").unwrap();
        std::fs::write(new.dir.join("tz"), "Europe/Berlin").unwrap();
        clone_checkpoint_files(&cp, &new).unwrap();
        assert_eq!(
            std::fs::read(new.disk()).unwrap(),
            std::fs::read(cp.join("disk.qcow2")).unwrap()
        );
        assert_eq!(std::fs::read(new.vars()).unwrap(), b"vars");
        for f in [new.disk(), new.vars()] {
            let mode = std::fs::metadata(&f).unwrap().permissions().mode();
            assert!(mode & 0o200 != 0, "{} writable", f.display());
        }
        assert_ne!(
            std::fs::metadata(new.disk()).unwrap().ino(),
            std::fs::metadata(cp.join("disk.qcow2")).unwrap().ino(),
            "a copy, not the checkpoint's own file"
        );
        let backing = qcow2_backing(&new.disk()).unwrap().unwrap();
        assert!(new.dir.join(&backing).is_file(), "{backing} resolves");
        assert!(!new.resume_marker().exists());
        assert!(!new.dir.join("tz").exists());
        assert_eq!(
            std::fs::read_to_string(new.dir.join("base")).unwrap(),
            "checkpoint"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_checkpoint_whose_backing_file_is_gone_is_refused() {
        let (root, _source, new, cp) = checkpoint_fixture();
        std::fs::remove_file(root.join("images/ubuntu-24.04.qcow2")).unwrap();
        let e = format!("{:#}", clone_checkpoint_files(&cp, &new).unwrap_err());
        assert!(e.contains("missing"), "{e}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_vanished_checkpoint_leaves_no_half_made_instance() {
        let (root, _source, new, cp) = checkpoint_fixture();
        std::fs::remove_dir_all(&cp).unwrap();
        let e = fill_or_discard(&new, || clone_checkpoint_files(&cp, &new)).unwrap_err();
        assert!(format!("{e:#}").starts_with("no_such_checkpoint"), "{e:#}");
        assert!(!new.dir.exists(), "the new instance directory is removed");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_successful_fill_keeps_the_instance() {
        let (root, _source, new, _cp) = checkpoint_fixture();
        fill_or_discard(&new, || Ok(())).unwrap();
        assert!(new.dir.is_dir());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_checkpoint_copy_holds_the_source_lock_then_the_image_lock() {
        use crate::instance::try_lock;
        let root = shutdown_fixture();
        let (src, img) = (root.join("src.lock"), root.join("img.lock"));
        copy_checkpoint_locked(
            &src,
            &img,
            || {
                assert!(try_lock(&src).is_none(), "the source lock is taken first");
                assert!(
                    try_lock(&img).is_some(),
                    "the image lock only after the source check"
                );
                true
            },
            || {
                assert!(try_lock(&src).is_none() && try_lock(&img).is_none());
                Ok(())
            },
        )
        .unwrap();
        assert!(
            try_lock(&src).is_some() && try_lock(&img).is_some(),
            "both released"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_source_deleted_while_waiting_is_not_copied() {
        let root = shutdown_fixture();
        let e = copy_checkpoint_locked(
            &root.join("src.lock"),
            &root.join("img.lock"),
            || false,
            || panic!("must not copy from a deleted VM"),
        )
        .unwrap_err();
        assert!(format!("{e:#}").starts_with("no_such_vm"), "{e:#}");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// reset_vm takes the instance lock, then the image lock; the copy takes the same order, so
    /// a reset of the source and a create from its checkpoint can't deadlock.
    #[test]
    fn the_checkpoint_copy_and_a_reset_of_the_source_never_deadlock() {
        use std::sync::mpsc;
        let root = shutdown_fixture();
        let (src, img) = (root.join("src.lock"), root.join("img.lock"));
        let (done, finished) = mpsc::channel();
        let (s, i, d) = (src.clone(), img.clone(), done.clone());
        let reset = std::thread::spawn(move || {
            for _ in 0..50 {
                let _a = crate::instance::lock(&s, None).unwrap();
                let _b = crate::instance::lock(&i, None).unwrap();
            }
            d.send(()).unwrap();
        });
        let (s, i) = (src.clone(), img.clone());
        let copy = std::thread::spawn(move || {
            for _ in 0..50 {
                copy_checkpoint_locked(&s, &i, || true, || Ok(())).unwrap();
            }
            done.send(()).unwrap();
        });
        for _ in 0..2 {
            finished
                .recv_timeout(Duration::from_secs(10))
                .expect("deadlock: lock order differs from reset's");
        }
        reset.join().unwrap();
        copy.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    fn output(code: i32, stdout: &str) -> std::process::Output {
        use std::os::unix::process::ExitStatusExt;
        std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    #[test]
    fn job_records_count_as_cleared_only_when_the_guest_says_so() {
        assert!(records_cleared(&output(0, "AGENTPC_CLEARED\n")).is_ok());
        assert!(records_cleared(&output(0, "login banner\r\nAGENTPC_CLEARED\r\n")).is_ok());
        assert!(records_cleared(&output(0, "")).is_err(), "no proof");
        assert!(records_cleared(&output(1, "AGENTPC_CLEARED\n")).is_err());
        assert!(records_cleared(&output(0, "not AGENTPC_CLEARED\n")).is_err());
    }

    /// Runs `retry_cleanup` against scripted guest answers (None = no answer); returns its
    /// result and how many attempts it made.
    fn cleanup_against(answers: Vec<Option<std::process::Output>>) -> (Result<()>, usize) {
        let mut answers = answers.into_iter();
        let mut calls = 0;
        let r = retry_cleanup(
            "vm",
            |_| {
                calls += 1;
                answers.next().expect("no more attempts expected")
            },
            Duration::from_secs(60),
            Duration::from_millis(1),
        );
        (r, calls)
    }

    #[test]
    fn the_cleanup_retries_a_guest_that_answers_without_confirming() {
        let (r, calls) = cleanup_against(vec![
            Some(output(1, "")),
            Some(output(0, "")),
            Some(output(0, "AGENTPC_CLEARED\n")),
        ]);
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(calls, 3);
    }

    #[test]
    fn the_cleanup_gives_up_after_its_attempts_or_when_the_guest_goes_silent() {
        let (r, calls) = cleanup_against(vec![Some(output(1, "")); RECORDS_ATTEMPTS as usize]);
        let e = format!("{:#}", r.unwrap_err());
        assert_eq!(calls, RECORDS_ATTEMPTS as usize);
        assert!(
            e.starts_with(&format!("after {RECORDS_ATTEMPTS} attempts")),
            "{e}"
        );
        // No answer at all is not retried.
        let (r, calls) = cleanup_against(vec![None]);
        assert_eq!(calls, 1);
        assert!(format!("{:#}", r.unwrap_err()).contains("did not answer"));
        // Silence after an answer says the guest was up but never confirmed.
        let (r, calls) = cleanup_against(vec![Some(output(1, "")), None]);
        assert_eq!(calls, 2);
        let e = format!("{:#}", r.unwrap_err());
        assert!(
            e.contains("after 2 attempts") && e.contains("no answer"),
            "{e}"
        );
    }

    #[test]
    fn a_vm_without_pending_records_is_never_sent_the_cleanup() {
        let (root, _source, new, _cp) = checkpoint_fixture();
        // No marker: Ok at once, with no SSH to the (absent) guest.
        clear_pending_records(&new).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_vm_records_the_checkpoint_it_was_made_from() {
        let (root, _source, new, _cp) = checkpoint_fixture();
        assert_eq!(created_from(&new), None);
        assert_eq!(from_note(&new), "");
        std::fs::write(new.dir.join(FROM_CHECKPOINT), "src/clean").unwrap();
        assert_eq!(
            created_from(&new),
            Some(CheckpointRef::parse("src/clean").unwrap())
        );
        let note = from_note(&new);
        assert!(
            note.contains("from checkpoint src/clean") && note.contains("booted fresh"),
            "{note}"
        );
        std::fs::write(new.dir.join(FROM_CHECKPOINT), "garbage").unwrap();
        assert_eq!(created_from(&new), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Reads the instances directory (no VM by that name exists) and creates nothing.
    #[test]
    fn create_from_checkpoint_refuses_an_absent_source_before_any_work() {
        let from = CheckpointRef::parse("agentpc-test-absent-source-7f3c/clean").unwrap();
        let e = create_from_checkpoint(
            &from,
            None,
            Some("agentpc-test-never-made-7f3c"),
            None,
            None,
            false,
            None,
        )
        .unwrap_err();
        assert!(format!("{e:#}").starts_with("no_such_vm"), "{e:#}");
        assert!(
            !crate::instance::instances_dir()
                .join("agentpc-test-never-made-7f3c")
                .exists()
        );
    }

    #[test]
    fn create_from_checkpoint_checks_the_size_first() {
        let from = CheckpointRef::parse("agentpc-test-absent-source-7f3c/clean").unwrap();
        let e = create_from_checkpoint(&from, None, None, Some(1), None, false, None).unwrap_err();
        assert_eq!(format!("{e:#}"), "memory must be 2-64 GB");
    }

    #[test]
    fn a_byte_order_mark_before_the_confirmation_is_ignored() {
        // Windows PowerShell 5.1 may start its UTF-8 output with U+FEFF.
        assert!(records_cleared(&output(0, "\u{feff}AGENTPC_CLEARED\r\n")).is_ok());
        assert!(records_cleared(&output(0, "\u{feff}\r\n")).is_err());
    }

    /// The step that runs before the first boot records both the origin and the pending cleanup.
    #[test]
    fn a_vm_made_from_a_checkpoint_records_its_origin_and_pending_cleanup() {
        let (root, _source, new, _cp) = checkpoint_fixture();
        let from = CheckpointRef::parse("src/clean").unwrap();
        record_checkpoint_origin(&new, &from).unwrap();
        assert!(new.dir.join(RECORDS_PENDING).is_file());
        assert_eq!(created_from(&new), Some(from));
        std::fs::remove_dir_all(root).unwrap();
    }

    /// reset_vm (clone_disk) makes a clean image install: it no longer came from a checkpoint.
    /// This tests the step clone_disk runs (calling clone_disk itself would spawn qemu-img,
    /// whose fork briefly holds other tests' lock descriptors).
    #[test]
    fn a_fresh_image_disk_forgets_the_checkpoint_it_replaces() {
        let (root, _source, new, _cp) = checkpoint_fixture();
        record_checkpoint_origin(&new, &CheckpointRef::parse("src/clean").unwrap()).unwrap();
        forget_checkpoint_origin(&new);
        assert!(!new.dir.join(RECORDS_PENDING).exists());
        assert!(!new.dir.join(FROM_CHECKPOINT).exists());
        assert_eq!(created_from(&new), None);
        assert_eq!(from_note(&new), "");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn only_a_checkpoint_made_vm_is_told_to_retry_with_from_checkpoint() {
        let (root, _source, new, _cp) = checkpoint_fixture();
        let e = format!("{:#}", not_ready(anyhow::anyhow!("boom"), &new));
        assert!(
            e.starts_with("boom") && !e.contains("from_checkpoint"),
            "{e}"
        );
        std::fs::write(new.dir.join(FROM_CHECKPOINT), "src/clean").unwrap();
        let e = format!("{:#}", not_ready(anyhow::anyhow!("boom"), &new));
        assert!(e.contains("from_checkpoint src/clean"), "{e}");
        std::fs::remove_dir_all(root).unwrap();
    }
}

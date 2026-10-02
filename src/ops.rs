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

pub const SSH_OPTS: [&str; 16] = [
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
    let os = image.os;
    // Checked before a download or build that can take minutes; rechecked below for races.
    Instance::check_size(memory, cpus)?;
    if let Some(n) = name {
        check_name(n)?;
        if crate::instance::instances_dir().join(n).exists() {
            bail!("instance '{n}' exists");
        }
    }
    check_memory(memory.unwrap_or(os.default_size().0))?;
    ensure_free_space(4, "create a VM")?;
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
    if crate::instance::instances_dir().join(&name).exists() {
        bail!("instance '{name}' exists");
    }
    let inst = Instance::create(&name, image, slot)?;
    drop(lock);
    let made = inst
        .set_size(memory, cpus)
        .and_then(|()| {
            if offline {
                std::fs::write(inst.dir.join("offline"), "")?;
            }
            Ok(())
        })
        .and_then(|()| clone_image_disk(&inst))
        // Before the first boot, so a VM that never becomes ready is still its owner's: a
        // retried create_vm boots it again, and the owner's exit stops it.
        .and_then(|()| owner.map_or(Ok(()), |o| set_owner(&inst, o)));
    if let Err(e) = made {
        let _ = std::fs::remove_dir_all(&inst.dir);
        return Err(e);
    }
    let note = crate::image::outdated(image).map(|h| format!("\n  note: the {image} image: {h}"));
    boot(&inst)
        .map(|s| s + &note.unwrap_or_default())
        .map_err(|e| {
            anyhow::anyhow!(
                "{e:#}\nThe VM '{name}' was created but didn't become ready (read_vm_log, or \
             {}, shows why). Starting it again retries, as does create_vm with the same name \
             from the same session; deleting it starts over.",
                inst.dir.join("qemu.log").display()
            )
        })
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

/// Start a stopped instance and wait until it's usable.
pub fn boot(inst: &Instance) -> Result<String> {
    let _lock = inst.lock()?;
    boot_locked(inst)
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

/// Save the instance's disk and, if it's running, its RAM under `label` (replacing an
/// older checkpoint of that name). A running VM pauses for a few seconds and carries on.
/// Disk copies are APFS clones, so they take no space until the VM writes more.
pub fn checkpoint(inst: &Instance, label: &str) -> Result<String> {
    let _lock = inst.lock()?;
    let dir = checkpoint_dir(inst, label)?;
    // A checkpoint clones the disk and, for a running VM, writes a RAM-sized state file.
    let (mem_gb, _) = inst.size();
    ensure_free_space(u64::from(mem_gb) + 2, "checkpoint")?;
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
    Ok(format!(
        "checkpoint {label} of {} saved ({kind})",
        inst.name
    ))
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
}

//! Launching QEMU (HVF) and driving it over QMP.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::instance::{Instance, Os, kill};
use crate::log;

pub fn qemu_bin() -> Result<PathBuf> {
    which("qemu-system-aarch64")
        .with_context(|| format!("qemu not found; {}", crate::setup::QEMU_HINT))
}

/// EDK2 firmware shipped next to the QEMU binary (<prefix>/share/qemu).
pub fn edk2() -> Result<PathBuf> {
    let bin = qemu_bin()?;
    for base in [bin.clone(), std::fs::canonicalize(&bin).unwrap_or(bin)] {
        if let Some(prefix) = base.parent().and_then(Path::parent) {
            let fw = prefix.join("share/qemu/edk2-aarch64-code.fd");
            if fw.is_file() {
                return Ok(fw);
            }
        }
    }
    bail!(
        "edk2-aarch64-code.fd not found next to {}",
        qemu_bin()?.display()
    )
}

pub fn which(cmd: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|d| Path::new(d).join(cmd))
        .find(|p| p.is_file())
}

/// The concrete versioned machine type that `virt` currently aliases (e.g. `virt-11.1`),
/// from `-machine help`. Saved states record it so they resume on the exact machine they
/// were made on; a fresh boot uses whatever `virt` means today. Falls back to `virt`.
pub fn machine_type() -> String {
    static CACHED: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CACHED
        .get_or_init(|| {
            let Ok(bin) = qemu_bin() else {
                return "virt".into();
            };
            let Ok(out) = Command::new(bin).args(["-machine", "help"]).output() else {
                return "virt".into();
            };
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .find_map(|l| {
                    let mut it = l.split_whitespace();
                    (it.next() == Some("virt")).then(|| {
                        l.split_once("alias of ")
                            .and_then(|(_, r)| {
                                r.trim().trim_end_matches(')').split_whitespace().next()
                            })
                            .unwrap_or("virt")
                            .to_string()
                    })
                })
                .unwrap_or_else(|| "virt".into())
        })
        .clone()
}

/// Sidecar next to a saved-state file recording the machine type it was captured on.
fn machine_sidecar(state: &Path) -> PathBuf {
    let mut s = state.as_os_str().to_owned();
    s.push(".machine");
    PathBuf::from(s)
}

/// The machine type to launch a resumed state with: its recorded sidecar, or `virt` for a
/// legacy state saved before sidecars existed.
fn resume_machine(state: &Path) -> String {
    std::fs::read_to_string(machine_sidecar(state))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "virt".into())
}

/// Boot an instance in the background. `extra` adds install media for image builds.
pub fn start(inst: &Instance, extra: &[String]) -> Result<()> {
    launch(inst, extra, None, false)
}

/// Boot the Windows installer. WinPE has no virtio-gpu driver, so it gets ramfb.
pub fn start_windows_installer(inst: &Instance, extra: &[String]) -> Result<()> {
    launch(inst, extra, None, true)
}

/// Resume a clone from a saved RAM snapshot instead of booting it.
pub fn start_resumed(inst: &Instance, state: &Path) -> Result<()> {
    launch(inst, &[], Some(state), false)?;
    let mut q = Qmp::connect(inst)?;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let st = q.execute("query-migrate", None)?;
        match st["status"].as_str().unwrap_or("") {
            "completed" => break,
            "failed" | "cancelled" => bail!(
                "restoring {} failed: {}",
                inst.name,
                st["error-desc"].as_str().unwrap_or("unknown error")
            ),
            _ if Instant::now() > deadline => bail!("restoring {} timed out", inst.name),
            _ => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    // The snapshot was taken paused, so the restored VM stays paused until told.
    q.execute("cont", None)?;
    Ok(())
}

/// Pause the VM, write its RAM and device state to `out`, then quit QEMU.
/// Migration flushes the disks, so the disk image matches the saved state.
pub fn save_state(inst: &Instance, out: &Path) -> Result<()> {
    let mut q = Qmp::connect(inst)?;
    pause_and_save(inst, &mut q, out)?;
    let _ = q.execute("quit", None);
    let deadline = Instant::now() + Duration::from_secs(30);
    while inst.running() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// Like `save_state`, but the VM runs on: `copy_disk` runs while it is still paused, so
/// the disk it copies matches the saved RAM.
pub fn checkpoint(
    inst: &Instance,
    out: &Path,
    copy_disk: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let mut q = Qmp::connect(inst)?;
    let saved = pause_and_save(inst, &mut q, out).and_then(|()| copy_disk());
    q.execute("cont", None)?;
    saved
}

fn pause_and_save(inst: &Instance, q: &mut Qmp, out: &Path) -> Result<()> {
    q.execute("stop", None)?;
    // QEMU caps migration at 128 MiB/s, meant for networks; a local file needs no cap.
    q.execute(
        "migrate-set-parameters",
        Some(json!({ "max-bandwidth": 1u64 << 40 })),
    )?;
    q.execute(
        "migrate",
        Some(json!({ "uri": format!("file:{}", out.display()) })),
    )?;
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        let st = q.execute("query-migrate", None)?;
        match st["status"].as_str().unwrap_or("") {
            "completed" => {
                // Record the machine type so this state resumes on the same machine.
                let _ = std::fs::write(machine_sidecar(out), machine_type());
                return Ok(());
            }
            "failed" | "cancelled" => bail!(
                "saving {} failed: {}",
                inst.name,
                st["error-desc"].as_str().unwrap_or("unknown error")
            ),
            _ if Instant::now() > deadline => bail!("saving {} timed out", inst.name),
            _ => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

fn launch(
    inst: &Instance,
    extra: &[String],
    incoming: Option<&Path>,
    installer: bool,
) -> Result<()> {
    let d = &inst.dir;
    // Refuse to relaunch a VM that's already up, rather than unlinking a live control socket.
    if Qmp::connect(inst).is_ok() {
        bail!("{} is already running", inst.name);
    }
    let _ = std::fs::remove_file(inst.qmp_socket());
    let machine = match incoming {
        Some(state) => resume_machine(state),
        None => machine_type(),
    };
    let mut fwd = format!("hostfwd=tcp:127.0.0.1:{}-:22", inst.ssh_port());
    let (mem, cpus) = inst.size();
    let mut args: Vec<String> = vec![
        "-smp".into(),
        cpus.to_string(),
        "-m".into(),
        format!("{mem}G"),
    ];
    match inst.os {
        Os::Windows => {
            fwd += &format!(",hostfwd=tcp:127.0.0.1:{}-:8000", inst.mcp_port());
            args.extend(
                [
                    "-device",
                    // ramfb's firmware driver tops out at 1024x768; Windows ships a
                    // virtio-gpu driver (viogpudo) from the setup disk.
                    if installer {
                        "ramfb"
                    } else {
                        "virtio-gpu-pci,xres=1280,yres=800"
                    },
                    "-rtc",
                    "base=localtime",
                    "-device",
                    "virtio-scsi-pci,id=scsi0",
                ]
                .map(String::from),
            );
            args.push("-drive".into());
            args.push(format!(
                "file={},id=data,format=qcow2,if=none,discard=unmap,cache=writeback",
                inst.disk().display()
            ));
            args.extend(
                ["-device", "scsi-hd,drive=data,bus=scsi0.0,bootindex=3"].map(String::from),
            );
        }
        Os::Ubuntu => {
            args.extend(["-device", "virtio-gpu-pci"].map(String::from));
            args.push("-drive".into());
            args.push(format!(
                "file={},if=virtio,format=qcow2,discard=unmap,cache=writeback",
                inst.disk().display()
            ));
        }
    }
    if let Some(state) = incoming {
        args.extend(["-incoming".into(), format!("file:{}", state.display())]);
    }
    let log_file = std::fs::File::create(d.join("qemu.log"))?;
    let mut cmd = Command::new(qemu_bin()?);
    cmd.args(["-machine", &format!("{machine},highmem=on")])
        .args(["-accel", "hvf", "-cpu", "host"])
        .args(&args)
        .arg("-drive")
        .arg(format!(
            "if=pflash,unit=0,format=raw,readonly=on,file={}",
            edk2()?.display()
        ))
        .arg("-drive")
        .arg(format!(
            "if=pflash,unit=1,format=raw,file={}",
            inst.vars().display()
        ))
        .args([
            "-fw_cfg",
            "name=opt/org.tianocore/UninstallMemAttrProtocol,string=y",
        ])
        // Enough root ports: EDK2 makes no boot entry for USB media behind a hub.
        .args([
            "-device",
            "qemu-xhci,id=xhci,p2=7,p3=7",
            "-device",
            "usb-kbd",
            "-device",
            "usb-tablet",
        ])
        .args([
            "-netdev",
            &format!(
                "user,id=net0,{fwd}{}",
                // restrict=on blocks the guest's own connections; host forwards still work.
                if inst.offline() { ",restrict=on" } else { "" }
            ),
            "-device",
            "virtio-net-pci,netdev=net0",
        ])
        .args([
            "-vnc",
            &format!(
                // password=on gates both the raw and websocket VNC on a per-VM password,
                // set over QMP once the monitor is up (below).
                "127.0.0.1:{},websocket={},password=on",
                inst.vnc_display(),
                inst.ws_port()
            ),
        ])
        .arg("-qmp")
        .arg(format!(
            "unix:{},server=on,wait=off",
            inst.qmp_socket().display()
        ))
        .arg("-serial")
        .arg(format!("file:{}", d.join("serial.log").display()))
        .arg("-pidfile")
        .arg(inst.pid_file())
        .args(extra)
        .stdin(Stdio::null())
        .stdout(log_file.try_clone()?)
        .stderr(log_file)
        // Own process group: Ctrl-C in the CLI must not take the VM down.
        .process_group(0);
    // Spawned detached rather than with -daemonize: QEMU's fork-based daemonizing
    // crashes during RAM snapshot save/restore under HVF.
    let mut child = cmd.spawn().context("spawn qemu")?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while !inst.qmp_socket().exists() {
        if let Some(st) = child.try_wait()? {
            let log = std::fs::read_to_string(d.join("qemu.log")).unwrap_or_default();
            bail!(
                "qemu exited while starting {} ({st}): {}",
                inst.name,
                log.trim()
            );
        }
        if Instant::now() > deadline {
            bail!("qemu did not open its control socket for {}", inst.name);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // Reap it when it exits so a long-lived parent (the MCP server) keeps no zombie,
    // which would still look alive to kill(pid, 0).
    std::thread::spawn(move || child.wait());
    set_vnc_password(inst);
    Ok(())
}

/// Apply this VM's stored VNC password over QMP (VNC was started with password=on, so it
/// rejects connections until this runs). Best-effort: a failure only affects the viewer,
/// and the password never reaches the log.
fn set_vnc_password(inst: &Instance) {
    let Ok(pass) = inst.ensure_vnc_password() else {
        log!(
            "{}: could not set a VNC password; viewer may be unavailable",
            inst.name
        );
        return;
    };
    if Qmp::connect(inst)
        .and_then(|mut q| {
            q.execute(
                "set_password",
                Some(json!({"protocol": "vnc", "password": pass})),
            )
        })
        .is_err()
    {
        log!(
            "{}: setting the VNC password failed; viewer may be unavailable",
            inst.name
        );
    }
}

/// Terminate QEMU immediately, without a guest shutdown.
pub fn quit(inst: &Instance) {
    let Some(pid) = inst.pid() else { return };
    let _ = Qmp::connect(inst).and_then(|mut q| q.execute("quit", None));
    let deadline = Instant::now() + Duration::from_secs(5);
    while inst.running() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    if inst.running() {
        kill(pid, 9);
    }
}

/// Clean ACPI shutdown, forcing QEMU to quit if the guest ignores it.
pub fn stop(inst: &Instance) -> Result<()> {
    let Some(pid) = inst.pid() else { return Ok(()) };
    let _ = Qmp::connect(inst).and_then(|mut q| q.execute("system_powerdown", None));
    let deadline = Instant::now() + Duration::from_secs(180);
    while Instant::now() < deadline {
        if !inst.running() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    log!("{}: no clean shutdown after 180s, forcing", inst.name);
    let _ = Qmp::connect(inst).and_then(|mut q| q.execute("quit", None));
    std::thread::sleep(Duration::from_secs(2));
    if inst.running() {
        kill(pid, 9);
    }
    Ok(())
}

/// QMP run state ("running", "paused", "postmigrate", "io-error", …), or `None` if the VM
/// isn't up. Exposed for callers that report state (e.g. `list_json`).
pub fn status(inst: &Instance) -> Option<String> {
    let mut q = Qmp::connect(inst).ok()?;
    let r = q.execute("query-status", None).ok()?;
    r["status"].as_str().map(String::from)
}

/// Resume a VM that's up but not running — paused, `postmigrate`, or `io-error` left by an
/// interrupted checkpoint. Best-effort.
pub fn resume_if_paused(inst: &Instance) {
    if let Some(st) = status(inst)
        && st != "running"
    {
        let _ = Qmp::connect(inst).and_then(|mut q| q.execute("cont", None));
    }
}

/// Screenshot straight to PNG (QEMU encodes it; no conversion step).
pub fn screenshot(inst: &Instance, out: &Path) -> Result<Vec<u8>> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    if !inst.running() {
        bail!("{} is not running", inst.name);
    }
    // Unique temp so parallel screenshots of one VM don't read each other's half-written PNG.
    let tmp = out.with_file_name(format!(
        ".screenshot-{}-{}.png",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let r = Qmp::connect(inst).and_then(|mut q| {
        q.execute(
            "screendump",
            Some(json!({"filename": tmp.to_string_lossy(), "format": "png"})),
        )
    });
    if let Err(e) = r {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    let data = std::fs::read(&tmp).context("read screenshot");
    let _ = std::fs::rename(&tmp, out);
    data
}

pub struct Qmp {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Qmp {
    pub fn connect(inst: &Instance) -> Result<Self> {
        let stream = UnixStream::connect(inst.qmp_socket())
            .with_context(|| format!("connect QMP for {}", inst.name))?;
        stream.set_read_timeout(Some(Duration::from_secs(60)))?;
        let mut q = Self {
            reader: BufReader::new(stream.try_clone()?),
            writer: stream,
        };
        q.read_msg()?; // greeting
        q.execute("qmp_capabilities", None)?;
        Ok(q)
    }

    fn read_msg(&mut self) -> Result<Value> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => bail!("QMP connection closed"),
            Ok(_) => {}
            // The read timeout (SO_RCVTIMEO) surfaces as EAGAIN / "os error 35".
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock || e.raw_os_error() == Some(35) =>
            {
                bail!("QEMU monitor timed out");
            }
            Err(e) => return Err(e.into()),
        }
        Ok(serde_json::from_str(&line)?)
    }

    pub fn execute(&mut self, cmd: &str, args: Option<Value>) -> Result<Value> {
        let mut msg = json!({ "execute": cmd });
        if let Some(a) = args {
            msg["arguments"] = a;
        }
        writeln!(self.writer, "{msg}")?;
        loop {
            let v = self.read_msg()?;
            if let Some(r) = v.get("return") {
                return Ok(r.clone());
            }
            if let Some(e) = v.get("error") {
                bail!("QMP {cmd}: {}", e["desc"].as_str().unwrap_or("error"));
            }
            // Asynchronous events are interleaved; skip them.
        }
    }

    pub fn send_key(&mut self, qcode: &str) -> Result<()> {
        self.execute(
            "send-key",
            Some(json!({"keys": [{"type": "qcode", "data": qcode}]})),
        )?;
        Ok(())
    }
}

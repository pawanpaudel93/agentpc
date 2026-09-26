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
    which("qemu-system-aarch64").context("qemu not found; install it with: brew install qemu")
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

/// Boot an instance in the background. `extra` adds install media for image builds.
pub fn start(inst: &Instance, extra: &[String]) -> Result<()> {
    launch(inst, extra, None)
}

/// Resume a clone from a saved RAM snapshot instead of booting it.
pub fn start_resumed(inst: &Instance, state: &Path) -> Result<()> {
    launch(inst, &[], Some(state))?;
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
    q.execute("stop", None)?;
    q.execute(
        "migrate",
        Some(json!({ "uri": format!("file:{}", out.display()) })),
    )?;
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        let st = q.execute("query-migrate", None)?;
        match st["status"].as_str().unwrap_or("") {
            "completed" => break,
            "failed" | "cancelled" => bail!(
                "saving {} failed: {}",
                inst.name,
                st["error-desc"].as_str().unwrap_or("unknown error")
            ),
            _ if Instant::now() > deadline => bail!("saving {} timed out", inst.name),
            _ => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    let _ = q.execute("quit", None);
    let deadline = Instant::now() + Duration::from_secs(30);
    while inst.running() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn launch(inst: &Instance, extra: &[String], incoming: Option<&Path>) -> Result<()> {
    let d = &inst.dir;
    let _ = std::fs::remove_file(inst.qmp_socket());
    let mut fwd = format!("hostfwd=tcp:127.0.0.1:{}-:22", inst.ssh_port());
    let mut args: Vec<String> = vec![];
    match inst.os {
        Os::Windows => {
            fwd += &format!(",hostfwd=tcp:127.0.0.1:{}-:8000", inst.mcp_port());
            args.extend(
                [
                    "-smp",
                    "4",
                    "-m",
                    "8G",
                    "-device",
                    "ramfb",
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
            args.extend(["-smp", "4", "-m", "4G", "-device", "virtio-gpu-pci"].map(String::from));
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
    cmd.args([
        "-machine",
        "virt,highmem=on",
        "-accel",
        "hvf",
        "-cpu",
        "host",
    ])
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
        &format!("user,id=net0,{fwd}"),
        "-device",
        "virtio-net-pci,netdev=net0",
    ])
    .args([
        "-vnc",
        &format!(
            "127.0.0.1:{},websocket={}",
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
    Ok(())
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

/// Screenshot straight to PNG (QEMU encodes it; no conversion step).
pub fn screenshot(inst: &Instance, out: &Path) -> Result<Vec<u8>> {
    if !inst.running() {
        bail!("{} is not running", inst.name);
    }
    Qmp::connect(inst)?.execute(
        "screendump",
        Some(json!({"filename": out.to_string_lossy(), "format": "png"})),
    )?;
    std::fs::read(out).context("read screenshot")
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
        if self.reader.read_line(&mut line)? == 0 {
            bail!("QMP connection closed");
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

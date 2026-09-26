//! Browser viewer: one shared static server for noVNC. QEMU serves VNC over
//! WebSocket itself (per-instance port), so no proxy is needed.

use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::instance::{Instance, cache_dir, home, kill, read_trimmed};

pub const PORT: u16 = 8100;
const NOVNC: &str = "v1.6.0";

pub fn url(inst: &Instance) -> String {
    format!(
        "http://127.0.0.1:{PORT}/vnc.html?autoconnect=1&resize=scale&host=127.0.0.1&port={}&path=",
        inst.ws_port()
    )
}

fn novnc_dir() -> PathBuf {
    cache_dir().join("novnc")
}

fn pid_file() -> PathBuf {
    home().join("viewer.pid")
}

fn ensure_novnc() -> Result<()> {
    if novnc_dir().join("vnc.html").is_file() {
        return Ok(());
    }
    std::fs::create_dir_all(cache_dir())?;
    let tmp = cache_dir().join("novnc.tmp");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    let st = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "curl -fsSL https://github.com/novnc/noVNC/archive/refs/tags/{NOVNC}.tar.gz | tar xz -C '{}' --strip-components 1",
            tmp.display()
        ))
        .status()?;
    if !st.success() {
        bail!("downloading noVNC failed");
    }
    std::fs::rename(&tmp, novnc_dir())?;
    Ok(())
}

fn listening() -> bool {
    TcpStream::connect_timeout(&([127, 0, 0, 1], PORT).into(), Duration::from_millis(300)).is_ok()
}

/// Start the detached viewer server if it isn't up.
pub fn ensure_running() -> Result<()> {
    if listening() {
        return Ok(());
    }
    ensure_novnc()?;
    use std::os::unix::process::CommandExt;
    let child = Command::new(std::env::current_exe()?)
        .arg("__viewer")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .context("spawn viewer")?;
    std::fs::write(pid_file(), child.id().to_string())?;
    Ok(())
}

/// Stop the viewer once no instance is running.
pub fn stop_if_idle() {
    if Instance::list().is_ok_and(|l| l.iter().all(|i| !i.running())) {
        if let Ok(pid) = read_trimmed(&pid_file()).and_then(|p| Ok(p.parse::<i32>()?)) {
            kill(pid, 15);
        }
        let _ = std::fs::remove_file(pid_file());
    }
}

/// Foreground server loop (`agentpc __viewer`).
pub fn serve() -> Result<()> {
    let root = novnc_dir();
    let server =
        tiny_http::Server::http(("127.0.0.1", PORT)).map_err(|e| anyhow::anyhow!("{e}"))?;
    for req in server.incoming_requests() {
        let path = req
            .url()
            .split('?')
            .next()
            .unwrap_or("/")
            .trim_start_matches('/')
            .to_string();
        let path = if path.is_empty() {
            "vnc.html".to_string()
        } else {
            path
        };
        let file = root.join(&path);
        let resp = if !path.contains("..") && file.is_file() {
            let mime = match file.extension().and_then(|e| e.to_str()) {
                Some("html") => "text/html",
                Some("js") => "text/javascript",
                Some("css") => "text/css",
                Some("svg") => "image/svg+xml",
                Some("png") => "image/png",
                Some("json") => "application/json",
                _ => "application/octet-stream",
            };
            let data = std::fs::read(&file).unwrap_or_default();
            tiny_http::Response::from_data(data)
                .with_header(tiny_http::Header::from_bytes("Content-Type", mime).unwrap())
                .boxed()
        } else {
            tiny_http::Response::from_string("not found")
                .with_status_code(404)
                .boxed()
        };
        let _ = req.respond(resp);
    }
    Ok(())
}

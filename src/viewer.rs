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
/// Pinned so a tampered or swapped tarball can't slip in. Recompute if `NOVNC` changes.
const NOVNC_SHA256: &str = "5066103959ef4e9b10f37e5a148627360dd8414e4cf8a7db92bdbd022e728aaa";

pub fn url(inst: &Instance) -> String {
    // A page served on this port reads the password, so only ever hand it to our own viewer.
    if !ours() {
        return format!(
            "unavailable: 127.0.0.1:{PORT} is in use by another program (VNC on 127.0.0.1:{})",
            inst.vnc_port()
        );
    }
    // The per-VM VNC password (if set) is handed to noVNC so the URL still auto-connects. It
    // goes in the fragment, which noVNC reads and a browser never sends in a request.
    let pass = inst
        .vnc_password()
        .map(|p| format!("&password={p}"))
        .unwrap_or_default();
    format!(
        "http://127.0.0.1:{PORT}/vnc.html#autoconnect=1&resize=scale&host=127.0.0.1&port={}&path={pass}",
        inst.ws_port()
    )
}

/// What our viewer answers at `/.agentpc-viewer`: a random token in a private file, which
/// another program on this port can't know.
fn token_file() -> PathBuf {
    home().join("viewer.token")
}

/// Whether the server on PORT is our viewer (it answers with the token).
fn ours() -> bool {
    use std::io::{Read, Write};
    let Ok(token) = read_trimmed(&token_file()) else {
        return false;
    };
    let Ok(mut s) =
        TcpStream::connect_timeout(&([127, 0, 0, 1], PORT).into(), Duration::from_millis(300))
    else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
    if s.write_all(b"GET /.agentpc-viewer HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut body = String::new();
    let _ = s.take(4096).read_to_string(&mut body);
    body.split("\r\n\r\n")
        .nth(1)
        .is_some_and(|b| b.trim() == token)
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
    let tarball = cache_dir().join("novnc.tar.gz");
    let st = Command::new("curl")
        .args([
            "-fsSL",
            "-o",
            &tarball.to_string_lossy(),
            &format!("https://github.com/novnc/noVNC/archive/refs/tags/{NOVNC}.tar.gz"),
        ])
        .status()?;
    if !st.success() {
        let _ = std::fs::remove_file(&tarball);
        bail!("downloading noVNC failed");
    }
    let got = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(std::fs::read(&tarball)?);
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    if got != NOVNC_SHA256 {
        let _ = std::fs::remove_file(&tarball);
        bail!("noVNC {NOVNC} checksum mismatch (got {got})");
    }
    let tmp = cache_dir().join("novnc.tmp");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    let st = Command::new("tar")
        .args(["xzf", &tarball.to_string_lossy(), "-C"])
        .arg(&tmp)
        .args(["--strip-components", "1"])
        .status()?;
    let _ = std::fs::remove_file(&tarball);
    if !st.success() {
        let _ = std::fs::remove_dir_all(&tmp);
        bail!("unpacking noVNC failed");
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
        if ours() {
            return Ok(());
        }
        // An older agentpc's viewer (it has no token) gives way to this one; anything else
        // keeps the port, and no VM's password goes near it.
        match read_trimmed(&pid_file()).and_then(|p| Ok(p.parse::<i32>()?)) {
            Ok(pid) if pid_is_our_viewer(pid) => {
                kill(pid, 15);
                for _ in 0..20 {
                    if !listening() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            _ => bail!("127.0.0.1:{PORT} is in use by another program; the browser viewer is off"),
        }
    }
    ensure_novnc()?;
    // A fresh token for this viewer, private to this user.
    let mut buf = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
        .context("read /dev/urandom")?;
    let token: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    crate::instance::write_private(&token_file(), &token)?;
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
        if let Ok(pid) = read_trimmed(&pid_file()).and_then(|p| Ok(p.parse::<i32>()?))
            && pid_is_our_viewer(pid)
        {
            kill(pid, 15);
        }
        let _ = std::fs::remove_file(pid_file());
    }
}

/// Confirm `pid` is our viewer subprocess (`agentpc __viewer`) before signalling it, so a
/// recycled pid in a stale viewer.pid isn't killed by mistake.
fn pid_is_our_viewer(pid: i32) -> bool {
    let Ok(out) = Command::new("ps")
        .args(["-ww", "-o", "command=", "-p", &pid.to_string()])
        .output()
    else {
        return false;
    };
    String::from_utf8_lossy(&out.stdout).contains("__viewer")
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
        let resp = if path == ".agentpc-viewer" {
            let token = read_trimmed(&token_file()).unwrap_or_default();
            tiny_http::Response::from_string(token).boxed()
        } else if !path.contains("..") && file.is_file() {
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

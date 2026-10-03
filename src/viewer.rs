//! Browser viewer: one shared static server for noVNC. QEMU serves VNC over
//! WebSocket itself (per-instance port), so no proxy is needed.

use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

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

/// Opening a browser is a per-call convenience, not a VM setting or a creation failure.
pub fn open_created(result: Result<String>, requested: bool) -> Result<String> {
    after_create(result, requested, |name| {
        let inst = Instance::load(name)?;
        let at = url(&inst);
        if !at.starts_with("http://127.0.0.1:") {
            bail!("the local viewer is unavailable");
        }
        // noVNC's URL can contain a VNC credential; never echo it or opener diagnostics.
        let status = Command::new("open")
            .arg(at)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("launch default browser")?;
        if !status.success() {
            bail!("default browser launcher failed");
        }
        Ok(())
    })
}

fn after_create(
    result: Result<String>,
    requested: bool,
    open: impl FnOnce(&str) -> Result<()>,
) -> Result<String> {
    let mut info = result?;
    if requested {
        let name = info
            .split_whitespace()
            .next()
            .context("created VM has no name")?;
        if open(name).is_err() {
            info.push_str("\n  warning: VM is ready, but its viewer could not be opened in the Mac's default browser");
        }
    }
    Ok(info)
}

/// Private HMAC key shared by the viewer and the commands that verify its identity.
/// It is never returned by the HTTP server.
fn token_file() -> PathBuf {
    home().join("viewer.token")
}

fn random_bytes() -> Result<[u8; 32]> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes))
        .context("read /dev/urandom")?;
    Ok(bytes)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(bytes)
}

fn identity_mac(key: &[u8], nonce: &[u8; 32]) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(b"agentpc-viewer-identity-v1\0");
    mac.update(nonce);
    mac
}

fn identity_response(url: &str, key: &[u8]) -> Option<String> {
    let nonce = unhex(url.strip_prefix("/.agentpc-viewer?nonce=")?)?;
    Some(hex(&identity_mac(key, &nonce).finalize().into_bytes()))
}

fn verify_identity(key: &[u8], nonce: &[u8; 32], response: &str) -> bool {
    unhex(response.trim()).is_some_and(|tag| identity_mac(key, nonce).verify_slice(&tag).is_ok())
}

/// Prove identity with a fresh nonce, so an observed response cannot authenticate a
/// different listener after the genuine viewer exits.
fn ours() -> bool {
    use std::io::{Read, Write};
    let Ok(key) = read_trimmed(&token_file()) else {
        return false;
    };
    if key.is_empty() {
        return false;
    }
    let Ok(nonce) = random_bytes() else {
        return false;
    };
    let Ok(mut s) =
        TcpStream::connect_timeout(&([127, 0, 0, 1], PORT).into(), Duration::from_millis(300))
    else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = s.set_write_timeout(Some(Duration::from_millis(500)));
    let request = format!(
        "GET /.agentpc-viewer?nonce={} HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        hex(&nonce)
    );
    if s.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut response = String::new();
    if s.take(4096).read_to_string(&mut response).is_err() {
        return false;
    }
    response
        .split_once("\r\n\r\n")
        .is_some_and(|(_, body)| verify_identity(key.as_bytes(), &nonce, body))
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
        // A viewer from an older protocol gives way to this one; anything else keeps the
        // port, and no VM's password goes near it.
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
    // A fresh private key for this viewer. Only nonce-bound HMACs cross the socket.
    let token = hex(&random_bytes()?);
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
    let key = read_trimmed(&token_file())?;
    if key.is_empty() {
        bail!("viewer identity key is empty");
    }
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
            match identity_response(req.url(), key.as_bytes()) {
                Some(proof) => tiny_http::Response::from_string(proof)
                    .with_header(
                        tiny_http::Header::from_bytes("Cache-Control", "no-store").unwrap(),
                    )
                    .boxed(),
                None => tiny_http::Response::from_string("invalid identity challenge")
                    .with_status_code(400)
                    .boxed(),
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewer_identity_proofs_are_nonce_bound_and_do_not_expose_the_key() {
        let key = [7u8; 32];
        let nonce = [1u8; 32];
        let other = [2u8; 32];
        let url = format!("/.agentpc-viewer?nonce={}", hex(&nonce));
        let proof = identity_response(&url, &key).unwrap();
        assert_eq!(proof.len(), 64);
        assert!(proof != hex(&key));
        assert!(verify_identity(&key, &nonce, &proof));
        assert!(!verify_identity(&key, &other, &proof));
        assert!(!verify_identity(&[8u8; 32], &nonce, &proof));
        assert!(!verify_identity(&key, &nonce, "invalid"));
        assert!(!verify_identity(&key, &nonce, &hex(&key)));
        for malformed in [
            "/.agentpc-viewer".to_string(),
            "/.agentpc-viewer?nonce=".to_string(),
            "/.agentpc-viewer?nonce=zz".to_string(),
            format!("{url}&extra=1"),
            format!("/different?nonce={}", hex(&nonce)),
        ] {
            assert!(identity_response(&malformed, &key).is_none());
        }
    }

    #[test]
    fn viewer_identity_challenges_are_fresh() {
        let first = random_bytes().unwrap();
        let second = random_bytes().unwrap();
        assert!(first != second);
        assert_eq!(unhex(&hex(&first)), Some(first));
    }

    #[test]
    fn browser_opens_only_after_success_and_when_requested() {
        let info = "ubuntu-7 (ubuntu): ready";
        assert_eq!(
            after_create(Ok(info.into()), false, |_| panic!("must not open")).unwrap(),
            info
        );
        assert!(
            after_create(Err(anyhow::anyhow!("boot failed")), true, |_| panic!(
                "must not open"
            ))
            .is_err()
        );
        let mut opened = false;
        assert_eq!(
            after_create(Ok(info.into()), true, |name| {
                assert_eq!(name, "ubuntu-7");
                opened = true;
                Ok(())
            })
            .unwrap(),
            info
        );
        assert!(opened);
    }

    #[test]
    fn browser_failure_warns_without_failing_the_vm_or_exposing_diagnostics() {
        let info = "ubuntu-7 (ubuntu): ready";
        let out = after_create(Ok(info.into()), true, |_| {
            bail!("private launcher diagnostics")
        })
        .unwrap();
        assert!(out.starts_with(info));
        assert!(out.contains("warning: VM is ready"));
        assert!(!out.contains("private launcher diagnostics"));
    }
}

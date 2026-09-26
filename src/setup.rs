//! Agent registration and prerequisite checks.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::image::{find_windows_iso, windows_setup_img_path};
use crate::instance::{Os, home};
use crate::log;
use crate::qemu::{edk2, which};

const SERVER: &str = "agentpc";
const DEFAULT_CLIENTS: [&str; 6] = [
    "claude",
    "claude-desktop",
    "codex",
    "cursor",
    "gemini",
    "vscode",
];

fn user_home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

/// Run a registration command, echoing it so the user sees what changed.
fn register_cmd(cmd: &str, args: &[&str], quiet_fail: bool) -> Result<()> {
    if !quiet_fail {
        log!("run: {cmd} {}", args.join(" "));
    }
    let out = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("run {cmd}"))?;
    if !out.status.success() && !quiet_fail {
        bail!(
            "{cmd} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Merge `{"mcpServers": {"agentpc": ...}}` into a JSON config, keeping every other key.
fn json_register(path: &Path, bin: &str) -> Result<()> {
    let mut cfg: Value = match std::fs::read_to_string(path) {
        Ok(s) if !s.trim().is_empty() => serde_json::from_str(&s)
            .with_context(|| format!("{} is not valid JSON; not touching it", path.display()))?,
        _ => json!({}),
    };
    let root = cfg
        .as_object_mut()
        .with_context(|| format!("{} is not a JSON object", path.display()))?;
    let servers = root.entry("mcpServers").or_insert_with(|| json!({}));
    let servers = servers
        .as_object_mut()
        .with_context(|| format!("{}: mcpServers is not an object", path.display()))?;
    servers.insert(SERVER.into(), json!({"command": bin, "args": ["mcp"]}));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&cfg)? + "\n")?;
    Ok(())
}

pub fn mcp_install(clients: &[String]) -> Result<()> {
    let clients: Vec<&str> = if clients.is_empty() {
        DEFAULT_CLIENTS.to_vec()
    } else {
        clients.iter().map(String::as_str).collect()
    };
    if let Some(c) = clients.iter().find(|c| !DEFAULT_CLIENTS.contains(c)) {
        bail!("unknown client '{c}' ({})", DEFAULT_CLIENTS.join("|"));
    }
    let exe = std::env::current_exe().context("locate agentpc binary")?;
    let bin = std::fs::canonicalize(&exe)
        .unwrap_or(exe)
        .to_string_lossy()
        .into_owned();
    let home = user_home();

    for c in clients {
        match c {
            "claude" => {
                if which("claude").is_none() {
                    log!("skip claude (not installed)");
                    continue;
                }
                register_cmd("claude", &["mcp", "remove", "-s", "user", SERVER], true)?;
                register_cmd(
                    "claude",
                    &["mcp", "add", "--scope", "user", SERVER, "--", &bin, "mcp"],
                    false,
                )?;
            }
            "claude-desktop" => {
                let dir = home.join("Library/Application Support/Claude");
                if !dir.is_dir() {
                    log!("skip claude-desktop (not installed)");
                    continue;
                }
                json_register(&dir.join("claude_desktop_config.json"), &bin)?;
                log!("restart Claude Desktop to load it");
            }
            "codex" => {
                if which("codex").is_none() {
                    log!("skip codex (not installed)");
                    continue;
                }
                register_cmd("codex", &["mcp", "remove", SERVER], true)?;
                register_cmd("codex", &["mcp", "add", SERVER, "--", &bin, "mcp"], false)?;
            }
            "cursor" => {
                if !home.join(".cursor").is_dir() {
                    log!("skip cursor (no ~/.cursor)");
                    continue;
                }
                json_register(&home.join(".cursor/mcp.json"), &bin)?;
            }
            "gemini" => {
                if !home.join(".gemini").is_dir() && which("gemini").is_none() {
                    log!("skip gemini (not installed)");
                    continue;
                }
                json_register(&home.join(".gemini/settings.json"), &bin)?;
            }
            "vscode" => {
                if which("code").is_none() {
                    log!("skip vscode (no `code` command)");
                    continue;
                }
                let spec = json!({"name": SERVER, "command": bin, "args": ["mcp"]}).to_string();
                register_cmd("code", &["--add-mcp", &spec], false)?;
            }
            _ => unreachable!(),
        }
        log!("registered {SERVER} with {c}");
    }
    println!("Other MCP clients: add a stdio server named \"{SERVER}\":");
    println!(
        "  {}",
        json!({"mcpServers": {SERVER: {"command": bin, "args": ["mcp"]}}})
    );
    Ok(())
}

/// Free GB on the volume holding `p` (or its nearest existing ancestor).
fn free_gb(p: &Path) -> Option<u64> {
    let dir = p.ancestors().find(|a| a.exists())?;
    let out = Command::new("df").arg("-g").arg(dir).output().ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .nth(1)?
        .split_whitespace()
        .nth(3)?
        .parse()
        .ok()
}

pub fn doctor() -> Result<bool> {
    let mut all_ok = true;
    let mut check = |name: &str, ok: bool, hint: &str| {
        if ok {
            println!("  ok   {name}");
        } else {
            all_ok = false;
            println!(
                "  FAIL {name}{}",
                if hint.is_empty() {
                    String::new()
                } else {
                    format!(" — {hint}")
                }
            );
        }
    };

    println!("host:");
    check("Apple Silicon", std::env::consts::ARCH == "aarch64", "");
    check("macOS", std::env::consts::OS == "macos", "");
    check("qemu (brew install qemu)", edk2().is_ok(), "");
    let h = home();
    check(
        &format!("free disk >= 40 GB ({})", h.display()),
        free_gb(&h).is_some_and(|g| g >= 40),
        "",
    );

    println!("windows image build only:");
    let setup_img = windows_setup_img_path().is_file();
    // Not required: image build downloads Microsoft's ISO when none is found.
    match find_windows_iso(None).filter(|p| p.is_file()) {
        Some(p) => println!("  ok   Windows ISO: {}", p.display()),
        None => println!(
            "  ok   Windows ISO: none yet; image build downloads it from Microsoft (7.3 GB)"
        ),
    }
    check(
        "colima + docker (brew install colima docker)",
        setup_img || (which("colima").is_some() && which("docker").is_some()),
        "",
    );
    let cpu = Command::new("sysctl")
        .args(["-n", "machdep.cpu.brand_string"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    check(
        "M3 or newer (nested virt for dockur)",
        setup_img || !(cpu.contains("M1") || cpu.contains("M2")),
        "",
    );

    println!("images:");
    for os in Os::ALL {
        check(
            &os.to_string(),
            os.image_disk().is_file(),
            &format!("run: agentpc image build {os} (or image pull ubuntu)"),
        );
    }
    Ok(all_ok)
}

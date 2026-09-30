//! Agent registration, prerequisite checks, cleanup and uninstall.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::image::find_windows_iso;
use crate::instance::{Image, Os, home};
use crate::log;
use crate::qemu::{edk2, which};

const SERVER: &str = "agentpc";
/// How the docs tell people to get QEMU; reused in hints so they match install.sh.
pub const QEMU_HINT: &str = "install: curl -fsSL https://agentpc.pawanpaudel.com.np/install.sh | sh   (or: brew install qemu)";
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

/// How to invoke agentpc in hint text: `agentpc` when that resolves on PATH to this same
/// executable, otherwise the full path to the running binary. MCPB-bundle and one-off
/// downloads aren't on PATH, so a bare `agentpc` in a hint would fail for them.
pub fn cmd_name() -> String {
    let exe = std::env::current_exe().ok();
    let same = |a: &Path, b: &Path| std::fs::canonicalize(a).ok() == std::fs::canonicalize(b).ok();
    match (exe, which("agentpc")) {
        (Some(exe), Some(on_path)) if same(&exe, &on_path) => "agentpc".into(),
        (Some(exe), _) => exe.to_string_lossy().into_owned(),
        (None, _) => "agentpc".into(),
    }
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

/// True when the agentpc Claude Code plugin is installed. The plugin already ships an MCP
/// server entry, so `mcp-install` must not also register a stdio server for Claude Code —
/// that lists agentpc twice.
fn claude_plugin_installed() -> bool {
    let path = user_home().join(".claude/plugins/installed_plugins.json");
    let Ok(s) = std::fs::read_to_string(&path) else {
        return false;
    };
    let Ok(cfg) = serde_json::from_str::<Value>(&s) else {
        return false;
    };
    // Keys look like "<name>@<marketplace>"; match on the plugin name.
    cfg.get("plugins")
        .and_then(Value::as_object)
        .is_some_and(|m| m.keys().any(|k| k.split('@').next() == Some(SERVER)))
}

/// codex has no CLI flag for MCP timeouts, so add them to the `[mcp_servers.agentpc]` block
/// `codex mcp add` just wrote. Image builds and first-boot resumes take minutes; codex's
/// short default would abort the tool call. Existing values are left untouched.
fn codex_set_timeouts() -> Result<()> {
    let path = user_home().join(".codex/config.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(()); // no config: codex uses its defaults, nothing to amend
    };
    let header = format!("[mcp_servers.{SERVER}]");
    let lines: Vec<&str> = text.lines().collect();
    let Some(start) = lines.iter().position(|l| l.trim() == header) else {
        return Ok(());
    };
    // The section runs to the next table header or end of file.
    let end = lines[start + 1..]
        .iter()
        .position(|l| l.trim_start().starts_with('['))
        .map_or(lines.len(), |i| start + 1 + i);
    let has = |key: &str| {
        lines[start + 1..end]
            .iter()
            .any(|l| l.trim_start().starts_with(key))
    };
    let mut add: Vec<&str> = Vec::new();
    if !has("tool_timeout_sec") {
        add.push("tool_timeout_sec = 900");
    }
    if !has("startup_timeout_sec") {
        add.push("startup_timeout_sec = 60");
    }
    if add.is_empty() {
        return Ok(());
    }
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        out.push_str(line);
        out.push('\n');
        if i == start {
            for a in &add {
                out.push_str(a);
                out.push('\n');
            }
        }
    }
    std::fs::write(&path, out)?;
    log!("set codex MCP timeouts in {}", path.display());
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
                if claude_plugin_installed() {
                    log!("skip claude ({SERVER} plugin already registers the MCP server)");
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
                codex_set_timeouts()?;
                // Approving every tool is the user's call (download_file writes to this Mac),
                // so say how rather than set it.
                log!(
                    "codex asks before each desktop action; to approve {SERVER}'s tools up front, \
                     add default_tools_approval_mode = \"approve\" under [mcp_servers.{SERVER}] \
                     in ~/.codex/config.toml"
                );
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
    let qemu_ok = edk2().is_ok();
    check("qemu", qemu_ok, QEMU_HINT);
    // An x86_64 QEMU (e.g. from a Rosetta brew) can't use HVF here; warn but don't fail.
    if qemu_ok && !qemu_is_arm64() {
        println!(
            "  warn qemu is not an arm64 build — VMs will be slow or fail; reinstall with: brew install qemu"
        );
    }
    let h = home();
    check(
        &format!("free disk >= 40 GB ({})", h.display()),
        free_gb(&h).is_some_and(|g| g >= 40),
        "",
    );

    println!("windows image build:");
    // Not required: image build downloads Microsoft's ISO when none is found.
    match find_windows_iso(Os::Windows.default_version(), None).filter(|p| p.is_file()) {
        Some(p) => println!("  ok   Windows ISO: {}", p.display()),
        None => println!(
            "  ok   Windows ISO: none yet; image build downloads it from Microsoft (7.3 GB)"
        ),
    }

    // No image yet is the normal fresh-install state, not a failure: report it as info
    // with the command that builds or pulls one, so `doctor` doesn't scare a new user.
    println!("images:");
    let images = Image::all();
    let cmd = cmd_name();
    for os in Os::ALL {
        if images.iter().any(|i| i.os == os) {
            println!("  ok   {os}");
        } else {
            let hint = match os {
                Os::Windows => format!("{cmd} image build windows"),
                Os::Ubuntu => format!("{cmd} image pull ubuntu (or {cmd} image build ubuntu)"),
                Os::Arch => format!("{cmd} image pull arch (or {cmd} image build arch)"),
            };
            println!("  info {os}: none yet — build or pull one: {hint}");
        }
    }
    Ok(all_ok)
}

/// True when the QEMU binary is a native arm64 Mach-O (so it can use HVF acceleration).
fn qemu_is_arm64() -> bool {
    let Ok(bin) = crate::qemu::qemu_bin() else {
        return false;
    };
    // `file` names each Mach-O slice; a usable build has an arm64/arm64e one.
    Command::new("file")
        .arg("-b")
        .arg(&bin)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("arm64"))
        .unwrap_or(true) // if `file` is unavailable, don't cry wolf
}

/// Drop `agentpc` from the `key` object of a JSON config; leaves the file alone otherwise.
fn json_unregister(path: &Path, key: &str) -> Result<bool> {
    let Ok(s) = std::fs::read_to_string(path) else {
        return Ok(false);
    };
    let Ok(mut cfg) = serde_json::from_str::<Value>(&s) else {
        return Ok(false);
    };
    let removed = cfg
        .get_mut(key)
        .and_then(Value::as_object_mut)
        .is_some_and(|servers| servers.remove(SERVER).is_some());
    if removed {
        std::fs::write(path, serde_json::to_string_pretty(&cfg)? + "\n")?;
    }
    Ok(removed)
}

/// Undo `mcp_install` for the given clients (all when empty).
pub fn mcp_uninstall(clients: &[String]) -> Result<()> {
    let clients: Vec<&str> = if clients.is_empty() {
        DEFAULT_CLIENTS.to_vec()
    } else {
        clients.iter().map(String::as_str).collect()
    };
    if let Some(c) = clients.iter().find(|c| !DEFAULT_CLIENTS.contains(c)) {
        bail!("unknown client '{c}' ({})", DEFAULT_CLIENTS.join("|"));
    }
    let home = user_home();
    for c in clients {
        let removed = match c {
            "claude" => {
                which("claude").is_some()
                    && Command::new("claude")
                        .args(["mcp", "remove", "-s", "user", SERVER])
                        .stdin(Stdio::null())
                        .output()
                        .is_ok_and(|o| o.status.success())
            }
            // `codex mcp remove` exits 0 even when there was nothing to remove.
            "codex" => {
                which("codex").is_some()
                    && Command::new("codex")
                        .args(["mcp", "remove", SERVER])
                        .stdin(Stdio::null())
                        .output()
                        .is_ok_and(|o| {
                            o.status.success()
                                && !String::from_utf8_lossy(&o.stdout).contains("No MCP server")
                        })
            }
            "claude-desktop" => json_unregister(
                &home.join("Library/Application Support/Claude/claude_desktop_config.json"),
                "mcpServers",
            )?,
            "cursor" => json_unregister(&home.join(".cursor/mcp.json"), "mcpServers")?,
            "gemini" => json_unregister(&home.join(".gemini/settings.json"), "mcpServers")?,
            "vscode" => json_unregister(
                &home.join("Library/Application Support/Code/User/mcp.json"),
                "servers",
            )?,
            _ => unreachable!(),
        };
        if removed {
            log!("unregistered {SERVER} from {c}");
        }
    }
    Ok(())
}

/// Disk use of `p` in bytes, as `du` counts it (sparse files and clones included correctly).
fn disk_use(p: &Path) -> u64 {
    Command::new("du")
        .arg("-sk")
        .arg(p)
        .output()
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()
        })
        .map_or(0, |k| k * 1024)
}

/// A checkpoint dir name that is interrupted-write scratch rather than a real checkpoint.
/// Matches the current `<label>.partial` temp and the newer `.partial-<label>` form; a real
/// checkpoint that merely ends in `.partial` (so it's in `valid`) is left alone.
fn partial_checkpoint(name: &str, valid: &[String]) -> bool {
    name.starts_with(".partial-")
        || (name.ends_with(".partial") && !valid.iter().any(|v| v == name))
}

/// Delete what can be downloaded or rebuilt again, and leftovers of interrupted work.
/// Images and VMs are never touched; unused images are listed.
pub fn clean(dry_run: bool) -> Result<String> {
    use crate::instance::{Instance, cache_dir, images_dir, instances_dir};
    let mut targets: Vec<(PathBuf, &str)> = Vec::new();
    for e in std::fs::read_dir(cache_dir())
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = e.file_name().to_string_lossy().into_owned();
        // The viewer's noVNC copy is tiny and needed offline; dotfiles are download locks.
        if name == "novnc" || name.starts_with('.') {
            continue;
        }
        let why = if name.ends_with(".iso") || name.ends_with(".img") {
            "download; fetched again when a build needs it"
        } else if name.starts_with("virtio-win") {
            "Windows drivers; fetched again when a build needs them"
        } else {
            "leftover download or build file"
        };
        targets.push((e.path(), why));
    }
    // Hidden `_`-prefixed VMs are build/snapshot scratch. A held lock on their image means
    // that build is live right now, so leave them alone; otherwise they are crash leftovers.
    for e in std::fs::read_dir(instances_dir())
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = e.file_name().to_string_lossy().into_owned();
        let inst = Instance::load(&name).ok();
        if name.starts_with('_') {
            let live = inst
                .as_ref()
                .is_some_and(|i| crate::instance::try_image_lock(&i.image).is_none());
            if live {
                continue;
            }
            if let Some(i) = inst.as_ref().filter(|i| i.running()) {
                // A crashed build can leave its VM up; stop it before removing its files.
                if !dry_run {
                    crate::qemu::quit(i);
                }
                targets.push((
                    e.path(),
                    "leftover from an interrupted image build (was still running)",
                ));
            } else {
                targets.push((e.path(), "leftover from an interrupted image build"));
            }
            continue;
        }
        // A real VM: sweep only unfinished checkpoint temp dirs, never a real checkpoint.
        let valid = inst
            .as_ref()
            .map(crate::ops::checkpoints)
            .unwrap_or_default();
        for cp in std::fs::read_dir(e.path().join("checkpoints"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let cp_name = cp.file_name().to_string_lossy().into_owned();
            if partial_checkpoint(&cp_name, &valid) {
                targets.push((cp.path(), "unfinished checkpoint"));
            }
        }
    }

    // Half-written image files from a crashed build or snapshot (`.qcow2.tmp`, `.state.tmp`).
    for e in std::fs::read_dir(images_dir())
        .into_iter()
        .flatten()
        .flatten()
    {
        if e.file_name().to_string_lossy().ends_with(".tmp") {
            targets.push((
                e.path(),
                "half-written image file from an interrupted build",
            ));
        }
    }

    let gb = |b: u64| b as f64 / 1e9;
    let mut out = String::new();
    let mut total = 0;
    for (p, why) in &targets {
        let size = disk_use(p);
        total += size;
        out += &format!("{:>7.2} GB  {}  ({why})\n", gb(size), p.display());
        if !dry_run {
            // Unpacked archives can hold read-only folders.
            let _ = Command::new("chmod").arg("-R").arg("u+w").arg(p).status();
            if p.is_dir() {
                std::fs::remove_dir_all(p)?;
            } else {
                std::fs::remove_file(p)?;
            }
        }
    }
    out += &if targets.is_empty() {
        "nothing to clean\n".to_string()
    } else if dry_run {
        format!(
            "{:.2} GB would be freed (run without --dry-run to delete)\n",
            gb(total)
        )
    } else {
        format!("freed {:.2} GB\n", gb(total))
    };
    let used: Vec<Image> = Instance::list()?.into_iter().map(|i| i.image).collect();
    let cmd = cmd_name();
    for image in Image::all().into_iter().filter(|i| !used.contains(i)) {
        let size = ["", ".snapshot"]
            .iter()
            .map(|s| disk_use(&image.disk().with_file_name(format!("{image}{s}.qcow2"))))
            .sum::<u64>()
            + disk_use(&image.snapshot_state());
        out += &format!(
            "kept: {image} ({:.1} GB) has no VMs; remove it with: {cmd} image rm {image}\n",
            gb(size)
        );
    }
    Ok(out)
}

/// Remove agentpc from this Mac: VMs stopped, agent registrations undone, data (unless
/// `keep_data`) and the binary deleted. Shared tools (QEMU, Homebrew, the PATH line) stay.
pub fn uninstall(keep_data: bool, yes: bool) -> Result<()> {
    use std::io::{IsTerminal, Write};
    let data = home();
    let exe = std::env::current_exe().context("locate agentpc binary")?;
    println!("This removes:");
    println!(
        "  - the agentpc MCP server from Claude Code, Claude Desktop, Codex, Cursor, Gemini, VS Code"
    );
    if keep_data {
        println!("  (keeping {}: images, VMs, keys)", data.display());
    } else {
        println!(
            "  - {} ({:.1} GB: every image, VM and checkpoint)",
            data.display(),
            disk_use(&data) as f64 / 1e9
        );
    }
    println!("  - {}", exe.display());
    if !yes {
        if !std::io::stdin().is_terminal() {
            bail!("pass --yes to uninstall without a prompt");
        }
        print!("Continue? [y/N] ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            bail!("cancelled");
        }
    }

    for inst in crate::instance::Instance::list()? {
        if inst.running() {
            log!("stopping {}", inst.name);
            if keep_data {
                crate::qemu::stop(&inst)?;
            } else {
                crate::qemu::quit(&inst);
            }
        }
    }
    crate::viewer::stop_if_idle();
    mcp_uninstall(&[])?;
    if !keep_data && data.exists() {
        let _ = Command::new("chmod")
            .arg("-R")
            .arg("u+w")
            .arg(&data)
            .status();
        std::fs::remove_dir_all(&data).with_context(|| format!("remove {}", data.display()))?;
        log!("removed {}", data.display());
    }
    std::fs::remove_file(&exe).with_context(|| format!("remove {}", exe.display()))?;
    log!("removed {}", exe.display());

    println!("\nagentpc is uninstalled. Left in place, since other tools may use them:");
    let dir = exe
        .parent()
        .map(|d| d.display().to_string())
        .unwrap_or_default();
    let home = user_home();
    for rc in [
        ".zshrc",
        ".bashrc",
        ".bash_profile",
        ".profile",
        ".config/fish/config.fish",
    ] {
        let p = home.join(rc);
        if std::fs::read_to_string(&p).is_ok_and(|s| s.contains(&dir)) {
            println!("  - the PATH line for {dir} in {}", p.display());
        }
    }
    if which("qemu-system-aarch64").is_some() {
        println!("  - QEMU (brew uninstall qemu, if nothing else needs it)");
    }
    println!("  - the Claude plugin, if you added it: /plugin uninstall agentpc@agentpc");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_checkpoint_selection() {
        let valid = vec!["clean".to_string(), "before-update".to_string()];
        // Both temp-dir spellings are scratch.
        assert!(partial_checkpoint("clean.partial", &valid));
        assert!(partial_checkpoint(".partial-clean", &valid));
        // A real checkpoint is never scratch, even if it ends in `.partial`.
        assert!(!partial_checkpoint("clean", &valid));
        let valid2 = vec!["weird.partial".to_string()];
        assert!(!partial_checkpoint("weird.partial", &valid2));
        // An orphaned `<label>.partial` whose checkpoint no longer exists is scratch.
        assert!(partial_checkpoint("gone.partial", &valid));
    }

    #[test]
    fn cmd_name_is_usable() {
        // Either the bare command (on PATH) or an absolute path to a real binary; never empty.
        let c = cmd_name();
        assert!(!c.is_empty());
        assert!(c == "agentpc" || Path::new(&c).is_absolute());
    }
}

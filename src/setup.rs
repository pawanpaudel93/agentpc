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
    // Only a missing or empty file starts from nothing: one that can't be read (not UTF-8,
    // no permission) would otherwise be overwritten with just this entry.
    let mut cfg: Value = match std::fs::read_to_string(path) {
        Ok(s) if !s.trim().is_empty() => serde_json::from_str(&s)
            .with_context(|| format!("{} is not valid JSON; not touching it", path.display()))?,
        Ok(_) => json!({}),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => bail!("can't read {}: {e}; not touching it", path.display()),
    };
    let root = cfg
        .as_object_mut()
        .with_context(|| format!("{} is not a JSON object", path.display()))?;
    let servers = root.entry("mcpServers").or_insert_with(|| json!({}));
    let servers = servers
        .as_object_mut()
        .with_context(|| format!("{}: mcpServers is not an object", path.display()))?;
    let server = servers.entry(SERVER).or_insert_with(|| json!({}));
    let server = server
        .as_object_mut()
        .with_context(|| format!("{}: agentpc is not an object", path.display()))?;
    server.insert("command".into(), json!(bin));
    server.insert("args".into(), json!(["mcp"]));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_atomic(path, &(serde_json::to_string_pretty(&cfg)? + "\n"))?;
    Ok(())
}

/// Create a private temporary file beside a config without following or truncating an
/// existing path (a stale temp or symlink must be left untouched).
fn create_config_temp(path: &Path) -> Result<(PathBuf, std::fs::File)> {
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_file_name(format!(
        ".{}.agentpc-{}.tmp",
        path.file_name()
            .map_or("config".into(), |n| n.to_string_lossy()),
        std::process::id()
    ));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("create private config temp {}", tmp.display()))?;
    Ok((tmp, file))
}

/// Replace a config file whole: keep its old mode, but create the temp privately before
/// writing any content. Rename on the same filesystem keeps interruptions from tearing it.
fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let permissions = match std::fs::metadata(path) {
        Ok(m) => m.permissions(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::Permissions::from_mode(0o600)
        }
        Err(e) => return Err(e).with_context(|| format!("stat {}", path.display())),
    };
    let (tmp, mut file) = create_config_temp(path)?;
    let result = (|| -> Result<()> {
        file.write_all(contents.as_bytes())?;
        file.set_permissions(permissions)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    // Only a temp we created is eligible for removal: create_new failures never reach here.
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.with_context(|| format!("write {}", path.display()))
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

/// Point an existing `[mcp_servers.agentpc]` in ~/.codex/config.toml at `bin`, rewriting only
/// its `command` and `args` lines. False when there is no such section to update.
fn codex_update_command(bin: &str) -> Result<bool> {
    let path = user_home().join(".codex/config.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => bail!("can't read {}: {e}; not touching it", path.display()),
    };
    let Some(out) = codex_with_command(&text, bin)? else {
        return Ok(false);
    };
    write_atomic(&path, &out)?;
    log!("updated agentpc in {}", path.display());
    Ok(true)
}

/// Parse without quoting input in the error: a client's config can contain credentials.
fn codex_document(text: &str) -> Result<toml_edit::DocumentMut> {
    text.parse()
        .map_err(|_| anyhow::anyhow!("Codex config is not valid TOML; not touching it"))
}

fn codex_server(
    document: &mut toml_edit::DocumentMut,
) -> Result<Option<&mut dyn toml_edit::TableLike>> {
    let Some(servers) = document.get_mut("mcp_servers") else {
        return Ok(None);
    };
    let servers = servers
        .as_table_like_mut()
        .context("Codex mcp_servers is not a table; not touching it")?;
    let Some(server) = servers.get_mut(SERVER) else {
        return Ok(None);
    };
    Ok(Some(server.as_table_like_mut().context(
        "Codex mcp_servers.agentpc is not a table; not touching it",
    )?))
}

/// Keep a managed field's surrounding whitespace and comments when replacing its value.
fn set_codex_value(server: &mut dyn toml_edit::TableLike, key: &str, mut value: toml_edit::Value) {
    if let Some(old) = server.get(key).and_then(toml_edit::Item::as_value) {
        *value.decor_mut() = old.decor().clone();
    }
    server.insert(key, toml_edit::Item::Value(value));
}

/// Set only the existing server's command and args, preserving other settings and TOML
/// formatting. Replacing parsed values also handles multiline arrays and quoted keys.
fn codex_with_command(text: &str, bin: &str) -> Result<Option<String>> {
    let mut document = codex_document(text)?;
    let Some(server) = codex_server(&mut document)? else {
        return Ok(None);
    };
    set_codex_value(server, "command", bin.into());
    let mut args = toml_edit::Array::new();
    args.push("mcp");
    set_codex_value(server, "args", args.into());
    Ok(Some(document.to_string()))
}

/// Add timeout defaults only when absent, using the same parsed server table as updates.
fn codex_with_timeouts(text: &str) -> Result<Option<String>> {
    let mut document = codex_document(text)?;
    let Some(server) = codex_server(&mut document)? else {
        return Ok(None);
    };
    let mut changed = false;
    for (key, default) in [("tool_timeout_sec", 900), ("startup_timeout_sec", 60)] {
        if !server.contains_key(key) {
            set_codex_value(server, key, default.into());
            changed = true;
        }
    }
    Ok(changed.then(|| document.to_string()))
}

/// codex has no CLI flag for MCP timeouts, so add them to the `[mcp_servers.agentpc]` block
/// `codex mcp add` just wrote. Image builds and first-boot resumes take minutes; codex's
/// short default would abort the tool call. Existing values are left untouched.
fn codex_set_timeouts() -> Result<()> {
    let path = user_home().join(".codex/config.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => bail!("can't read {}: {e}; not touching it", path.display()),
    };
    let Some(out) = codex_with_timeouts(&text)? else {
        return Ok(());
    };
    write_atomic(&path, &out)?;
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
                // An existing entry keeps its other settings (env such as AGENTPC_HOME,
                // approvals, timeouts); only the command is brought up to date.
                if !codex_update_command(&bin)? {
                    register_cmd("codex", &["mcp", "add", SERVER, "--", &bin, "mcp"], false)?;
                }
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
    let macos = macos_version();
    check(
        &match &macos {
            Some(v) => format!("macOS {v}"),
            None => "macOS".into(),
        },
        std::env::consts::OS == "macos",
        "",
    );
    // hvf_tso.c needs macOS 15's Hypervisor API; older hosts fall back to FEX's emulation.
    if let Some(v) = &macos
        && version_below(v, (15, 0))
    {
        println!("  warn x86apps VMs use emulated TSO (hardware TSO needs macOS 15+)");
    }
    let qemu_ok = edk2().is_ok();
    let qemu_version = qemu_ok.then(qemu_version).flatten();
    check(
        &match &qemu_version {
            Some(v) => format!("qemu {v}"),
            None => "qemu".into(),
        },
        qemu_ok,
        QEMU_HINT,
    );
    if let Some(v) = &qemu_version
        && version_below(v, QEMU_MIN)
    {
        println!(
            "  warn qemu {v} is older than {}.{}; saving and resuming VMs may fail — upgrade with: brew upgrade qemu",
            QEMU_MIN.0, QEMU_MIN.1
        );
    }
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
            for image in images.iter().filter(|i| i.os == os) {
                if let Some(hint) = crate::image::outdated(image) {
                    println!("  info {image}: {hint}");
                }
            }
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

/// The oldest QEMU `doctor` doesn't warn about. VMs are saved and resumed through `file:`
/// migration URIs (`migrate`, `-incoming file:`, QEMU 8.1+) and screenshots use
/// `screendump` PNG output (7.1+); 9.0 is the conservative floor over both, and what
/// has been tested with HVF.
const QEMU_MIN: (u32, u32) = (9, 0);

/// The host's macOS version, e.g. "15.7.1".
fn macos_version() -> Option<String> {
    let out = Command::new("sysctl")
        .args(["-n", "kern.osproductversion"])
        .output()
        .ok()?;
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !v.is_empty()).then_some(v)
}

/// QEMU's version, from the first line of `qemu-system-aarch64 --version`.
fn qemu_version() -> Option<String> {
    let bin = crate::qemu::qemu_bin().ok()?;
    let out = Command::new(bin).arg("--version").output().ok()?;
    parse_qemu_version(&String::from_utf8_lossy(&out.stdout))
}

/// "QEMU emulator version 11.1.1 (Homebrew)\n..." -> "11.1.1".
fn parse_qemu_version(text: &str) -> Option<String> {
    let line = text.lines().next()?;
    let (_, rest) = line.split_once("version ")?;
    let v = rest.split_whitespace().next()?;
    v.starts_with(|c: char| c.is_ascii_digit())
        .then(|| v.to_string())
}

/// Whether a dotted version (`15.7.1`, `9.2.0`) is below `min` (major, minor). An
/// unparsable version isn't flagged.
fn version_below(v: &str, min: (u32, u32)) -> bool {
    let mut it = v.split(['.', '-']).map(|p| p.parse::<u32>().ok());
    match (it.next().flatten(), it.next().flatten().unwrap_or(0)) {
        (Some(major), minor) => (major, minor) < min,
        (None, _) => false,
    }
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
        write_atomic(path, &(serde_json::to_string_pretty(&cfg)? + "\n"))?;
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

/// The image a hidden build or snapshot VM (`_build-<image>`, `_snap-<image>`) makes. An
/// Arch build's VM records the Ubuntu image it runs, so its name is what tells.
fn scratch_image(name: &str) -> Option<Image> {
    name.strip_prefix("_build-")
        .or_else(|| name.strip_prefix("_snap-"))?
        .parse()
        .ok()
}

/// The image a half-written file in `images/` belongs to: `<image>.qcow2.tmp`,
/// `<image>.snapshot.qcow2.tmp`, `<image>.snapshot.state.tmp` (and its `.machine` sidecar),
/// `<image>.vars.fd.tmp`.
fn tmp_image(name: &str) -> Option<Image> {
    let s = name.strip_suffix(".machine").unwrap_or(name);
    let s = s.strip_suffix(".tmp")?;
    let s = s
        .strip_suffix(".qcow2")
        .or_else(|| s.strip_suffix(".state"))
        .or_else(|| s.strip_suffix(".vars.fd"))?;
    s.strip_suffix(".snapshot").unwrap_or(s).parse().ok()
}

/// The image a file in `images/` belongs to: `.<image>.lock`, `<image>.json`, its disks,
/// vars and snapshot files.
fn image_of_file(name: &str) -> Option<Image> {
    if let Some(n) = name.strip_prefix('.') {
        return n.strip_suffix(".lock")?.parse().ok();
    }
    [
        ".snapshot.state.machine",
        ".snapshot.state",
        ".snapshot.vars.fd",
        ".snapshot.qcow2",
        ".vars.fd",
        ".qcow2",
        ".json",
    ]
    .iter()
    .find_map(|suffix| name.strip_suffix(suffix))?
    .parse()
    .ok()
}

/// Current image locks must keep their inode even when the image is absent: another
/// process may already be waiting on it before a fresh build or pull begins.
fn canonical_image_lock(name: &str) -> bool {
    image_of_file(name).is_some_and(|image| name == format!(".{image}.lock"))
}

/// A file in `~/.agentpc/lib` that `clean` may remove: another build's `hvf-tso-*.dylib`
/// or a leftover `hvf-tso-*.tmp` from writing one, never this build's `current` library.
fn stale_tso_lib(name: &str, current: &str) -> bool {
    name.starts_with("hvf-tso-")
        && name != current
        && (name.ends_with(".dylib") || name.ends_with(".tmp") || name.contains(".tmp."))
}

/// Whether a build, pull or snapshot of `image` holds its lock right now.
/// The locks `clean` takes as it decides, held until its deletions are done: a build, pull or
/// checkpoint must not start using a file between the check and its removal.
#[derive(Default)]
struct Held {
    images: std::collections::HashMap<String, std::fs::File>,
    other: Vec<std::fs::File>,
}

impl Held {
    /// Whether someone else holds `image`'s lock (else it is now ours).
    fn image_busy(&mut self, image: &Image) -> bool {
        let key = image.to_string();
        if self.images.contains_key(&key) {
            return false;
        }
        match crate::instance::try_image_lock(image) {
            Some(f) => {
                self.images.insert(key, f);
                false
            }
            None => true,
        }
    }

    /// Whether someone else holds the lock at `path` (else it is now ours).
    fn busy(&mut self, path: &Path) -> bool {
        match crate::instance::try_lock(path) {
            Some(f) => {
                self.other.push(f);
                false
            }
            None => true,
        }
    }

    /// Whether any image's lock is held elsewhere: some build may be using the shared
    /// downloads.
    fn any_image_busy(&mut self) -> bool {
        let images: Vec<Image> = std::fs::read_dir(crate::instance::images_dir())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.strip_prefix('.')?.strip_suffix(".lock")?.parse().ok()
            })
            .collect();
        // Every lock taken, not only up to the first busy one.
        let mut busy = false;
        for i in &images {
            busy |= self.image_busy(i);
        }
        busy
    }
}

/// Delete what can be downloaded or rebuilt again, and leftovers of interrupted work.
/// Images and VMs are never touched; unused images are listed. Files a running build,
/// pull or push holds (by its lock) are kept.
pub fn clean(dry_run: bool) -> Result<String> {
    use crate::instance::{Instance, cache_dir, images_dir, instances_dir};
    let mut targets: Vec<(PathBuf, &str)> = Vec::new();
    let mut in_use: Vec<PathBuf> = Vec::new();
    let mut held = Held::default();
    let building = held.any_image_busy();
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
        let pull = name.strip_prefix("pull-").and_then(|i| i.parse().ok());
        let push = name.strip_prefix("push-").and_then(|i| i.parse().ok());
        let live = match (&pull, &push) {
            (Some(i), _) => held.image_busy(i),
            (_, Some(i)) => held.busy(&crate::registry::push_lock(i)),
            // Cloud images, ISOs and drivers a running build may be reading.
            _ => building,
        };
        if live {
            in_use.push(e.path());
            continue;
        }
        let why = if pull.is_some() || push.is_some() {
            "leftover from an interrupted pull or push"
        } else if name.ends_with(".iso") || name.ends_with(".img") {
            "download; fetched again when a build needs it"
        } else if name.starts_with("virtio-win") {
            "Windows drivers; fetched again when a build needs them"
        } else if name == "alarm" {
            "Arch Linux ARM tarball download; fetched again when a build needs it"
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
            // The image it makes, or the one it runs (an Arch build's helper drops that
            // one's lock once cloned, so the name is what counts there).
            let live = scratch_image(&name).is_some_and(|i| held.image_busy(&i))
                || inst.as_ref().is_some_and(|i| held.image_busy(&i.image));
            if live {
                in_use.push(e.path());
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
        // A real VM: sweep only unfinished checkpoint temp dirs, never a real checkpoint, and
        // none while a lifecycle operation (a checkpoint being written, say) holds its lock.
        if inst.as_ref().is_some_and(|i| held.busy(&i.lock_path())) {
            continue;
        }
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

    // Half-written image files from a crashed build, pull or snapshot (`.qcow2.tmp`,
    // `.state.tmp` and its `.tmp.machine` sidecar).
    for e in std::fs::read_dir(images_dir())
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.ends_with(".tmp") || name.ends_with(".tmp.machine") {
            let live = match tmp_image(&name) {
                Some(i) => held.image_busy(&i),
                None => building,
            };
            if live {
                in_use.push(e.path());
                continue;
            }
            targets.push((
                e.path(),
                "half-written image file from an interrupted build",
            ));
        } else if name.starts_with('.') && name.ends_with(".lock") && name != ".lock" {
            // Never unlink a canonical lock, even while holding it: a new opener would
            // lock a different inode while existing waiters still use the original.
            if canonical_image_lock(&name) {
                continue;
            }
            // Aliases and old names no current command uses are removable when idle.
            if held.busy(&e.path()) {
                in_use.push(e.path());
                continue;
            }
            targets.push((e.path(), "obsolete image lock file"));
        } else if let Some(i) = image_of_file(&name)
            && !i.exists()
        {
            // A deleted (or older agentpc's) image's lock, info or sidecar. Its lock held
            // means it is being built or pulled right now.
            if held.image_busy(&i) {
                in_use.push(e.path());
                continue;
            }
            targets.push((e.path(), "left over from an image that no longer exists"));
        }
    }

    // TSO libraries of other agentpc builds (and stray temps from writing one). A running
    // x86apps VM, hidden build VMs included, may have one loaded, so leave them all then.
    let tso_in_use = std::fs::read_dir(instances_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| Instance::load(&e.file_name().to_string_lossy()).ok())
        .any(|i| i.image.x86_apps() && i.running());
    let current_tso = crate::qemu::hvf_tso_path();
    let current_tso = current_tso
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    for e in std::fs::read_dir(home().join("lib"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = e.file_name().to_string_lossy().into_owned();
        if !stale_tso_lib(&name, &current_tso) {
            continue;
        }
        if tso_in_use {
            in_use.push(e.path());
        } else {
            targets.push((e.path(), "TSO library of another agentpc version"));
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
    for p in &in_use {
        let by = if p.starts_with(home().join("lib")) {
            "an x86apps VM is running"
        } else {
            "in use by a running build, pull or push"
        };
        out += &format!("kept: {} ({by})\n", p.display());
    }
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

    // Hidden build/snapshot VMs (`_build-*`, `_snap-*`) too: removing the data would orphan them.
    let all = std::fs::read_dir(crate::instance::instances_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| crate::instance::Instance::load(&e.file_name().to_string_lossy()).ok());
    for inst in all {
        if inst.running() {
            log!("stopping {}", inst.name);
            if keep_data && !inst.name.starts_with('_') {
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
    fn fixture_dir(label: &str) -> std::path::PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "agentpc-setup-{label}-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    #[test]
    fn atomic_config_temps_start_private_and_final_modes_are_preserved() {
        use std::os::unix::fs::PermissionsExt;
        let dir = fixture_dir("private-temp");
        let path = dir.join("synthetic.json");
        let (tmp, file) = create_config_temp(&path).unwrap();
        assert_eq!(file.metadata().unwrap().len(), 0);
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o077, 0);
        drop(file);
        std::fs::remove_file(tmp).unwrap();
        write_atomic(&path, "synthetic content").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        write_atomic(&path, "updated synthetic content").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "updated synthetic content"
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn atomic_config_write_never_clobbers_existing_temp_or_symlink_target() {
        use std::os::unix::fs::symlink;
        let dir = fixture_dir("temp-collision");
        let path = dir.join("synthetic.json");
        std::fs::write(&path, "original config").unwrap();
        let (tmp, file) = create_config_temp(&path).unwrap();
        drop(file);
        std::fs::write(&tmp, "existing temp").unwrap();
        assert!(write_atomic(&path, "replacement").is_err());
        assert_eq!(std::fs::read_to_string(&tmp).unwrap(), "existing temp");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original config");
        std::fs::remove_file(&tmp).unwrap();
        let target = dir.join("synthetic-target.txt");
        std::fs::write(&target, "leave this alone").unwrap();
        symlink(&target, &tmp).unwrap();
        assert!(write_atomic(&path, "replacement").is_err());
        assert!(
            std::fs::symlink_metadata(&tmp)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "leave this alone"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original config");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn json_register_rejects_nonobject_server_without_modifying_config() {
        let dir = fixture_dir("invalid-server");
        let path = dir.join("synthetic.json");
        let text = r#"{"mcpServers":{"agentpc":7}}"#;
        std::fs::write(&path, text).unwrap();
        assert!(json_register(&path, "/bin/agentpc").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn canonical_image_locks_are_kept_independent_of_image_existence() {
        for name in [
            ".ubuntu-24.04.lock",
            ".ubuntu-22.04.lock",
            ".arch-rolling.lock",
            ".arch-rolling-x86apps.lock",
            ".windows-11-25h2.lock",
        ] {
            assert!(canonical_image_lock(name), "must keep {name}");
        }
        for name in [
            ".ubuntu.lock",
            ".ubuntu-x86apps.lock",
            ".arch.lock",
            ".lock",
            "ubuntu-24.04.json",
        ] {
            assert!(!canonical_image_lock(name), "not canonical: {name}");
        }
    }

    #[test]
    fn json_register_never_overwrites_an_unreadable_config() {
        let dir = std::env::temp_dir().join(format!("agentpc-jr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("mcp.json");
        // Not UTF-8: reading fails, and the file must stay as it was.
        std::fs::write(&cfg, b"{\"mcpServers\": {\"x\": \"\xff\"}}").unwrap();
        assert!(super::json_register(&cfg, "/bin/agentpc").is_err());
        assert_eq!(
            std::fs::read(&cfg).unwrap(),
            b"{\"mcpServers\": {\"x\": \"\xff\"}}"
        );
        // A good config keeps its other servers, and no temp file is left behind.
        std::fs::write(
            &cfg,
            r#"{"mcpServers": {"other": {"command": "x"}, "agentpc": {"command": "/old", "args": ["old"], "env": {"AGENTPC_HOME": "/vms"}, "timeout": 900}}, "theme": 1}"#,
        )
        .unwrap();
        super::json_register(&cfg, "/bin/agentpc").unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        assert_eq!(v["mcpServers"]["agentpc"]["command"], "/bin/agentpc");
        assert_eq!(v["mcpServers"]["agentpc"]["args"], json!(["mcp"]));
        assert_eq!(v["mcpServers"]["agentpc"]["env"]["AGENTPC_HOME"], "/vms");
        assert_eq!(v["mcpServers"]["agentpc"]["timeout"], 900);
        assert_eq!(v["theme"], 1);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        // A missing file is created.
        let fresh = dir.join("new.json");
        super::json_register(&fresh, "/bin/agentpc").unwrap();
        assert!(fresh.is_file());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn codex_update_keeps_other_settings() {
        let text = "[other]\nx = 1\n\n[mcp_servers.agentpc]\ncommand = \"/old/agentpc\"\nargs = [\"mcp\"]\n\
                    tool_timeout_sec = 1200\nenv = { AGENTPC_HOME = \"/vms\" }\n\n[tail]\ny = 2\n";
        let out = super::codex_with_command(text, "/new/agent \"pc\"")
            .unwrap()
            .unwrap();
        let document = super::codex_document(&out).unwrap();
        assert_eq!(
            document["mcp_servers"]["agentpc"]["command"].as_str(),
            Some("/new/agent \"pc\"")
        );
        assert!(!out.contains("/old/agentpc"));
        for keep in [
            "tool_timeout_sec = 1200",
            "AGENTPC_HOME = \"/vms\"",
            "[other]",
            "x = 1",
            "[tail]",
            "y = 2",
        ] {
            assert!(out.contains(keep), "lost {keep}: {out}");
        }
        assert_eq!(out.matches("args = ").count(), 1);
        let out = super::codex_with_command("[mcp_servers.agentpc]\nenv = {}\n", "/b")
            .unwrap()
            .unwrap();
        let document = super::codex_document(&out).unwrap();
        assert_eq!(
            document["mcp_servers"]["agentpc"]["command"].as_str(),
            Some("/b")
        );
        assert_eq!(
            document["mcp_servers"]["agentpc"]["args"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(
            super::codex_with_command("[mcp_servers.other]\n", "/b")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn codex_update_handles_multiline_and_inline_tables() {
        for text in [
            "[mcp_servers.\"agentpc\"] # keep header\n\"command\" = \"/old\" # keep command\nargs = [\n  \"mcp\",\n  \"old-extra\",\n]\n[mcp_servers.agentpc.env]\nAGENTPC_HOME = \"/vms\"\n",
            "mcp_servers = { agentpc = { command = '/old', args = ['mcp', 'old-extra'], env = { AGENTPC_HOME = '/vms' } } }\n",
        ] {
            let out = codex_with_command(text, "/new/path\\with\"quotes")
                .unwrap()
                .unwrap();
            let document = codex_document(&out).unwrap();
            let server = &document["mcp_servers"]["agentpc"];
            assert_eq!(server["command"].as_str(), Some("/new/path\\with\"quotes"));
            let args = server["args"].as_array().unwrap();
            assert_eq!(args.len(), 1);
            assert_eq!(args.get(0).unwrap().as_str(), Some("mcp"));
            assert_eq!(server["env"]["AGENTPC_HOME"].as_str(), Some("/vms"));
            if text.contains("# keep header") {
                assert!(out.contains("# keep header"));
                assert!(out.contains("# keep command"));
            }
        }
    }

    #[test]
    fn codex_timeouts_preserve_values_and_handle_quoted_headers() {
        let text = "[mcp_servers.\"agentpc\"] # server\ntool_timeout_sec = 1200\nargs = [\n  \"mcp\",\n]\n[mcp_servers.agentpc.env]\nAGENTPC_HOME = \"/vms\"\n";
        let out = codex_with_timeouts(text).unwrap().unwrap();
        let document = codex_document(&out).unwrap();
        let server = &document["mcp_servers"]["agentpc"];
        assert_eq!(server["tool_timeout_sec"].as_integer(), Some(1200));
        assert_eq!(server["startup_timeout_sec"].as_integer(), Some(60));
        assert_eq!(server["env"]["AGENTPC_HOME"].as_str(), Some("/vms"));
        assert!(out.contains("# server"));
        assert!(codex_with_timeouts(&out).unwrap().is_none());
        let out = codex_with_timeouts(
            "mcp_servers = { agentpc = { tool_timeout_seconds_legacy = 1 } }\n",
        )
        .unwrap()
        .unwrap();
        let document = codex_document(&out).unwrap();
        assert_eq!(
            document["mcp_servers"]["agentpc"]["tool_timeout_sec"].as_integer(),
            Some(900)
        );
        assert!(
            codex_with_timeouts("[mcp_servers.other]\n")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn invalid_codex_configs_are_rejected_without_echoing_input() {
        let invalid = "[mcp_servers.agentpc\n";
        let error = codex_with_command(invalid, "/b").unwrap_err().to_string();
        assert_eq!(error, "Codex config is not valid TOML; not touching it");
        assert!(codex_with_timeouts(invalid).is_err());
        for text in ["mcp_servers = 3\n", "[mcp_servers]\nagentpc = 3\n"] {
            assert!(codex_with_command(text, "/b").is_err());
            assert!(codex_with_timeouts(text).is_err());
        }
    }

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
    fn names_the_image_of_scratch_files() {
        let name = |i: Option<Image>| i.map(|i| i.to_string());
        assert_eq!(
            name(scratch_image("_build-arch-rolling-x86apps")),
            Some("arch-rolling-x86apps".into())
        );
        assert_eq!(
            name(scratch_image("_snap-ubuntu-24.04")),
            Some("ubuntu-24.04".into())
        );
        assert_eq!(name(scratch_image("ubuntu-1")), None);
        for (file, image) in [
            ("ubuntu-24.04.qcow2.tmp", "ubuntu-24.04"),
            ("ubuntu-24.04.snapshot.qcow2.tmp", "ubuntu-24.04"),
            ("arch-rolling.snapshot.state.tmp", "arch-rolling"),
            ("ubuntu-24.04-x86apps.vars.fd.tmp", "ubuntu-24.04-x86apps"),
            (
                "windows-11-25h2.snapshot.state.tmp.machine",
                "windows-11-25h2",
            ),
        ] {
            assert_eq!(name(tmp_image(file)), Some(image.into()), "{file}");
        }
        assert_eq!(name(tmp_image("ubuntu-24.04.qcow2")), None);
        for (file, image) in [
            (".ubuntu-22.04.lock", "ubuntu-22.04"),
            (".ubuntu-x86apps.lock", "ubuntu-24.04-x86apps"),
            ("ubuntu-26.04.snapshot.state.machine", "ubuntu-26.04"),
            ("arch-rolling.snapshot.vars.fd", "arch-rolling"),
            ("windows-11-25h2.qcow2", "windows-11-25h2"),
            ("ubuntu-24.04-x86apps.json", "ubuntu-24.04-x86apps"),
        ] {
            assert_eq!(name(image_of_file(file)), Some(image.into()), "{file}");
        }
        // Not an image's: the creation lock, other files.
        assert_eq!(name(image_of_file(".lock")), None);
        assert_eq!(name(image_of_file("notes.txt")), None);
        assert_eq!(name(tmp_image("junk.tmp")), None);
    }

    #[test]
    fn picks_stale_tso_libraries() {
        let cur = "hvf-tso-1a2b3c4d.dylib";
        assert!(stale_tso_lib("hvf-tso-deadbeef.dylib", cur));
        assert!(stale_tso_lib("hvf-tso-1a2b3c4d.4242.tmp", cur));
        assert!(stale_tso_lib("hvf-tso-deadbeef.4242.tmp", cur));
        assert!(!stale_tso_lib(cur, cur));
        assert!(!stale_tso_lib("other.dylib", cur));
        assert!(!stale_tso_lib("hvf-tso-notes.txt", cur));
    }

    #[test]
    fn reads_versions() {
        assert_eq!(
            parse_qemu_version(
                "QEMU emulator version 11.1.1\nCopyright (c) 2003-2025 Fabrice Bellard"
            )
            .as_deref(),
            Some("11.1.1")
        );
        assert_eq!(
            parse_qemu_version("QEMU emulator version 9.2.0 (Homebrew)").as_deref(),
            Some("9.2.0")
        );
        assert_eq!(parse_qemu_version("garbage"), None);
        assert!(version_below("8.2.1", QEMU_MIN));
        assert!(!version_below("9.0.0", QEMU_MIN));
        assert!(!version_below("11.1.1", QEMU_MIN));
        assert!(version_below("14.6.1", (15, 0)));
        assert!(!version_below("15.0", (15, 0)));
        assert!(!version_below("26.1", (15, 0)));
        assert!(!version_below("unknown", (15, 0)));
    }

    #[test]
    fn cmd_name_is_usable() {
        // Either the bare command (on PATH) or an absolute path to a real binary; never empty.
        let c = cmd_name();
        assert!(!c.is_empty());
        assert!(c == "agentpc" || Path::new(&c).is_absolute());
    }
}

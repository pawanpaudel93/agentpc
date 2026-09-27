//! MCP gateway over stdio. Lifecycle tools wrap `ops`; `use_desktop_tool` forwards to the
//! instance's own desktop-control server (Windows-MCP over HTTP, cua-driver over SSH)
//! through one session kept open per instance, so element references returned by one
//! call stay valid in the next.

use std::collections::HashMap;
use std::os::fd::AsFd;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, Implementation, JsonObject,
    ProgressNotificationParam, ServerCapabilities, ServerConfig,
};
use rmcp::service::{RequestContext, RunningService};
use rmcp::transport::{ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{
    RoleClient, RoleServer, ServerHandler, ServiceError, ServiceExt, tool, tool_handler,
    tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::instance::{Image, Instance, Os};
use crate::{ops, qemu};

const INSTRUCTIONS: &str = "\
Controls disposable Windows and Ubuntu desktop VMs on this Mac.

Typical flow: list_vms -> create_vm (or start_vm) -> take_screenshot -> list_desktop_tools ->
use_desktop_tool(...) -> take_screenshot to verify. reset_vm returns an instance to a clean state;
checkpoint_vm/restore_vm save and return to any point in seconds (disk and memory).
Windows desktop tools come from Windows-MCP (call Snapshot first; Click/Type need a loc
[x, y] or label). Ubuntu desktop tools come from cua-driver (keyboard/mouse input needs
\"delivery_mode\": \"foreground\"). run_command runs PowerShell on Windows and bash on Ubuntu.
create_vm takes an optional version (Ubuntu release like \"22.04\"; Windows \"11-25h2\",
\"11-24h2\" or \"11-23h2\"); list_vms shows which images exist.
";

pub async fn serve() -> Result<()> {
    let out = protocol_stdout()?;
    Gateway::default()
        .serve((tokio::io::stdin(), out))
        .await?
        .waiting()
        .await?;
    Ok(())
}

/// Returns a private handle on stdout for the protocol and points fd 1 at stderr, so
/// no child process (qemu, qemu-img, curl) can write into the JSON-RPC stream.
fn protocol_stdout() -> Result<tokio::fs::File> {
    unsafe extern "C" {
        fn dup2(src: i32, dst: i32) -> i32;
    }
    let fd = std::io::stdout().as_fd().try_clone_to_owned()?;
    // SAFETY: dup2 on the process's own standard descriptors.
    if unsafe { dup2(2, 1) } < 0 {
        bail!("redirect stdout: {}", std::io::Error::last_os_error());
    }
    Ok(tokio::fs::File::from_std(fd.into()))
}

type Client = Arc<RunningService<RoleClient, ()>>;
/// Per-instance slot, locked while connecting so concurrent calls share one session.
type Slot = Arc<tokio::sync::Mutex<Option<Client>>>;

#[derive(Clone, Default)]
struct Gateway {
    sessions: Arc<Mutex<HashMap<String, Slot>>>,
}

#[derive(Deserialize, JsonSchema)]
struct NameArgs {
    name: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(inline)]
enum OsArg {
    Windows,
    Ubuntu,
}

#[derive(Deserialize, JsonSchema)]
struct CreateArgs {
    os: OsArg,
    /// Default: ubuntu 24.04, windows 11. Ubuntu: any release, e.g. "22.04", "26.04".
    /// Windows: "11-25h2", "11-24h2" or "11-23h2". Omitted, Windows uses 25H2, or the newest
    /// installed Windows 11 image if 25H2 isn't built.
    version: Option<String>,
    name: Option<String>,
    /// Memory in GB (default 8 on Windows, 4 on Ubuntu). A non-default size boots cold
    /// (~25 s Windows, ~15 s Ubuntu) instead of resuming the image's snapshot.
    memory_gb: Option<u32>,
    /// CPUs (default 4). A non-default count boots cold, like memory_gb.
    cpus: Option<u32>,
    /// Cut the VM off from the internet and this Mac; run_command, files, the desktop tools
    /// and forward_port still work. For testing offline behaviour or untrusted software.
    #[serde(default)]
    offline: bool,
}

#[derive(Deserialize, JsonSchema)]
struct CheckpointArgs {
    name: String,
    /// Letters, digits, `.`, `-` and `_`, e.g. "deps-installed".
    label: String,
}

#[derive(Deserialize, JsonSchema)]
struct ExecArgs {
    name: String,
    command: String,
    /// Seconds to wait for the command (default 120). Ignored with `background`.
    #[serde(default = "default_timeout")]
    timeout: u64,
    /// Start the command detached and return at once, for servers and long jobs: it keeps
    /// running after this call. Returns where its output goes and how to stop it.
    #[serde(default)]
    background: bool,
}

fn default_timeout() -> u64 {
    120
}

#[derive(Deserialize, JsonSchema)]
struct UploadArgs {
    name: String,
    /// File or directory on this Mac (absolute, or relative to the server's working directory).
    host_path: String,
    /// Destination in the VM; relative paths are under the agent user's home
    /// (e.g. "Downloads/" on Windows, "/tmp/" on Ubuntu).
    guest_path: String,
}

#[derive(Deserialize, JsonSchema)]
struct DownloadArgs {
    name: String,
    /// File or directory in the VM; relative paths are under the agent user's home.
    guest_path: String,
    /// Destination on this Mac (absolute, or relative to the server's working directory).
    host_path: String,
}

#[derive(Deserialize, JsonSchema)]
struct ForwardArgs {
    name: String,
    /// Port a server listens on inside the VM.
    guest_port: u16,
    /// Port on 127.0.0.1 of this Mac; a free one is picked if omitted.
    host_port: Option<u16>,
}

#[derive(Deserialize, JsonSchema)]
struct ToolsArgs {
    name: String,
    tool: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct DesktopArgs {
    name: String,
    tool: String,
    arguments: Option<JsonObject>,
}

#[tool_router]
impl Gateway {
    #[tool(
        description = "List VM instances (name, image, state, size, checkpoints, viewer URL) and which images\n\
                          exist.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_vms(&self) -> CallToolResult {
        text(blocking(ops::list_json).await)
    }

    #[tool(
        description = "Create and boot a new instance cloned from its image (~1 s ubuntu, ~4 s windows).\n\
                          Returns once the desktop and its control server are ready.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn create_vm(
        &self,
        Parameters(a): Parameters<CreateArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let os = match a.os {
            OsArg::Windows => Os::Windows,
            OsArg::Ubuntu => Os::Ubuntu,
        };
        text(
            with_progress(&ctx, move || {
                let name = match a.version {
                    Some(v) => format!("{os}-{v}"),
                    None => os.to_string(),
                };
                ops::create(
                    &Image::resolve(&name)?,
                    a.name.as_deref(),
                    a.memory_gb,
                    a.cpus,
                    a.offline,
                )
            })
            .await,
        )
    }

    #[tool(
        description = "Boot a stopped instance and wait until its desktop is ready.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn start_vm(
        &self,
        Parameters(a): Parameters<NameArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        text(self.lifecycle(&a.name, ops::boot, &ctx).await)
    }

    #[tool(
        description = "Shut an instance down cleanly (its disk is kept).",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn stop_vm(
        &self,
        Parameters(a): Parameters<NameArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        text(self.lifecycle(&a.name, ops::stop, &ctx).await)
    }

    #[tool(
        description = "Discard all changes: restore the instance to a fresh copy of its image and boot it.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn reset_vm(
        &self,
        Parameters(a): Parameters<NameArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        text(self.lifecycle(&a.name, ops::reset, &ctx).await)
    }

    #[tool(
        description = "Stop an instance and delete it with its disk and checkpoints.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn delete_vm(
        &self,
        Parameters(a): Parameters<NameArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        text(self.lifecycle(&a.name, ops::delete, &ctx).await)
    }

    #[tool(
        description = "Save the instance's disk and memory under a label (replacing an older one of that\n\
                          name). A running VM pauses for a few seconds and carries on. Use before a risky\n\
                          step; restore_vm returns to it in seconds.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn checkpoint_vm(
        &self,
        Parameters(a): Parameters<CheckpointArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        text(
            async {
                let inst = load(&a.name)?;
                with_progress(&ctx, move || ops::checkpoint(&inst, &a.label)).await
            }
            .await,
        )
    }

    #[tool(
        description = "Put the instance back exactly as it was at a checkpoint and start it (resumes in\n\
                          seconds). Port forwards must be set up again.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn restore_vm(
        &self,
        Parameters(a): Parameters<CheckpointArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.drop_session(&a.name);
        text(
            async {
                let inst = load(&a.name)?;
                with_progress(&ctx, move || ops::restore(&inst, &a.label)).await
            }
            .await,
        )
    }

    #[tool(
        description = "Screenshot the instance's display from the hypervisor. Works at any time, even while\n\
                          booting or when the desktop server is unresponsive.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn take_screenshot(&self, Parameters(a): Parameters<NameArgs>) -> CallToolResult {
        let png = async {
            let inst = load(&a.name)?;
            blocking(move || qemu::screenshot(&inst, &inst.dir.join("screen.png"))).await
        };
        reply(png.await.map(|png| {
            // A PNG's width and height sit at bytes 16..24 of its IHDR chunk.
            let dim = |i: usize| {
                png.get(i..i + 4)
                    .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
            };
            let size = match (dim(16), dim(20)) {
                (Some(w), Some(h)) => format!(
                    "{w}x{h} screenshot; pixel coordinates in it are the screen coordinates the \
                     desktop tools use (no scaling)."
                ),
                _ => "screenshot".into(),
            };
            vec![
                ContentBlock::image(
                    base64::engine::general_purpose::STANDARD.encode(&png),
                    "image/png",
                ),
                ContentBlock::text(size),
            ]
        }))
    }

    #[tool(
        description = "Run a shell command in the instance over SSH: PowerShell on windows, bash on ubuntu.\n\
                          Returns the exit code, stdout and stderr; each stream is trimmed to its first and\n\
                          last 10,000 characters (write big output to a file and download_file it).\n\
                          With background: true the command keeps running after the call (servers, long\n\
                          jobs); the reply says where its output goes and how to stop it.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = true
        )
    )]
    async fn run_command(&self, Parameters(a): Parameters<ExecArgs>) -> CallToolResult {
        if a.background {
            return text(exec_background(&a.name, &a.command).await);
        }
        text(exec(&a.name, &a.command, a.timeout).await)
    }

    #[tool(
        description = "Copy a file or directory from this Mac into a VM (scp).",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn upload_file(&self, Parameters(a): Parameters<UploadArgs>) -> CallToolResult {
        text(
            async {
                let inst = load(&a.name)?;
                let host = std::path::absolute(&a.host_path)?;
                blocking(move || ops::upload(&inst, &host, &a.guest_path)).await
            }
            .await,
        )
    }

    #[tool(
        description = "Copy a file or directory from a VM to this Mac (scp).",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn download_file(&self, Parameters(a): Parameters<DownloadArgs>) -> CallToolResult {
        text(
            async {
                let inst = load(&a.name)?;
                let host = std::path::absolute(&a.host_path)?;
                blocking(move || ops::download(&inst, &a.guest_path, &host)).await
            }
            .await,
        )
    }

    #[tool(
        description = "Make a server running inside a VM reachable from this Mac: forwards a port on\n\
                          127.0.0.1 to the guest port until the VM stops. Returns the host address.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn forward_port(&self, Parameters(a): Parameters<ForwardArgs>) -> CallToolResult {
        text(
            async {
                let inst = load(&a.name)?;
                blocking(move || ops::forward(&inst, a.guest_port, a.host_port)).await
            }
            .await,
        )
    }

    #[tool(
        description = "List the desktop-control tools available in an instance (name + summary), or the full\n\
                          input schema of one tool when `tool` is given. Call them with `use_desktop_tool`.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_desktop_tools(&self, Parameters(a): Parameters<ToolsArgs>) -> CallToolResult {
        let r = async {
            let tools = self
                .with_session(&a.name, "list_tools", |c: Client| async move {
                    c.list_all_tools().await
                })
                .await?;
            let Some(want) = a.tool else {
                let lines: Vec<String> = tools
                    .iter()
                    .map(|t| {
                        let desc = t.description.as_deref().unwrap_or("").trim();
                        let first: String = desc
                            .lines()
                            .next()
                            .unwrap_or("")
                            .chars()
                            .take(160)
                            .collect();
                        format!("{}: {first}", t.name)
                    })
                    .collect();
                return Ok(lines.join("\n"));
            };
            let t = tools
                .iter()
                .find(|t| t.name == want)
                .ok_or_else(|| anyhow!("no tool '{want}' in {}", a.name))?;
            Ok(serde_json::to_string_pretty(
                &json!({"name": t.name, "description": t.description, "input_schema": t.input_schema}),
            )?)
        };
        text(r.await)
    }

    #[tool(
        description = "Call a desktop-control tool inside an instance (see list_desktop_tools for names and schemas).\n\
                          Screenshots and other content are returned as-is.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = true
        )
    )]
    async fn use_desktop_tool(&self, Parameters(a): Parameters<DesktopArgs>) -> CallToolResult {
        let params = CallToolRequestParams::new(a.tool.clone())
            .with_arguments(a.arguments.unwrap_or_default());
        let r = self
            .with_session(&a.name, &a.tool, |c: Client| {
                let params = params.clone();
                async move { c.call_tool(params).await }
            })
            .await;
        match r {
            Ok(res) if res.is_error == Some(true) => {
                let msg: Vec<&str> = res
                    .content
                    .iter()
                    .filter_map(|c| c.as_text().map(|t| t.text.as_str()))
                    .collect();
                let msg = msg.join(" ");
                CallToolResult::error(vec![ContentBlock::text(if msg.is_empty() {
                    format!("{} failed", a.tool)
                } else {
                    msg
                })])
            }
            Ok(res) => {
                // Inner servers (cua-driver's browser tools) put ids like tab_id only in
                // structured content, and many clients show only text, so mirror it as text.
                let mut content = res.content;
                if let Some(sc) = &res.structured_content {
                    let json = serde_json::to_string_pretty(sc).unwrap_or_default();
                    if !content
                        .iter()
                        .any(|c| c.as_text().is_some_and(|t| t.text.contains(&json)))
                    {
                        content.push(ContentBlock::text(json));
                    }
                }
                let mut out = CallToolResult::success(content);
                out.structured_content = res.structured_content;
                out
            }
            Err(e) => text(Err(e)),
        }
    }
}

#[tool_handler]
impl ServerHandler for Gateway {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("agentpc", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

impl Gateway {
    /// A lifecycle op invalidates the desktop session (guest restarted or gone).
    async fn lifecycle(
        &self,
        name: &str,
        op: fn(&Instance) -> Result<String>,
        ctx: &RequestContext<RoleServer>,
    ) -> Result<String> {
        self.drop_session(name);
        let inst = load(name)?;
        with_progress(ctx, move || op(&inst)).await
    }

    fn drop_session(&self, name: &str) {
        self.sessions.lock().unwrap().remove(name);
    }

    async fn session(&self, name: &str) -> Result<Client> {
        let slot = self
            .sessions
            .lock()
            .unwrap()
            .entry(name.to_string())
            .or_default()
            .clone();
        let mut slot = slot.lock().await;
        if let Some(c) = slot.as_ref().filter(|c| !c.is_closed()) {
            return Ok(c.clone());
        }
        let c = Arc::new(connect(name).await?);
        *slot = Some(c.clone());
        Ok(c)
    }

    /// Runs `f` on the instance's session. A transport failure usually means the guest
    /// restarted underneath us, so reconnect once and retry.
    async fn with_session<T, F>(&self, name: &str, what: &str, f: impl Fn(Client) -> F) -> Result<T>
    where
        F: Future<Output = Result<T, ServiceError>>,
    {
        for attempt in 1..=2 {
            match f(self.session(name).await?).await {
                Ok(v) => return Ok(v),
                Err(ServiceError::McpError(e)) => bail!("{what} failed in {name}: {}", e.message),
                Err(e) => {
                    self.drop_session(name);
                    if attempt == 2 {
                        bail!("{what} failed in {name}: {e}");
                    }
                }
            }
        }
        unreachable!()
    }
}

async fn connect(name: &str) -> Result<RunningService<RoleClient, ()>> {
    let inst = load(name)?;
    if !inst.running() {
        bail!("{name} is stopped; call start_vm first");
    }
    let session = async {
        match inst.os {
            Os::Windows => {
                let url = format!("http://127.0.0.1:{}/mcp", inst.mcp_port());
                Ok(
                    ().serve(StreamableHttpClientTransport::from_uri(url))
                        .await?,
                )
            }
            Os::Ubuntu => {
                let remote = format!("{} ~/.local/bin/cua-driver mcp", ops::UBUNTU_SESSION_ENV);
                let cmd = tokio::process::Command::new("ssh").configure(|c| {
                    c.args(ops::ssh_args(&inst, &remote));
                });
                Ok(().serve(TokioChildProcess::new(cmd)?).await?)
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(60), session)
        .await
        .map_err(|_| anyhow!("timed out after 60s"))
        .and_then(|r: Result<_>| r)
        .with_context(|| format!("cannot reach the desktop server in {name}"))
}

/// Characters kept from each end of a long stdout or stderr.
const OUTPUT_KEEP: usize = 10_000;

async fn exec(name: &str, command: &str, timeout: u64) -> Result<String> {
    let inst = load(name)?;
    let out = tokio::process::Command::new("ssh")
        .args(ops::ssh_args(&inst, command))
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(Duration::from_secs(timeout), out)
        .await
        .map_err(|_| anyhow!("command timed out after {timeout}s"))?
        .context("run ssh")?;
    // ssh exits 255 when it can't connect; the command's own code otherwise.
    let code = out.status.code().unwrap_or(-1);
    let mut text = format!("exit code: {code}");
    for (label, bytes) in [("stdout", &out.stdout), ("stderr", &out.stderr)] {
        let s = String::from_utf8_lossy(bytes);
        let s = s.trim_end();
        if !s.is_empty() {
            text += &format!("\n--- {label} ---\n{}", clip(s, OUTPUT_KEEP));
        }
    }
    if code != 0 {
        bail!("{text}");
    }
    Ok(text)
}

/// Start `command` detached from the SSH session, which would otherwise take it down when it
/// ends. On Windows that means a scheduled task in the logged-in session (so GUI apps show).
/// The command travels base64-encoded, so no quoting can break it.
async fn exec_background(name: &str, command: &str) -> Result<String> {
    use base64::Engine;
    let inst = load(name)?;
    if !inst.running() {
        bail!("{name} is stopped; call start_vm first");
    }
    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis();
    let b64 = base64::engine::general_purpose::STANDARD.encode(command);
    let script = match inst.os {
        // Separate lines: `a && b &` would background the whole list, and that shell would
        // hold the SSH session open.
        Os::Ubuntu => format!(
            "d=~/agentpc-bg; mkdir -p $d && echo {b64} | base64 -d > $d/{id}.sh || exit 1\n\
             {env} setsid nohup bash $d/{id}.sh > $d/{id}.log 2>&1 < /dev/null &\n\
             echo \"started in the background (pid $!); output: $HOME/agentpc-bg/{id}.log. \
             Read it with: tail -n 50 ~/agentpc-bg/{id}.log. Stop it with: kill $!\"",
            env = ops::UBUNTU_SESSION_ENV
        ),
        Os::Windows => format!(
            r#"$d = "$env:USERPROFILE\agentpc-bg"; New-Item -ItemType Directory -Force $d | Out-Null
$ps = "$d\{id}.ps1"; $log = "$d\{id}.log"; $task = 'agentpc-bg-{id}'
[IO.File]::WriteAllText($ps, [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{b64}')))
$a = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -Command `"& '$ps' *> '$log'`""
$p = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Highest
$s = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
Register-ScheduledTask -TaskName $task -Action $a -Principal $p -Settings $s -Force | Out-Null
Start-ScheduledTask -TaskName $task
"started in the background as scheduled task $task; output: $log. Read it with: Get-Content '$log' -Tail 50. Stop it with: Stop-ScheduledTask $task (and Stop-Process for anything it started)""#
        ),
    };
    let out = exec(name, &script, 60).await?;
    Ok(out
        .split_once("--- stdout ---\n")
        .map_or(out.clone(), |(_, s)| s.trim().to_string()))
}

/// Keep the first and last `keep` characters of `s`, noting how much was cut.
fn clip(s: &str, keep: usize) -> String {
    let n = s.chars().count();
    if n <= 2 * keep {
        return s.to_string();
    }
    let head: String = s.chars().take(keep).collect();
    let tail: String = s.chars().skip(n - keep).collect();
    format!(
        "{head}\n[... {} characters omitted ...]\n{tail}",
        n - 2 * keep
    )
}

fn load(name: &str) -> Result<Instance> {
    // Names become paths under the instances dir; keep `..` and `_build-*` out of reach.
    if name.starts_with(['_', '.']) || name.contains('/') {
        bail!("no instance '{name}'; see list_vms");
    }
    Instance::load(name).map_err(|_| anyhow!("no instance '{name}'; see list_vms"))
}

/// `blocking`, plus: when the client asked for progress (a progressToken), each progress
/// line agentpc logs meanwhile is sent to it as an MCP progress notification.
async fn with_progress<T: Send + 'static>(
    ctx: &RequestContext<RoleServer>,
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    let Some(token) = ctx.meta.get_progress_token() else {
        return blocking(f).await;
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let peer = ctx.peer.clone();
    let forward = tokio::spawn(async move {
        let mut n = 0.0;
        while let Some(line) = rx.recv().await {
            n += 1.0;
            let note = ProgressNotificationParam::new(token.clone(), n).with_message(line);
            let _ = peer.notify_progress(note).await;
        }
    });
    let result = blocking(move || ops::with_progress(tx, f)).await;
    let _ = forward.await;
    result
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f).await?
}

fn text(r: Result<String>) -> CallToolResult {
    reply(r.map(|s| vec![ContentBlock::text(s)]))
}

fn reply(r: Result<Vec<ContentBlock>>) -> CallToolResult {
    match r {
        Ok(content) => CallToolResult::success(content),
        Err(e) => CallToolResult::error(vec![ContentBlock::text(format!("{e:#}"))]),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn clips_long_output_keeping_both_ends() {
        assert_eq!(super::clip("short", 10), "short");
        let long: String = (0..100)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect();
        let c = super::clip(&long, 10);
        assert!(
            c.starts_with(&long[..10]) && c.ends_with(&long[90..]),
            "{c}"
        );
        assert!(c.contains("[... 80 characters omitted ...]"), "{c}");
        // Multi-byte characters are counted, not split.
        assert_eq!(super::clip(&"é".repeat(30), 10).matches('é').count(), 20);
    }
}

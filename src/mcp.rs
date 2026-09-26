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
    ServerCapabilities, ServerConfig,
};
use rmcp::service::RunningService;
use rmcp::transport::{ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServerHandler, ServiceError, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::instance::{Instance, Os};
use crate::{ops, qemu, viewer};

const INSTRUCTIONS: &str = "\
Controls disposable Windows 11 and Ubuntu desktop VMs on this Mac.

Typical flow: list_vms -> create_vm (or start_vm) -> take_screenshot -> list_desktop_tools ->
use_desktop_tool(...) -> take_screenshot to verify. reset_vm returns an instance to a clean state.
Windows desktop tools come from Windows-MCP (call Snapshot first; Click/Type need a loc
[x, y] or label). Ubuntu desktop tools come from cua-driver (keyboard/mouse input needs
\"delivery_mode\": \"foreground\"). run_command runs PowerShell on Windows and bash on Ubuntu.
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
    name: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct ExecArgs {
    name: String,
    command: String,
    #[serde(default = "default_timeout")]
    timeout: u64,
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
    #[tool(description = "List VM instances (name, os, state, viewer URL) and which images exist.")]
    async fn list_vms(&self) -> CallToolResult {
        text(blocking(|| {
            let instances: Vec<_> = Instance::list()?
                .iter()
                .map(|i| {
                    json!({
                        "name": i.name, "os": i.os, "state": if i.running() { "running" } else { "stopped" },
                        "slot": i.slot, "viewer": viewer::url(i), "ssh_port": i.ssh_port(),
                    })
                })
                .collect();
            let images: Vec<Value> = Os::ALL
                .into_iter()
                .filter(|os| os.image_disk().is_file())
                .map(|os| {
                    let info = crate::image::read_info(os);
                    json!({
                        "os": os,
                        "version": info.as_ref().map(|i| i.version.clone()),
                        "based_on": info.as_ref().map(|i| i.base.clone()),
                        "desktop_server": info.as_ref().map(|i| i.desktop_server.clone()),
                        "fast_start": os.has_snapshot(),
                    })
                })
                .collect();
            Ok(serde_json::to_string_pretty(&json!({"instances": instances, "images": images}))?)
        })
        .await)
    }

    #[tool(
        description = "Create and boot a new instance cloned from its image (~1 s ubuntu, ~4 s windows).\n\
                          Returns once the desktop and its control server are ready."
    )]
    async fn create_vm(&self, Parameters(a): Parameters<CreateArgs>) -> CallToolResult {
        let os = match a.os {
            OsArg::Windows => Os::Windows,
            OsArg::Ubuntu => Os::Ubuntu,
        };
        text(blocking(move || ops::create(os, a.name.as_deref())).await)
    }

    #[tool(description = "Boot a stopped instance and wait until its desktop is ready.")]
    async fn start_vm(&self, Parameters(a): Parameters<NameArgs>) -> CallToolResult {
        text(self.lifecycle(&a.name, ops::boot).await)
    }

    #[tool(description = "Shut an instance down cleanly (its disk is kept).")]
    async fn stop_vm(&self, Parameters(a): Parameters<NameArgs>) -> CallToolResult {
        text(self.lifecycle(&a.name, ops::stop).await)
    }

    #[tool(
        description = "Discard all changes: restore the instance to a fresh copy of its image and boot it."
    )]
    async fn reset_vm(&self, Parameters(a): Parameters<NameArgs>) -> CallToolResult {
        text(self.lifecycle(&a.name, ops::reset).await)
    }

    #[tool(description = "Stop an instance and delete it with its disk.")]
    async fn delete_vm(&self, Parameters(a): Parameters<NameArgs>) -> CallToolResult {
        text(self.lifecycle(&a.name, ops::delete).await)
    }

    #[tool(
        description = "Screenshot the instance's display from the hypervisor. Works at any time, even while\n\
                          booting or when the desktop server is unresponsive."
    )]
    async fn take_screenshot(&self, Parameters(a): Parameters<NameArgs>) -> CallToolResult {
        let png = async {
            let inst = load(&a.name)?;
            blocking(move || qemu::screenshot(&inst, &inst.dir.join("screen.png"))).await
        };
        reply(png.await.map(|png| {
            vec![ContentBlock::image(
                base64::engine::general_purpose::STANDARD.encode(png),
                "image/png",
            )]
        }))
    }

    #[tool(
        description = "Run a shell command in the instance over SSH: PowerShell on windows, bash on ubuntu.\n\
                          Returns combined stdout and stderr."
    )]
    async fn run_command(&self, Parameters(a): Parameters<ExecArgs>) -> CallToolResult {
        text(exec(&a.name, &a.command, a.timeout).await)
    }

    #[tool(description = "Copy a file or directory from this Mac into a VM (scp).")]
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

    #[tool(description = "Copy a file or directory from a VM to this Mac (scp).")]
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
                          127.0.0.1 to the guest port until the VM stops. Returns the host address."
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
                          input schema of one tool when `tool` is given. Call them with `use_desktop_tool`."
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
                          Screenshots and other content are returned as-is."
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
    async fn lifecycle(&self, name: &str, op: fn(&Instance) -> Result<String>) -> Result<String> {
        self.drop_session(name);
        let inst = load(name)?;
        blocking(move || op(&inst)).await
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
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let text = text.trim();
    if !out.status.success() {
        bail!(
            "{}",
            if text.is_empty() {
                format!("command failed ({})", out.status)
            } else {
                text.to_string()
            }
        );
    }
    Ok(text.to_string())
}

fn load(name: &str) -> Result<Instance> {
    // Names become paths under the instances dir; keep `..` and `_build-*` out of reach.
    if name.starts_with(['_', '.']) || name.contains('/') {
        bail!("no instance '{name}'; see list_vms");
    }
    Instance::load(name).map_err(|_| anyhow!("no instance '{name}'; see list_vms"))
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

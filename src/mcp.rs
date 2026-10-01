//! MCP gateway over stdio. Lifecycle tools wrap `ops`; `use_desktop_tool` forwards to the
//! instance's own desktop-control server (cua-driver over SSH)
//! through one session kept open per instance, so element references returned by one
//! call stay valid in the next.

use std::collections::{HashMap, HashSet};
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
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
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
Controls instant, resettable Windows, Ubuntu and Arch Linux ARM desktop VMs on this Mac. Treat
VMs as throwaway sandboxes. If list_vms shows an OS or image that create_vm's options don't
list, your tool definitions predate an agentpc update: reconnect the agentpc MCP server.

Flow: list_vms -> create_vm (or start_vm on one you created) -> take_screenshot -> list_desktop_tools ->
use_desktop_tool(...) -> take_screenshot to verify. reset_vm returns an instance to a clean
install; checkpoint_vm/restore_vm save and return to any point in seconds (disk and memory) --
checkpoint before a risky or slow-to-redo step. run_command runs PowerShell on Windows and bash
on Ubuntu and Arch; the guest login is agent/agent. create_vm takes an optional version (Ubuntu
release like \"22.04\"; Windows \"11-25h2\", \"11-24h2\", \"11-23h2\"; Arch is rolling: no version,
or \"rolling-YYYYMMDD\" / \"rolling-x86apps-YYYYMMDD\" to pin a published build; \"x86apps\" on
Ubuntu or Arch, see below); list_vms shows which images exist.
The first create of an image can take minutes (download/build); after that it's seconds.
All guests are ARM64. On Windows, x64 and x86 programs run through Prism emulation (slower;
no x64 drivers), so prefer an ARM64 build when one exists. For x86_64 and i386 Linux programs,
create ubuntu or arch with version \"x86apps\": they run through FEX translation, about 2x slower
(Node 6-7x). Go programs work; amd64 containers work with docker run --platform linux/amd64
(install Docker first: sudo apt install docker.io on Ubuntu; sudo pacman -Syu --noconfirm docker
&& sudo systemctl start docker on Arch); x86 Electron/Chromium apps need --no-sandbox. On Ubuntu,
x86 libraries install with sudo apt install libfoo:amd64 and x86 .debs with
sudo apt install ./app_amd64.deb (their install scripts see x86_64); on Arch (no multiarch), sudo fex-pacman
-Sy --noconfirm --needed <pkg> installs x86 packages into the x86 Arch tree FEX runs them in.
x86 systemd services run too, hardened ones included: a generator relaxes
MemoryDenyWriteExecute=/LockPersonality= (which stop FEX, as any JIT) for units whose ExecStart is
an x86 program. If a unit runs its x86 program through a script and dies at start with a SIGSEGV
inside FEX, sudo fex-unit <unit> does the same. An installer
that refuses non-x86_64 (uname -m) runs unmodified under the x86 bash: sudo FEXBash ./install.sh;
for a paste block (curl ... | sudo bash -s), start FEXBash and paste it there.
list_vms shows x86_tso: hardware (fast; needs macOS 15+) or emulated.
An Arch Linux ARM guest (os \"arch\") works like Ubuntu (XFCE, bash, the same desktop tools), but
packages come from pacman (sudo pacman -Syu --noconfirm <pkg>: Arch doesn't support partial
upgrades, and an image's package lists age; the first -Syu may upgrade everything, so give
run_command a longer timeout or background: true, and after a kernel upgrade reboot with
start_vm before loading new modules; /tmp is cleared at every boot) and its browser is Chromium:
launch_app {\"name\": \"chromium\", \"additional_arguments\": [\"<url>\"]} where Ubuntu uses google-chrome.

Desktop tools come from cua-driver on every OS: launch_app returns a pid and window_ids;
get_window_state(pid, window_id) returns numbered elements and a snapshot_id to pass with
element_index to click/type_text. On Ubuntu and Arch, keyboard/mouse input needs \"delivery_mode\":
\"foreground\"; on Windows, typing into the focused field, scroll, drag and right-click often do.
list_desktop_tools shows each tool's required arguments; a call with wrong arguments returns
the tool's argument list, and a wrong tool name returns close matches (\"did you mean ...\").
If a reply starts with a reconnect note (the VM or its driver restarted), snapshot ids and browser
sessions are gone: take a new snapshot and run browser_prepare again.

If get_window_state comes back \"degraded\" (no elements), act by pixels instead: pass x/y read
from the screenshot of a get_window_state call that included one (the default).

Web pages on Ubuntu (Google Chrome) and Arch (Chromium: use \"chromium\" for \"google-chrome\"):
- To read a page: launch_app {\"name\": \"google-chrome\", \"additional_arguments\": [\"<url>\"]},
  then get_window_state on its window; the page's text, links and fields are in the tree.
- To drive a page with the browser_* tools: browser_prepare {\"allow_launch\": true, \"profile\":
  {\"mode\": \"isolated_new\"}, \"session\": \"<label>\"} -> list_windows {\"pid\": <prepared_pid>} ->
  get_browser_state {pid, window_id} for target_id/tab_id -> browser_navigate / browser_click /
  browser_type. Pass the same session label on every call. A session ends after about 5
  minutes without calls, or if the driver connection drops (the reply then says so): run
  browser_prepare again.
- launch_app with `urls` opens them through the default handler and returns no pid; don't use
  the legacy `page` tool.

The desktop driver is pinned per image so tools match these docs; don't update it inside a VM
(reset_vm restores it).

Rules:
- Ownership: create your OWN uniquely named VM and work in it. Never reset/delete/restore a VM you
  did not create (list_vms shows each VM's owner) unless the user asks. Delete the VMs you created
  when you're done, unless the user wants them kept. VMs you created, started, reset or restored
  may be stopped automatically when this session ends. A VM with owner_running: false belongs to
  a session that is gone; if it was yours (your session restarted), create_vm with its name and
  image takes it back.
- Long jobs and servers: use run_command with background: true (it keeps running after the call
  and returns a job id); poll it with get_job_status. Foreground run_command times out (default 120 s).
- Reach a server in the VM from the Mac with forward_port (works even for servers bound to the
  guest's own 127.0.0.1); it returns a 127.0.0.1:<port> address and lasts until the VM stops.
  For a UDP server pass protocol: \"udp\"; it must listen on 0.0.0.0. list_forwards /
  delete_forward manage them. From inside the guest, 10.0.2.2 reaches this Mac.
- Don't start a Windows image build yourself -- if no Windows image exists, ask the user to
  build one (~12 min).
- Output from run_command is trimmed to the first and last 10,000 characters per stream; write big
  output to a file in the VM and download_file it.
- Don't put real credentials or secrets into a VM; VMs are reachable from anything on this Mac.
";

pub async fn serve() -> Result<()> {
    let out = protocol_stdout()?;
    let gateway = Gateway::new();
    // A server that was killed (or crashed) couldn't stop its VMs on the way out; stop them
    // now, in the background so the client's handshake isn't held up.
    if !ops::keep_running() {
        tokio::task::spawn_blocking(|| {
            let stopped = ops::stop_orphans();
            if !stopped.is_empty() {
                crate::log!(
                    "stopped {} VM(s) whose MCP server exited without stopping them: {}",
                    stopped.len(),
                    stopped.join(", ")
                );
            }
        });
    }
    let running = gateway.clone().serve((tokio::io::stdin(), out)).await?;
    running.waiting().await?;
    // Client disconnected / stdin EOF: stop the VMs this process left running so they don't
    // pile up. CLI-started VMs aren't in `owned`, so they're untouched.
    gateway.shutdown().await;
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

#[derive(Clone)]
struct Gateway {
    sessions: Arc<Mutex<HashMap<String, Slot>>>,
    /// VMs this server process created or started, for auto-stop on exit.
    owned: Arc<Mutex<HashSet<String>>>,
    /// Identifies this server process, so a VM's owner tag is unique per session.
    session_id: Arc<str>,
    /// VMs this process has connected to the desktop driver of, and those whose connection
    /// has since been replaced (the driver's sessions, bindings and snapshot ids went with
    /// the old one) and whose next desktop call should say so.
    connected: Arc<Mutex<HashSet<String>>>,
    reconnected: Arc<Mutex<HashSet<String>>>,
    /// The agentpc binary this server runs, as found at startup, to notice an update.
    exe: Option<(std::path::PathBuf, u64)>,
}

impl Gateway {
    fn new() -> Self {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        Self {
            sessions: Default::default(),
            owned: Default::default(),
            session_id: format!("{pid:x}{nanos:x}").into(),
            connected: Default::default(),
            reconnected: Default::default(),
            exe: std::env::current_exe()
                .ok()
                .and_then(|p| exe_id(&p).map(|id| (p, id))),
        }
    }

    /// A note when the agentpc binary was replaced (updated) since this server started: an
    /// agent otherwise trusts this older server's tools and schemas until its session restarts.
    fn stale_note(&self) -> Option<String> {
        let (path, id) = self.exe.as_ref()?;
        if exe_id(path)? == *id {
            return None;
        }
        let now = std::process::Command::new(path)
            .arg("--version")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        Some(format!(
            "Note: agentpc was updated since this MCP server started (this server is agentpc {}, \
             the installed one is {now}); its tools and options may be out of date. Reconnect the \
             agentpc MCP server (or restart the session) to use the update.\n",
            env!("CARGO_PKG_VERSION")
        ))
    }

    /// Owner tag stored on VMs created this session: the MCP client's name plus a per-process id.
    fn owner_tag(&self, ctx: &RequestContext<RoleServer>) -> String {
        let client = ctx
            .peer
            .peer_info()
            .map(|i| i.client_info.name.clone())
            .unwrap_or_else(|| "mcp".into());
        format!("{client} [{}]", self.session_id)
    }

    /// Make this session a VM's owner: its tag (with this process, for `owner_running`) and
    /// the auto-stop set agree.
    fn claim(&self, name: &str, ctx: &RequestContext<RoleServer>) {
        if let Ok(inst) = load(name) {
            let _ = ops::set_owner(&inst, &self.owner_tag(ctx));
        }
        self.owned.lock().unwrap().insert(name.to_string());
    }

    /// Best-effort, bounded shutdown: gracefully stop the VMs this session left running, unless
    /// AGENTPC_KEEP_RUNNING=1. Force-quit any that don't stop in time so the process can exit.
    async fn shutdown(&self) {
        if ops::keep_running() {
            return;
        }
        let names: Vec<String> = self.owned.lock().unwrap().iter().cloned().collect();
        let running: Vec<Instance> = names
            .into_iter()
            .filter_map(|n| load(&n).ok())
            .filter(|i| i.running())
            .collect();
        if running.is_empty() {
            return;
        }
        crate::log!(
            "MCP server exiting; stopping {} VM(s) it started",
            running.len()
        );
        let stops = running.iter().cloned().map(|inst| {
            blocking(move || {
                // Tear down this VM's port forwards before it stops, so no stale tunnel or
                // pid file is left behind.
                ops::stop_forwards(&inst);
                ops::stop(&inst)
            })
        });
        let _ =
            tokio::time::timeout(Duration::from_secs(45), futures::future::join_all(stops)).await;
        for inst in &running {
            if inst.running() {
                qemu::quit(inst);
            }
        }
    }
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
    Arch,
}

#[derive(Deserialize, JsonSchema)]
struct CreateArgs {
    os: OsArg,
    /// Default: ubuntu 24.04, windows 11, arch rolling. Ubuntu: any release, e.g. "22.04", "26.04";
    /// "x86apps" (or "<release>-x86apps", e.g. "22.04-x86apps") is Ubuntu that also runs
    /// x86_64 and i386 Linux programs; "24.04-YYYYMMDD" pins a published build (download-only).
    /// Windows: "11-25h2", "11-24h2" or "11-23h2". Omitted, Windows uses 25H2, or the newest
    /// installed Windows 11 image if 25H2 isn't built. Arch: the default, "rolling"; "x86apps"
    /// (Arch that also runs x86 Linux programs); or a pinned download, "rolling-YYYYMMDD" /
    /// "rolling-x86apps-YYYYMMDD".
    version: Option<String>,
    /// VM name: up to 64 letters, digits, ".", "-" or "_" (default "<os>-<n>"). With a name,
    /// retrying a create that timed out returns the same VM instead of making another.
    name: Option<String>,
    /// Memory in GB (default 8 on Windows, 4 on Ubuntu and Arch). A non-default size boots cold
    /// (~25 s Windows, ~15 s Ubuntu and Arch) instead of resuming the image's snapshot.
    #[schemars(range(min = 2, max = 64))]
    memory_gb: Option<u32>,
    /// CPUs (default 4). A non-default count boots cold, like memory_gb.
    #[schemars(range(min = 1, max = 16))]
    cpus: Option<u32>,
    /// Cut the VM off from the internet and this Mac; run_command, files, the desktop tools
    /// and forward_port (TCP only) still work. For testing offline behaviour or untrusted software.
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
    /// Destination in the VM; relative paths are under the agent user's home (e.g. "Downloads/");
    /// on Arch, /tmp is cleared at every boot.
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

#[derive(Deserialize, JsonSchema, Clone, Copy)]
#[serde(rename_all = "lowercase")]
#[schemars(inline)]
enum ProtocolArg {
    Tcp,
    Udp,
}

impl From<ProtocolArg> for ops::Protocol {
    fn from(p: ProtocolArg) -> Self {
        match p {
            ProtocolArg::Tcp => ops::Protocol::Tcp,
            ProtocolArg::Udp => ops::Protocol::Udp,
        }
    }
}

#[derive(Deserialize, JsonSchema)]
struct ForwardArgs {
    name: String,
    /// Port a server listens on inside the VM.
    guest_port: u16,
    /// Port on 127.0.0.1 of this Mac; a free one is picked if omitted.
    host_port: Option<u16>,
    /// "tcp" (default) or "udp". A UDP forward reaches a server listening on the guest's
    /// 0.0.0.0 (its 10.0.2.15 address), not on its 127.0.0.1; not available on offline VMs.
    protocol: Option<ProtocolArg>,
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

#[derive(Deserialize, JsonSchema)]
struct ScreenshotArgs {
    name: String,
    /// Also write the PNG to this path on this Mac (absolute, or relative to the server's
    /// working directory).
    save_to: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct RemoveForwardArgs {
    name: String,
    /// The 127.0.0.1 host port to stop forwarding.
    host_port: u16,
    /// "tcp" or "udp"; omitted, the forwards of both protocols on that port are removed.
    protocol: Option<ProtocolArg>,
}

#[derive(Deserialize, JsonSchema)]
struct JobArgs {
    name: String,
    /// Job id returned by a `background: true` run_command.
    id: u64,
    /// Lines of the log to return from the end (default 50).
    #[serde(default = "default_tail")]
    tail_lines: usize,
}

fn default_tail() -> usize {
    50
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(inline)]
enum LogKind {
    Qemu,
    Serial,
}

#[derive(Deserialize, JsonSchema)]
struct LogArgs {
    name: String,
    /// Which log: the hypervisor's `qemu` log or the guest's `serial` console.
    which: LogKind,
    /// Lines to return from the end (default 100).
    #[serde(default = "default_log_tail")]
    tail_lines: usize,
}

fn default_log_tail() -> usize {
    100
}

#[tool_router]
impl Gateway {
    #[tool(
        title = "List VMs",
        description = "List VM instances (name, image, state, size, checkpoints, owner, viewer URL and, for\n\
                          running x86apps VMs, x86_tso: hardware|emulated) and which images exist.\n\
                          Each VM shows its owner (owner_running: false once that session is gone); only\n\
                          reset/delete/restore a VM you created, unless the user asks otherwise.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_vms(&self) -> CallToolResult {
        // The note goes in its own block, so the JSON stays parseable.
        let note = self.stale_note();
        reply(blocking(ops::list_json).await.map(|json| {
            note.into_iter()
                .chain([json])
                .map(ContentBlock::text)
                .collect()
        }))
    }

    #[tool(
        title = "Create VM",
        description = "Create and boot a new instance cloned from its image, and return once its desktop and\n\
                          control server are ready. Usually seconds; the FIRST create of an image can take\n\
                          minutes while it is downloaded or built. Give it your own unique name.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = true
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
            OsArg::Arch => Os::Arch,
        };
        let owner = self.owner_tag(&ctx);
        let image = match a.version {
            Some(v) => format!("{os}-{v}"),
            None => os.to_string(),
        };
        // Idempotent retry: a create re-sent after a client timeout finds its own VM already
        // there (of the image asked for) and returns it, booted, rather than erroring or
        // making a second one. The same goes for a VM whose owning server has exited (a
        // session restarted): its creator adopts it by asking for it again.
        if let Some(name) = &a.name
            && let Ok(inst) = load(name)
            && (ops::owner(&inst).as_deref() == Some(owner.as_str())
                || ops::owner_alive(&inst) == Some(false))
            && Image::resolve(&image).is_ok_and(|i| i == inst.image)
        {
            self.claim(name, &ctx);
            if inst.running() {
                return text(Ok(ops::info(&inst)));
            }
            return text(with_progress(&ctx, move || ops::boot(&inst)).await);
        }
        let requested = a.name.clone();
        let tag = owner.clone();
        let res = with_progress(&ctx, move || {
            ops::create(
                &Image::resolve(&image)?,
                a.name.as_deref(),
                a.memory_gb,
                a.cpus,
                a.offline,
                Some(&tag),
            )
        })
        .await;
        // Track it for auto-stop (ops::create recorded the owner before booting), also when
        // it was made but didn't become ready. Without a requested name, `info` starts with
        // the generated one.
        let made = match &res {
            Ok(info) => requested
                .as_deref()
                .or_else(|| info.split_whitespace().next())
                .map(str::to_string),
            Err(_) => requested.filter(|n| {
                load(n).is_ok_and(|i| ops::owner(&i).as_deref() == Some(owner.as_str()))
            }),
        };
        if let Some(name) = made {
            self.owned.lock().unwrap().insert(name);
        }
        let note = self.stale_note().unwrap_or_default();
        text(res.map(|s| note + &s))
    }

    #[tool(
        title = "Start VM",
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
        let r = self.lifecycle(&a.name, ops::boot, &ctx).await;
        if r.is_ok() {
            self.claim(&a.name, &ctx);
        }
        text(r)
    }

    #[tool(
        title = "Stop VM",
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
        title = "Reset VM",
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
        let r = self.lifecycle(&a.name, ops::reset, &ctx).await;
        if r.is_ok() {
            self.claim(&a.name, &ctx);
        }
        text(r)
    }

    #[tool(
        title = "Delete VM",
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
        title = "Checkpoint VM",
        description = "Save the instance's disk and memory under a label (replacing an older one of that\n\
                          name). A running VM pauses for a few seconds and carries on. Use before a risky\n\
                          step; restore_vm returns to it in seconds.",
        annotations(
            read_only_hint = false,
            // Overwrites any checkpoint already stored under this label.
            destructive_hint = true,
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
        title = "Restore VM",
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
        let name = a.name.clone();
        let r = async {
            let inst = load(&a.name)?;
            with_progress(&ctx, move || ops::restore(&inst, &a.label)).await
        }
        .await;
        if r.is_ok() {
            self.claim(&name, &ctx);
        }
        text(r)
    }

    #[tool(
        title = "Take screenshot",
        description = "Screenshot the instance's display from the hypervisor. Works at any time, even while\n\
                          booting or when the desktop server is unresponsive.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn take_screenshot(&self, Parameters(a): Parameters<ScreenshotArgs>) -> CallToolResult {
        let png = async {
            let inst = load(&a.name)?;
            let save_to = a.save_to.as_deref().map(std::path::absolute).transpose()?;
            blocking(move || {
                let png = qemu::screenshot(&inst, &inst.dir.join("screen.png"))?;
                if let Some(dst) = save_to {
                    std::fs::write(&dst, &png)
                        .with_context(|| format!("write {}", dst.display()))?;
                }
                Ok(png)
            })
            .await
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
        title = "Run command",
        description = "Run a shell command in the instance over SSH: PowerShell on windows, bash on ubuntu and arch.\n\
                          Returns the exit code, stdout and stderr; each stream is trimmed to its first and\n\
                          last 10,000 characters (write big output to a file and download_file it).\n\
                          Foreground runs are killed at `timeout` seconds (default 120) with their partial\n\
                          output returned; for servers or anything slow, pass background: true -- it keeps\n\
                          running after the call, returns a job id, and you poll it with get_job_status. A GUI\n\
                          installer run in the foreground should be waited on (e.g. PowerShell\n\
                          `Start-Process -Wait -PassThru`) or it returns before the install finishes.",
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
        text(exec(&a.name, &a.command, a.timeout, true).await)
    }

    #[tool(
        title = "Upload file",
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
        title = "Download file",
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
        title = "Forward port",
        description = "Make a server running inside a VM reachable from this Mac: forwards a port on\n\
                          127.0.0.1 to the guest port until the VM stops. Returns the host address. TCP (default)\n\
                          reaches servers on the guest's own 127.0.0.1. protocol \"udp\" (game, DNS, QUIC, relay\n\
                          servers) needs the guest server listening on 0.0.0.0 and doesn't work on offline VMs.\n\
                          Other VMs reach a forward at 10.0.2.2:<host_port>.",
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
                let protocol = a.protocol.map_or(ops::Protocol::Tcp, Into::into);
                blocking(move || ops::forward(&inst, a.guest_port, a.host_port, protocol)).await
            }
            .await,
        )
    }

    #[tool(
        title = "List desktop tools",
        description = "List the desktop-control tools available in an instance (name + summary), or the full\n\
                          input schema of one tool when `tool` is given. Call them with `use_desktop_tool`. A wrong tool\n\
                          name returns close matches.",
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
                        let read_only = t
                            .annotations
                            .as_ref()
                            .and_then(|a| a.read_only_hint)
                            .unwrap_or(false);
                        format!(
                            "{}({}){}: {first}",
                            t.name,
                            signature(&t.input_schema),
                            if read_only { " [read-only]" } else { "" }
                        )
                    })
                    .collect();
                return Ok(format!(
                    "Required arguments in (), \"…\" = optional ones; pass `tool` for a full schema.\n{}",
                    lines.join("\n")
                ));
            };
            let Some(t) = tools.iter().find(|t| t.name == want) else {
                let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
                let near = similar(&want, &names);
                bail!(
                    "no tool '{want}' in {}{}",
                    a.name,
                    if near.is_empty() {
                        String::new()
                    } else {
                        format!(" (did you mean {}?)", near.join(", "))
                    }
                );
            };
            Ok(serde_json::to_string_pretty(
                &json!({"name": t.name, "description": t.description, "input_schema": t.input_schema}),
            )?)
        };
        text(r.await)
    }

    #[tool(
        title = "Use desktop tool",
        description = "Call a desktop-control tool inside an instance (see list_desktop_tools for names and schemas).\n\
                          Screenshots and other content are returned as-is. A wrong tool name returns close\n\
                          matches; wrong arguments return the tool's argument list.",
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
        let failure = match r {
            Ok(res) if res.is_error == Some(true) => {
                let msg: Vec<&str> = res
                    .content
                    .iter()
                    .filter_map(|c| c.as_text().map(|t| t.text.as_str()))
                    .collect();
                let msg = msg.join(" ");
                if msg.is_empty() {
                    format!("{} failed", a.tool)
                } else {
                    msg
                }
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
                return self.noted(&a.name, out);
            }
            Err(e) => format!("{e:#}"),
        };
        // A wrong guess at a tool's name or arguments is the usual failure: answer it with the
        // right names or the arguments, so the next call can be right without another lookup.
        // (cua-driver reports an unknown name as a permission error, so check names here.)
        let tools = self
            .with_session(&a.name, "list_tools", |c: Client| async move {
                c.list_all_tools().await
            })
            .await
            .ok();
        let hint = match tools
            .as_deref()
            .map(|ts| (ts, ts.iter().find(|t| t.name == a.tool)))
        {
            Some((ts, None)) => {
                let names: Vec<&str> = ts.iter().map(|t| t.name.as_ref()).collect();
                let near = similar(&a.tool, &names);
                format!(
                    "\n\nThere is no desktop tool named \"{}\" in {}{}; list_desktop_tools lists them.",
                    a.tool,
                    a.name,
                    if near.is_empty() {
                        String::new()
                    } else {
                        format!(" (did you mean {}?)", near.join(", "))
                    }
                )
            }
            Some((_, Some(t))) if is_argument_error(&failure) => format!(
                "\n\n{} arguments (* = required): {}\nFull schema: list_desktop_tools with tool=\"{}\".",
                a.tool,
                arguments(&t.input_schema),
                a.tool
            ),
            _ => String::new(),
        };
        self.noted(
            &a.name,
            CallToolResult::error(vec![ContentBlock::text(failure + &hint)]),
        )
    }

    #[tool(
        name = "get_job_status",
        title = "Get job status",
        description = "Check on a background job started by run_command (background: true), by the id it\n\
                          returned: whether it is still running or has exited (with its code), plus the tail\n\
                          of its log.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn job_status(&self, Parameters(a): Parameters<JobArgs>) -> CallToolResult {
        text(job_status(&a.name, a.id, a.tail_lines).await)
    }

    #[tool(
        title = "Delete checkpoint",
        description = "Delete a checkpoint by label, freeing its disk and memory snapshot. The VM is not\n\
                          affected.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn delete_checkpoint(&self, Parameters(a): Parameters<CheckpointArgs>) -> CallToolResult {
        text(
            async {
                let inst = load(&a.name)?;
                blocking(move || ops::delete_checkpoint(&inst, &a.label)).await
            }
            .await,
        )
    }

    #[tool(
        title = "List port forwards",
        description = "List the active port forwards for a VM: each one's protocol (tcp/udp), host port on\n\
                          127.0.0.1, the guest port it reaches, and whether it is still alive.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_forwards(&self, Parameters(a): Parameters<NameArgs>) -> CallToolResult {
        text(
            async {
                let inst = load(&a.name)?;
                blocking(move || Ok(ops::forwards_text(&inst))).await
            }
            .await,
        )
    }

    #[tool(
        name = "delete_forward",
        title = "Delete port forward",
        description = "Stop forwarding a host port set up by forward_port (only the given protocol's forward\n\
                          if protocol is set); other forwards keep running.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn delete_forward(&self, Parameters(a): Parameters<RemoveForwardArgs>) -> CallToolResult {
        text(
            async {
                let inst = load(&a.name)?;
                let protocol = a.protocol.map(Into::into);
                blocking(move || ops::remove_forward(&inst, a.host_port, protocol)).await
            }
            .await,
        )
    }

    #[tool(
        title = "Read VM log",
        description = "Read the tail of a VM's log: the hypervisor's `qemu` log (boot/device errors) or the\n\
                          guest's `serial` console. Useful when a VM won't boot or the desktop is unreachable.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn read_vm_log(&self, Parameters(a): Parameters<LogArgs>) -> CallToolResult {
        let which = match a.which {
            LogKind::Qemu => "qemu",
            LogKind::Serial => "serial",
        };
        text(
            async {
                let inst = load(&a.name)?;
                blocking(move || ops::read_log(&inst, which, a.tail_lines)).await
            }
            .await,
        )
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

    /// Puts a note in front of a desktop call's result when its connection to the VM's driver
    /// was replaced since the last call, since references from before no longer resolve.
    fn noted(&self, name: &str, mut r: CallToolResult) -> CallToolResult {
        if self.reconnected.lock().unwrap().remove(name) {
            r.content.insert(
                0,
                ContentBlock::text(format!(
                    "Note: agentpc reconnected to the desktop driver in {name}, so driver sessions, \
                     browser bindings and snapshot ids from earlier calls are gone: take a new \
                     snapshot, and run browser_prepare again for browser work."
                )),
            );
        }
        r
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
        if !self.connected.lock().unwrap().insert(name.to_string()) {
            self.reconnected.lock().unwrap().insert(name.to_string());
        }
        Ok(c)
    }

    /// Runs `f` on the instance's session. A transport failure usually means the guest
    /// restarted underneath us, so reconnect once and retry.
    async fn with_session<T, F>(&self, name: &str, what: &str, f: impl Fn(Client) -> F) -> Result<T>
    where
        F: Future<Output = Result<T, ServiceError>>,
    {
        for attempt in 1..=2 {
            let client = self.session(name).await?;
            // A hung desktop tool must not wedge the call forever; treat a stall like a
            // transport failure so we reconnect once, then give up.
            let call = tokio::time::timeout(DESKTOP_CALL_TIMEOUT, f(client)).await;
            match call {
                Ok(Ok(v)) => return Ok(v),
                Ok(Err(ServiceError::McpError(e))) => {
                    bail!("{what} failed in {name}: {}", e.message)
                }
                Ok(Err(e)) => {
                    self.drop_session(name);
                    if attempt == 2 {
                        bail!("{what} failed in {name}: {e}");
                    }
                }
                Err(_) => {
                    self.drop_session(name);
                    if attempt == 2 {
                        bail!(
                            "{what} timed out after {}s in {name}",
                            DESKTOP_CALL_TIMEOUT.as_secs()
                        );
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
            // The `serve` daemon runs in the logged-in desktop session; SSH is Session 0,
            // where plain `mcp` refuses to start, so name the daemon's pipe explicitly.
            Os::Windows => {
                windows_cua_driver(&inst).await?;
                let remote = format!(
                    "& \"{}\" mcp --socket \\\\.\\pipe\\cua-driver",
                    ops::WINDOWS_CUA_DRIVER
                );
                let cmd = tokio::process::Command::new("ssh").configure(|c| {
                    c.args(ops::ssh_args(&inst, &remote));
                });
                Ok(().serve(TokioChildProcess::new(cmd)?).await?)
            }
            Os::Ubuntu | Os::Arch => {
                let remote = format!("{} ~/.local/bin/cua-driver mcp", ops::LINUX_SESSION_ENV);
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

/// Makes sure the VM's cua-driver daemon is running, starting it if needed (its logon task
/// doesn't restart it after a crash or kill).
async fn windows_cua_driver(inst: &Instance) -> Result<()> {
    let script = format!(
        r#"$c = "{}"
if (-not (Test-Path $c)) {{ exit 3 }}
function up {{ (& $c status 2>&1 | Out-String) -match 'daemon is running' }}
if (-not (up)) {{ & $c autostart kick *> $null; foreach ($i in 1..15) {{ Start-Sleep 1; if (up) {{ break }} }} }}
exit 0"#,
        ops::WINDOWS_CUA_DRIVER
    );
    let out = tokio::process::Command::new("ssh")
        .args(ops::ssh_args(inst, &script))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .await
        .context("run ssh")?;
    match out.status.code() {
        Some(0) => Ok(()),
        Some(3) => bail!("{}", ops::old_windows_image(inst)),
        _ => bail!(
            "ssh to {} failed: {}",
            inst.name,
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

/// Characters kept from each end of a long stdout or stderr.
const OUTPUT_KEEP: usize = 10_000;

/// A single desktop-control tool call may take a while (typing, waits) but must not hang forever.
const DESKTOP_CALL_TIMEOUT: Duration = Duration::from_secs(120);

async fn exec(name: &str, command: &str, timeout: u64, desktop_env: bool) -> Result<String> {
    let inst = load(name)?;
    if !inst.running() {
        bail!("VM {name} is not running; start_vm first");
    }
    // A foreground bash command needs the logged-in desktop session's env to reach the display
    // (xdotool, GUI apps); the background path sets it itself, so it opts out.
    let command = match (inst.os.is_linux(), desktop_env) {
        (true, true) => format!("export {}; {command}", ops::LINUX_SESSION_ENV),
        (true, false) => command.to_string(),
        (false, _) => ops::windows_command(command),
    };
    let mut child = tokio::process::Command::new("ssh")
        .args(ops::ssh_args(&inst, &command))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("run ssh")?;
    // Drain both pipes concurrently so a timeout kill still yields whatever the command printed.
    let so = child.stdout.take().unwrap();
    let se = child.stderr.take().unwrap();
    let read_out = tokio::spawn(drain_ends(so));
    let read_err = tokio::spawn(drain_ends(se));
    let status = tokio::time::timeout(Duration::from_secs(timeout), child.wait()).await;
    let timed_out = status.is_err();
    if timed_out {
        let _ = child.kill().await;
    }
    let stdout = read_out.await.unwrap_or_default();
    let stderr = read_err.await.unwrap_or_default();
    // ssh exits 255 when it can't connect; the command's own code otherwise.
    let code = match &status {
        Ok(Ok(s)) => s.code().unwrap_or(-1),
        _ => -1,
    };
    let head = if timed_out {
        format!(
            "timed out after {timeout}s. The SSH session was closed, but the command may still \
             be running in the VM (find it with ps or Get-Process). Use background: true for \
             long jobs.\n(partial output below)"
        )
    } else {
        format!("exit code: {code}")
    };
    let mut text = head;
    for (label, out) in [("stdout", &stdout), ("stderr", &stderr)] {
        let s = out.text();
        if !s.is_empty() {
            text += &format!("\n--- {label} ---\n{s}");
        }
    }
    if timed_out || code != 0 {
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
        bail!("VM {name} is not running; start_vm first");
    }
    let id = job_id();
    let b64 = base64::engine::general_purpose::STANDARD.encode(command);
    let script = match inst.os {
        // Separate lines: `a && b &` would background the whole list, and that shell would
        // hold the SSH session open. The wrapper records the exit code in <id>.exit so
        // get_job_status can report it after the job ends.
        Os::Ubuntu | Os::Arch => format!(
            "d=~/agentpc-bg; mkdir -p $d && echo {b64} | base64 -d > $d/{id}.sh || exit 1\n\
             {env} setsid nohup bash -c 'bash \"$0\"; echo $? > \"$1\"' \
             $d/{id}.sh $d/{id}.exit > $d/{id}.log 2>&1 < /dev/null &\n\
             echo \"started in the background (id {id}, pid $!). \
             Poll it with get_job_status name={name} id={id}. \
             Output: $HOME/agentpc-bg/{id}.log. Stop it with: kill $!\"",
            env = ops::LINUX_SESSION_ENV
        ),
        Os::Windows => format!(
            r#"$d = "$env:USERPROFILE\agentpc-bg"; New-Item -ItemType Directory -Force $d | Out-Null
$ps = "$d\{id}.ps1"; $log = "$d\{id}.log"; $exit = "$d\{id}.exit"; $task = 'agentpc-bg-{id}'
# A BOM, or Windows PowerShell 5.1 reads the script as ANSI.
[IO.File]::WriteAllText($ps, [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{b64}')), (New-Object Text.UTF8Encoding $true))
$inner = "& '$ps' *>&1 | Out-File -Encoding utf8 '$log'; `$c = `$LASTEXITCODE; if (`$null -eq `$c) {{ `$c = 0 }}; Set-Content -Encoding utf8 '$exit' `$c"
$a = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -Command `"$inner`""
$p = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Highest
$s = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
Register-ScheduledTask -TaskName $task -Action $a -Principal $p -Settings $s -Force | Out-Null
Start-ScheduledTask -TaskName $task
"started in the background as scheduled task $task (id {id}). Poll it with get_job_status name={name} id={id}. Output: $log. Stop it with: Stop-ScheduledTask $task (and Stop-Process for anything it started)""#
        ),
    };
    let out = exec(name, &script, 60, false).await?;
    Ok(out
        .split_once("--- stdout ---\n")
        .map_or(out.clone(), |(_, s)| s.trim().to_string()))
}

/// Report a background job's state from its log and exit-code file (see exec_background):
/// running while no <id>.exit exists yet, otherwise exited with that code, plus a log tail.
async fn job_status(name: &str, id: u64, tail_lines: usize) -> Result<String> {
    let inst = load(name)?;
    if !inst.running() {
        bail!("VM {name} is not running; start_vm first");
    }
    let script = match inst.os {
        Os::Ubuntu | Os::Arch => format!(
            "d=~/agentpc-bg\n\
             if [ ! -f $d/{id}.log ]; then echo 'STATE: no such job'; exit 0; fi\n\
             if [ -f $d/{id}.exit ]; then echo \"STATE: exited $(cat $d/{id}.exit)\"; \
             else echo 'STATE: running'; fi\n\
             echo '--- log tail ---'; tail -n {tail_lines} $d/{id}.log",
        ),
        Os::Windows => format!(
            r#"$d = "$env:USERPROFILE\agentpc-bg"; $log = "$d\{id}.log"; $exit = "$d\{id}.exit"
if (-not (Test-Path $log)) {{ 'STATE: no such job'; exit 0 }}
if (Test-Path $exit) {{ "STATE: exited $((Get-Content $exit -Raw).Trim())" }} else {{ 'STATE: running' }}
'--- log tail ---'; if (Test-Path $log) {{ Get-Content $log -Tail {tail_lines} }}"#
        ),
    };
    let out = exec(name, &script, 30, false).await?;
    Ok(out
        .split_once("--- stdout ---\n")
        .map_or_else(|| out.clone(), |(_, s)| s.trim().to_string()))
}

/// Keep the first and last `keep` characters of `s`, noting how much was cut.
/// A background job's id: the time in milliseconds, but never the same twice in this
/// process, since two jobs started in one millisecond would share their files.
fn job_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let prev = LAST
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |last| {
            Some(now.max(last + 1))
        })
        .unwrap_or(0);
    now.max(prev + 1)
}

/// What a command printed on one stream: only its first and last bytes are kept while
/// reading, so a command that prints gigabytes can't exhaust this server's memory.
#[derive(Default)]
struct Captured {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    total: usize,
}

/// Bytes kept from each end of a stream: enough for OUTPUT_KEEP characters of any UTF-8.
const CAPTURE_KEEP: usize = 4 * OUTPUT_KEEP;

async fn drain_ends<R: tokio::io::AsyncRead + Unpin>(mut r: R) -> Captured {
    use tokio::io::AsyncReadExt;
    let mut c = Captured::default();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        c.push(&buf[..n]);
    }
    c
}

impl Captured {
    fn push(&mut self, mut chunk: &[u8]) {
        self.total += chunk.len();
        if self.head.len() < CAPTURE_KEEP {
            let take = (CAPTURE_KEEP - self.head.len()).min(chunk.len());
            self.head.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
        }
        self.tail.extend(chunk);
        let over = self.tail.len().saturating_sub(CAPTURE_KEEP);
        self.tail.drain(..over);
    }

    /// The output for a reply: whole if it is short, else its two ends.
    fn text(&self) -> String {
        let tail: Vec<u8> = self.tail.iter().copied().collect();
        if self.total == self.head.len() + tail.len() {
            let mut all = self.head.clone();
            all.extend_from_slice(&tail);
            return clip(String::from_utf8_lossy(&all).trim_end(), OUTPUT_KEEP);
        }
        let head = String::from_utf8_lossy(&self.head);
        let tail = String::from_utf8_lossy(&tail);
        let head: String = head.chars().take(OUTPUT_KEEP).collect();
        let n = tail.chars().count();
        let tail: String = tail.chars().skip(n.saturating_sub(OUTPUT_KEEP)).collect();
        format!(
            "{head}\n[... middle omitted: {} bytes of output in all ...]\n{}",
            self.total,
            tail.trim_end()
        )
    }
}

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

/// A desktop tool's required argument names, with "…" when it also takes optional ones:
/// `get_window_state(pid, window_id, …)`.
fn signature(schema: &JsonObject) -> String {
    let required = required(schema);
    let optional = schema
        .get("properties")
        .and_then(|p| p.as_object())
        .is_some_and(|p| p.keys().any(|k| !required.contains(&k.as_str())));
    let mut parts: Vec<&str> = required;
    if optional {
        parts.push("…");
    }
    parts.join(", ")
}

/// Every argument of a desktop tool, one short entry each: `text* (string); scope
/// (window|desktop)`. `*` marks required ones. Descriptions are left to the full schema.
fn arguments(schema: &JsonObject) -> String {
    let required = required(schema);
    let Some(props) = schema.get("properties").and_then(|p| p.as_object()) else {
        return "none".into();
    };
    let mut entries: Vec<(bool, String)> = props
        .iter()
        .map(|(name, p)| {
            let kind = match p.get("enum").and_then(|e| e.as_array()) {
                Some(values) => values
                    .iter()
                    .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string))
                    .collect::<Vec<_>>()
                    .join("|"),
                None => match p.get("type") {
                    Some(serde_json::Value::String(t)) => t.clone(),
                    Some(serde_json::Value::Array(ts)) => ts
                        .iter()
                        .filter_map(|t| t.as_str())
                        .filter(|t| *t != "null")
                        .collect::<Vec<_>>()
                        .join("|"),
                    _ => "object".into(),
                },
            };
            let req = required.contains(&name.as_str());
            (
                !req,
                format!("{name}{} ({kind})", if req { "*" } else { "" }),
            )
        })
        .collect();
    // Required first, then the rest in the schema's order.
    entries.sort_by_key(|(optional, _)| *optional);
    let entries: Vec<String> = entries.into_iter().map(|(_, e)| e).collect();
    entries.join("; ")
}

fn required(schema: &JsonObject) -> Vec<&str> {
    schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|r| r.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default()
}

/// Up to three tool names close to a mistyped one: same letters ignoring `_`/case, one
/// containing the other, or a small edit distance.
fn similar<'a>(want: &str, names: &[&'a str]) -> Vec<&'a str> {
    let norm = |s: &str| s.to_lowercase().replace(['_', '-'], "");
    let w = norm(want);
    let mut scored: Vec<(usize, &str)> = names
        .iter()
        .filter_map(|n| {
            let m = norm(n);
            let d = if m == w || m.contains(&w) || w.contains(&m) {
                0
            } else {
                edit_distance(&m, &w)
            };
            (d <= 2.max(w.len() / 4)).then_some((d, *n))
        })
        .collect();
    scored.sort();
    scored.into_iter().take(3).map(|(_, n)| n).collect()
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != *cb)).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

/// Whether a desktop tool failed because of the arguments it was given, as opposed to what
/// happened on screen.
fn is_argument_error(msg: &str) -> bool {
    let msg = msg.to_lowercase();
    [
        "missing required",
        "required parameter",
        "required property",
        "invalid param",
        "invalid argument",
        "invalid type",
        "invalid value",
        "unknown field",
        "unknown property",
        "unknown variant",
        "additional propert",
        "unknown tool",
        "tool not found",
        "no such tool",
    ]
    .iter()
    .any(|p| msg.contains(p))
}

/// Identifies the binary at `path` (its inode), which an update replaces.
fn exe_id(path: &std::path::Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| m.ino())
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
    fn captures_short_output_whole() {
        let mut c = super::Captured::default();
        c.push(b"hello ");
        c.push(b"world\n");
        assert_eq!(c.text(), "hello world");
    }

    #[test]
    fn captures_only_the_ends_of_huge_output() {
        let mut c = super::Captured::default();
        c.push(b"START");
        for _ in 0..1000 {
            c.push(&[b'x'; 4096]);
        }
        c.push(b"END");
        // Bounded memory, whatever was printed.
        assert!(c.head.len() <= super::CAPTURE_KEEP && c.tail.len() <= super::CAPTURE_KEEP);
        let t = c.text();
        assert!(t.starts_with("START"), "{}", &t[..20]);
        assert!(t.ends_with("END"));
        assert!(t.contains(&format!("{} bytes of output in all", 5 + 1000 * 4096 + 3)));
    }

    #[test]
    fn job_ids_never_repeat() {
        let ids: Vec<u64> = (0..1000).map(|_| super::job_id()).collect();
        assert!(ids.windows(2).all(|w| w[1] > w[0]));
    }

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

    fn schema(v: serde_json::Value) -> rmcp::model::JsonObject {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn summarizes_desktop_tool_arguments() {
        let s = schema(serde_json::json!({
            "type": "object",
            "required": ["text"],
            "properties": {
                "pid": {"type": "integer"},
                "text": {"type": "string"},
                "delivery_mode": {"enum": ["background", "foreground"], "type": "string"},
                "target": {"anyOf": [{"type": "object"}, {"type": "null"}]},
                "window_id": {"type": ["integer", "null"]}
            }
        }));
        assert_eq!(super::signature(&s), "text, …");
        assert_eq!(
            super::arguments(&s),
            "text* (string); pid (integer); delivery_mode (background|foreground); \
             target (object); window_id (integer)"
        );
        let none = schema(serde_json::json!({"type": "object", "properties": {}}));
        assert_eq!(super::signature(&none), "");
        assert_eq!(
            super::signature(&schema(serde_json::json!({
                "required": ["pid", "window_id"],
                "properties": {"pid": {}, "window_id": {}}
            }))),
            "pid, window_id"
        );
    }

    #[test]
    fn spots_argument_errors() {
        // Messages seen from cua-driver and serde.
        assert!(super::is_argument_error(
            "Missing required parameter: window_id"
        ));
        assert!(super::is_argument_error(
            "invalid type: string \"x\", expected integer"
        ));
        assert!(super::is_argument_error(
            "unknown field `foo`, expected one of `pid`"
        ));
        assert!(!super::is_argument_error(
            "The latest snapshot for this window does not contain a screenshot owned by this session."
        ));
        assert!(!super::is_argument_error("no elements found"));
        assert!(super::is_argument_error(
            "Missing required string field: text"
        ));
    }

    #[test]
    fn suggests_close_tool_names() {
        let names = [
            "type_text",
            "get_window_state",
            "get_desktop_state",
            "click",
            "list_windows",
        ];
        assert_eq!(super::similar("typetext", &names), ["type_text"]);
        assert_eq!(
            super::similar("get_windows_state", &names)[0],
            "get_window_state"
        );
        assert_eq!(super::similar("clik", &names), ["click"]);
        assert!(super::similar("screenshot", &names).is_empty());
    }
}

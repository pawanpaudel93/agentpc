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

use crate::execution::{self, Cancel, Captured, Probe, RecoveryAction, Reply, drain_ends};
#[cfg(test)]
use crate::execution::{CAPTURE_KEEP, clip};
use crate::instance::{Image, Instance, Os};
use crate::{jobs, ops, qemu};

const INSTRUCTIONS: &str = "\
Controls instant, resettable Windows, Ubuntu and Arch Linux ARM desktop VMs on this Mac. Treat
VMs as throwaway sandboxes. If list_vms shows an OS or image that create_vm's options don't
list, your tool definitions predate an agentpc update: reconnect the agentpc MCP server.

Flow: list_vms -> create_vm (or start_vm on one you created) -> take_screenshot -> list_desktop_tools ->
use_desktop_tool(...) (inspect_desktop(...) to only look) -> take_screenshot to verify. reset_vm returns an instance to a clean
install; checkpoint_vm/restore_vm save and return to any point in seconds (disk and memory) --
checkpoint before a risky or slow-to-redo step. run_command runs PowerShell on Windows and bash
on Ubuntu and Arch; the guest login is agent/agent (SSH
takes only agentpc's key). create_vm takes an optional version (Ubuntu
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
inspect_desktop takes the same arguments but is read-only: it runs a tool only if the VM's driver
explicitly marks it read-only (list_desktop_tools shows [read-only]) and refuses anything else before
dispatching it (looking up the marks does ask the VM's driver); the mark is the driver's own claim, a
guard against mistakes. For a compact look,
pass include_screenshot: false (and optionally query) to get_window_state: the element tree, no image.
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
  and returns a job id); poll it with get_job_status and end it with stop_job. At a foreground
  run_command's timeout (default 120 s) agentpc attempts to stop it and keeps its partial output.
- run_command results: read structuredContent (state, exit_code, stdout, stderr, timed_out,
  termination_confirmed, effect_unknown, execution_id, job_id, recovery_action) and isError; the text is
  the readable form, not JSON. Every nonzero exit, timeout, failed start or lost connection is an error.
  termination_confirmed: true means agentpc verified none of the processes it identified is left: on
  Linux a systemd user scope when the guest has a user manager, else the marked processes and their
  descendants, plus the verified root; on Windows the verified process tree, best effort. It is not a
  sandbox guarantee: system services, other systemd units and Windows services, tasks and WMI-created
  processes are not covered. A command whose exit code the VM recorded but whose run had not ended by the
  timeout returns that exit code with timed_out true once that cleanup is verified. If
  termination_confirmed is false, or the state is connection_lost with effect_unknown true, the command
  may still be running: use job_id with get_job_status / stop_job and don't rerun it before you know.
  connection_lost with effect_unknown false means the VM recorded its exit_code (it finished; output may
  be incomplete) and job_id is null. Commands are never retried for you; only
  recovery_action \"retry\" means it provably never ran. After a timeout the reply can take up to about 50
  s longer while agentpc finishes and verifies its cleanup. A Linux foreground command is limited to
  92 KiB (94,208 UTF-8 bytes) of text, checked before anything is sent: upload_file a bigger script and
  run it.
- Watching a job: get_job_status {name, id} answers at once with the log tail and a next_cursor.
  wait_seconds (0 to 30, default 0) waits for the job to leave running (with a cursor: for new output)
  and never stops it; wait_timed_out says the wait ran out. Pass cursor (a next_cursor, or \"start\") to
  read new output only: at most 16 KiB a reply, advancing by exactly what it returned, UTF-8 safe;
  has_more says call again. If cursor_reset is true (VM reset, reboot, log replaced or rewritten;
  detection is best effort) discard what you assumed: output restarts at the log's beginning. missing
  means nothing has recorded itself under that id yet, not that it never started: don't blindly retry.
- Reach a server in the VM from the Mac with forward_port (works even for servers bound to the
  guest's own 127.0.0.1); it returns a 127.0.0.1:<port> address and lasts until the VM stops.
  For a UDP server pass protocol: \"udp\"; it must listen on 0.0.0.0 (on Windows, also allow
  the port in its firewall: New-NetFirewallRule -Direction Inbound -Protocol UDP -LocalPort <port>
  -Action Allow). list_forwards /
  delete_forward manage them. From inside the guest, 10.0.2.2 reaches this Mac.
- Don't start a Windows image build yourself -- if no Windows image exists, ask the user to
  build one (~12 min).
- Output from run_command is trimmed to the first and last 10,000 characters per stream
  (output_truncated says so); write big output to a file in the VM and download_file it.
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
    /// Per VM, the desktop tools its driver marks read-only: only those are re-sent after a
    /// dropped connection.
    read_only: Arc<Mutex<HashMap<String, HashSet<String>>>>,
    /// Test seam: drivers `session` hands out, in order, instead of connecting over SSH.
    #[cfg(test)]
    dials: Arc<Mutex<std::collections::VecDeque<RunningService<RoleClient, ()>>>>,
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
            read_only: Default::default(),
            #[cfg(test)]
            dials: Default::default(),
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

    /// Whether the VM's desktop driver marks `tool` read-only (asked once per VM). Unknown
    /// counts as not: a repeated click or keystroke is worse than a reported failure.
    async fn read_only_tool(&self, name: &str, tool: &str) -> bool {
        if let Some(set) = self.read_only.lock().unwrap().get(name) {
            return set.contains(tool);
        }
        let Ok(tools) = self
            .with_session(name, "list_tools", true, |c: Client| async move {
                c.list_all_tools().await
            })
            .await
        else {
            return false;
        };
        let set: HashSet<String> = tools
            .iter()
            .filter(|t| t.annotations.as_ref().and_then(|a| a.read_only_hint) == Some(true))
            .map(|t| t.name.to_string())
            .collect();
        let found = set.contains(tool);
        self.read_only.lock().unwrap().insert(name.to_string(), set);
        found
    }

    /// Make this session a VM's owner: its tag (with this process, for `owner_running`) and
    /// the auto-stop set agree.
    async fn claim(&self, name: &str, ctx: &RequestContext<RoleServer>) -> Result<()> {
        let inst = load(name)?;
        let owner = self.owner_tag(ctx);
        self.claim_with(name, move || ops::claim(&inst, &owner))
            .await
    }

    async fn claim_with(
        &self,
        name: &str,
        claim: impl FnOnce() -> Result<()> + Send + 'static,
    ) -> Result<()> {
        blocking(claim).await?;
        self.owned.lock().unwrap().insert(name.to_string());
        Ok(())
    }

    /// Best-effort, bounded shutdown: gracefully stop the VMs this session left running, unless
    /// AGENTPC_KEEP_RUNNING=1. Force-quit any that don't stop in time so the process can exit.
    async fn shutdown(&self) {
        if ops::keep_running() {
            return;
        }
        let names: Vec<String> = self.owned.lock().unwrap().iter().cloned().collect();
        // Only VMs still ours: another session may have taken one over since (its owner tag
        // then names that session).
        let mine = format!("[{}]", self.session_id);
        let running: Vec<Instance> = names
            .into_iter()
            .filter_map(|n| load(&n).ok())
            .filter(|i| i.running() && ops::owner(i).is_some_and(|o| o.ends_with(&mine)))
            .collect();
        if running.is_empty() {
            return;
        }
        crate::log!(
            "MCP server exiting; stopping {} VM(s) it started",
            running.len()
        );
        // Bound the lock-holding operation itself. Cancelling a spawn_blocking waiter
        // would leave that stop running (and its lock held) past the session deadline.
        let deadline = std::time::Instant::now() + Duration::from_secs(45);
        let stops = running.into_iter().map(|inst| {
            let mine = mine.clone();
            blocking(move || ops::stop_if_owned(&inst, &mine, deadline))
        });
        for result in futures::future::join_all(stops).await {
            if let Err(error) = result {
                crate::log!("session VM shutdown failed: {error:#}");
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
    /// "x86apps" (or "22.04-x86apps"; x86apps is for 22.04 and 24.04 only) is Ubuntu that also runs
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
    /// Open the ready VM's desktop viewer in the Mac's default browser (not inside the guest).
    /// Default false. Per-call only; a launch failure warns without failing VM creation.
    #[serde(default)]
    open_in_browser: bool,
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
    /// Seconds to wait for the command (default 120); at the timeout agentpc stops it, and what
    /// it started, in the VM. Ignored with `background`.
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
    /// Also write the PNG to this new file on this Mac (absolute, or relative to the server's
    /// working directory); an existing file is not overwritten.
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
    /// Job id returned by run_command: a `background: true` job's, or the job_id of a
    /// foreground run whose outcome was uncertain.
    id: u64,
    /// Lines of the log to return from the end (default 50); at most 16 KiB come back. Ignored
    /// when `cursor` is given.
    #[serde(default = "default_tail")]
    tail_lines: usize,
    /// Seconds to wait (default 0, maximum 30) for the job to finish or, with a `cursor`, for
    /// new output. The job is never stopped; when the wait runs out the current state comes
    /// back with `wait_timed_out: true`.
    #[serde(default)]
    #[schemars(range(min = 0, max = 30))]
    wait_seconds: u64,
    /// A `next_cursor` from an earlier reply about this job, or "start": returns the log from
    /// that position instead of its tail (at most 16 KiB per reply), with the next cursor.
    cursor: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct StopJobArgs {
    name: String,
    /// Job id returned by run_command: a `background: true` job's, or the job_id of a
    /// foreground run whose outcome was uncertain.
    id: u64,
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
                          An image with `outdated` lacks this agentpc's newer guest fixes: tell the user\n\
                          the command it gives. Each VM shows its owner (owner_running: false once that\n\
                          session is gone); only reset/delete/restore a VM you created, unless the user\n\
                          asks otherwise.",
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
        let open_in_browser = a.open_in_browser;
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
            // Settings asked for again must match: an `offline: true` retry must never come
            // back with an online VM.
            let (mem, cpus) = inst.size();
            let differs = [
                (a.offline != inst.offline()).then(|| format!("offline is {}", inst.offline())),
                a.memory_gb
                    .filter(|&m| m != mem)
                    .map(|_| format!("memory_gb is {mem}")),
                a.cpus
                    .filter(|&c| c != cpus)
                    .map(|_| format!("cpus is {cpus}")),
            ];
            let differs: Vec<String> = differs.into_iter().flatten().collect();
            if !differs.is_empty() {
                return text(Err(anyhow!(
                    "VM {name} already exists with other settings ({}); pick another name, or \
                     delete_vm it first",
                    differs.join(", ")
                )));
            }
            if let Err(error) = self.claim(name, &ctx).await {
                return text(Err(error));
            }
            // Through boot even when QEMU runs: a VM that never became ready is waited for
            // again (and a paused one resumed), rather than reported ready.
            return text(
                with_progress(&ctx, move || {
                    crate::viewer::open_created(ops::boot(&inst), open_in_browser)
                })
                .await,
            );
        }
        let requested = a.name.clone();
        // Tracked before it boots: a client that disconnects mid-create must still have it
        // stopped (shutdown stops only VMs whose owner tag is this session's, which create
        // writes before the first boot).
        if let Some(n) = &requested
            && load(n).is_err()
        {
            self.owned.lock().unwrap().insert(n.clone());
        }
        let tag = owner.clone();
        let res = with_progress(&ctx, move || {
            crate::viewer::open_created(
                ops::create(
                    &Image::resolve(&image)?,
                    a.name.as_deref(),
                    a.memory_gb,
                    a.cpus,
                    a.offline,
                    Some(&tag),
                ),
                open_in_browser,
            )
        })
        .await;
        // Track it for auto-stop (ops::create recorded the owner before booting), also when
        // it was made but didn't become ready. Without a requested name, `info` starts with
        // the generated one.
        let made: Vec<String> = match &res {
            Ok(info) => requested
                .as_deref()
                .or_else(|| info.split_whitespace().next())
                .map(str::to_string)
                .into_iter()
                .collect(),
            // Made but not ready: it carries this session's owner tag (its name may have been
            // generated, so look for the tag).
            Err(_) => Instance::list()
                .unwrap_or_default()
                .into_iter()
                .filter(|i| ops::owner(i).as_deref() == Some(owner.as_str()))
                .map(|i| i.name)
                .collect(),
        };
        self.owned.lock().unwrap().extend(made);
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
        // Claimed before booting, so a VM whose boot fails is still this session's to stop.
        if let Err(error) = self.claim(&a.name, &ctx).await {
            return text(Err(error));
        }
        text(self.lifecycle(&a.name, ops::boot, &ctx).await)
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
        if let Err(error) = self.claim(&a.name, &ctx).await {
            return text(Err(error));
        }
        text(self.lifecycle(&a.name, ops::reset, &ctx).await)
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
        if let Err(error) = self.claim(&a.name, &ctx).await {
            return text(Err(error));
        }
        let r = async {
            let inst = load(&a.name)?;
            with_progress(&ctx, move || ops::restore(&inst, &a.label)).await
        }
        .await;
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
                    // A new file only (create_new also refuses a symlink there): this tool is
                    // read-only, and save_to must not be a way to overwrite files on this Mac.
                    use std::io::Write;
                    std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&dst)
                        .and_then(|mut f| f.write_all(&png))
                        .with_context(|| {
                            format!("write {} (save_to must be a new file)", dst.display())
                        })?;
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
                          At `timeout` seconds (default 120) agentpc attempts to stop a foreground run, with what\n\
                          it started in the VM (best effort on Windows, where a process whose launcher has\n\
                          already exited can be missed), and returns its partial output; if the stop can't be\n\
                          confirmed (or the connection fails before its end is known), the reply says so and gives a job_id for\n\
                          get_job_status / stop_job. Commands are never retried. A Linux foreground command is\n\
                          limited to 92 KiB (94,208 UTF-8 bytes) of text, checked before sending (upload_file a bigger script and run it). For servers\n\
                          or anything slow,\n\
                          pass background: true -- it keeps running after the call, returns a job id, and you\n\
                          poll it with get_job_status and end it with stop_job. Structured content carries the\n\
                          same outcome as typed fields (state, exit_code, timed_out, termination_confirmed,\n\
                          effect_unknown, job_id, ...). A GUI\n\
                          installer run in the foreground should be waited on (e.g. PowerShell\n\
                          `Start-Process -Wait -PassThru`) or it returns before the install finishes.",
        output_schema = execution::output_schema(),
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = true
        )
    )]
    async fn run_command(&self, Parameters(a): Parameters<ExecArgs>) -> CallToolResult {
        if a.background {
            return exec_background(&a.name, &a.command).await.into_result();
        }
        run_tracked(&a.name, &a.command, a.timeout)
            .await
            .into_result()
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
                          servers) needs the guest server listening on 0.0.0.0 (a Windows guest also needs an\n\
                          inbound firewall rule for the port) and doesn't work on offline VMs.\n\
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
                          input schema of one tool when `tool` is given. Act with `use_desktop_tool`; look with `inspect_desktop` ([read-only] ones only). A wrong tool\n\
                          name returns close matches.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_desktop_tools(&self, Parameters(a): Parameters<ToolsArgs>) -> CallToolResult {
        let r = async {
            let tools = self
                .with_session(&a.name, "list_tools", true, |c: Client| async move {
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
            Ok(serde_json::to_string_pretty(&json!({
                "name": t.name,
                "description": t.description,
                "annotations": t.annotations,
                "input_schema": t.input_schema,
            }))?)
        };
        text(r.await)
    }

    #[tool(
        title = "Use desktop tool",
        description = "Call a desktop-control tool inside an instance (see list_desktop_tools for names and schemas).\n\
                          Screenshots and other content are returned as-is. A wrong tool name returns close\n\
                          matches; wrong arguments return the tool's argument list. This is the route for tools that\n\
                          act (click, type, launch); to only look, prefer inspect_desktop.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = true
        )
    )]
    async fn use_desktop_tool(&self, Parameters(a): Parameters<DesktopArgs>) -> CallToolResult {
        self.desktop_call(a, DesktopRoute::Use).await
    }

    #[tool(
        title = "Inspect desktop",
        description = "Read-only twin of use_desktop_tool, with the same arguments (name, tool, arguments): runs a\n\
                          desktop tool only if that VM's desktop driver explicitly marks it read-only (list_desktop_tools\n\
                          shows [read-only]), e.g. get_window_state. Anything else,\n\
                          including a tool with no read-only mark or an unknown name, is refused before it is dispatched, with\n\
                          nothing run (looking up the marks does ask the driver); use use_desktop_tool for tools that act. The mark is the driver's own claim, checked\n\
                          against the driver connected at the time of each attempt: a safeguard against mistakes, not a\n\
                          guarantee against a driver or guest that mislabels a tool. Output and errors are the same as\n\
                          use_desktop_tool's.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = true
        )
    )]
    async fn inspect_desktop(&self, Parameters(a): Parameters<DesktopArgs>) -> CallToolResult {
        self.desktop_call(a, DesktopRoute::Inspect).await
    }

    #[tool(
        name = "get_job_status",
        title = "Get job status",
        description = "Check on a background job started by run_command (background: true), by the id it\n\
                          returned: queued, running, exited (with its code), stopped, ended without an exit\n\
                          code (killed, or the VM restarted), or missing (nothing has recorded itself under\n\
                          that id; a lost launch may still arrive), plus its log. Also takes the job_id of a\n\
                          foreground run_command whose outcome was uncertain (its output went to that reply, so\n\
                          there is no log). The result is structured (state, exit_code, log_tail or output,\n\
                          next_cursor, cursor_reset, wait_timed_out, output_truncated; an unreachable VM is an\n\
                          error with state unknown, never 'ended'). Without cursor it returns the log's last\n\
                          tail_lines lines and a next_cursor at the end of what it read. With cursor (a\n\
                          next_cursor, or \"start\") it returns the log from there, at most 16 KiB, and the\n\
                          cursor to continue from, so no output repeats or is lost; a cursor that no longer\n\
                          matches the log (VM reset, reboot, log replaced or truncated) is reset, said so, and\n\
                          reads from the log's start. wait_seconds (0 to 30, default 0) waits for the job to\n\
                          finish (with a cursor: or for new output) without ever stopping it. Read-only.",
        output_schema = jobs::output_schema(),
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn job_status(&self, Parameters(a): Parameters<JobArgs>) -> CallToolResult {
        job_status(a).await
    }

    #[tool(
        name = "stop_job",
        title = "Stop job",
        description = "Stop a background job started by run_command (background: true), or a foreground\n\
                          run's job_id: the job and every\n\
                          process it started (on Linux, its systemd scope when the guest has a user manager, which\n\
                          processes that detached stay in, else the processes marked with the job and their\n\
                          descendants; on Windows, its process tree).\n\
                          get_job_status then reports it as stopped. For a job that already exited, it\n\
                          stops only what the job left running (a server it started) and keeps its exit code.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn stop_job(&self, Parameters(a): Parameters<StopJobArgs>) -> CallToolResult {
        text(stop_job(&a.name, a.id).await)
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
    /// Both desktop routes: `use_desktop_tool` sends the call as it is; `inspect_desktop` first
    /// checks, on the connection each attempt uses, that the driver marks `tool` read-only, and
    /// otherwise sends nothing. The reply (images, mirrored structured content, error hints,
    /// reconnect note) is the same either way. Neither route falls back to the other.
    async fn desktop_call(&self, a: DesktopArgs, route: DesktopRoute) -> CallToolResult {
        let DesktopArgs {
            name,
            tool,
            arguments: call_args,
        } = a;
        let params =
            CallToolRequestParams::new(tool.clone()).with_arguments(call_args.unwrap_or_default());
        let inspect = route == DesktopRoute::Inspect;
        // `use_desktop_tool` may re-send after a dropped connection only a call that was
        // read-only on the connection it was sent on and still is on the one it is re-sent
        // to; both are checked at the attempt itself, on that attempt's connection, never
        // taken from the per-VM cache. The cache only decides whether an attempt asks at all:
        // a tool it does not know as read-only is sent once and never re-sent. An inspection
        // is classified on every attempt, so it can always be retried.
        let ask = !inspect && self.read_only_tool(&name, &tool).await;
        let sent = std::sync::atomic::AtomicBool::new(false);
        let proven = std::sync::atomic::AtomicBool::new(false);
        let r = self
            .with_session_gated(
                &name,
                &tool,
                // Nothing sent yet: re-sending is no repeat. Otherwise only on proof.
                || {
                    inspect
                        || !sent.load(std::sync::atomic::Ordering::SeqCst)
                        || proven.load(std::sync::atomic::Ordering::SeqCst)
                },
                |c: Client| {
                    let params = params.clone();
                    let (name, tool) = (name.clone(), tool.clone());
                    let (sent, proven) = (&sent, &proven);
                    async move {
                        use std::sync::atomic::Ordering::SeqCst;
                        let resend = sent.load(SeqCst);
                        if inspect || ask || resend {
                            let tools = c.list_all_tools().await?;
                            let verdict = inspect_verdict(&tools, &name, &tool);
                            if inspect {
                                if let Err(refusal) = verdict {
                                    return Ok(Dispatch::Refused(refusal));
                                }
                            } else if resend && verdict.is_err() {
                                return Ok(Dispatch::Refused(Refusal {
                                    message: format!(
                                        "use_desktop_tool did not re-send \"{tool}\" in {name}: the \
                                         connection dropped after it was sent, and the reconnected \
                                         desktop driver does not explicitly mark it read-only, so whether \
                                         the first send took effect is unknown. Look first (a new \
                                         screenshot or get_window_state) before repeating it."
                                    ),
                                    unknown_tool: false,
                                }));
                            } else {
                                proven.store(verdict.is_ok(), SeqCst);
                            }
                        }
                        sent.store(true, SeqCst);
                        c.call_tool(params).await.map(Dispatch::Done)
                    }
                },
            )
            .await;
        let failure = match r {
            Ok(Dispatch::Refused(refusal)) => {
                if !refusal.unknown_tool {
                    return self.noted(
                        &name,
                        CallToolResult::error(vec![ContentBlock::text(refusal.message)]),
                    );
                }
                refusal.message
            }
            Ok(Dispatch::Done(res)) if res.is_error == Some(true) => {
                let msg: Vec<&str> = res
                    .content
                    .iter()
                    .filter_map(|c| c.as_text().map(|t| t.text.as_str()))
                    .collect();
                let msg = msg.join(" ");
                if msg.is_empty() {
                    format!("{tool} failed")
                } else {
                    msg
                }
            }
            Ok(Dispatch::Done(res)) => {
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
                return self.noted(&name, out);
            }
            Err(e) => format!("{e:#}"),
        };
        // A wrong guess at a tool's name or arguments is the usual failure: answer it with the
        // right names or the arguments, so the next call can be right without another lookup.
        // (cua-driver reports an unknown name as a permission error, so check names here.)
        let tools = self
            .with_session(&name, "list_tools", true, |c: Client| async move {
                c.list_all_tools().await
            })
            .await
            .ok();
        let hint = match tools
            .as_deref()
            .map(|ts| (ts, ts.iter().find(|t| t.name == tool)))
        {
            Some((ts, None)) => {
                let names: Vec<&str> = ts.iter().map(|t| t.name.as_ref()).collect();
                let near = similar(&tool, &names);
                format!(
                    "\n\nThere is no desktop tool named \"{tool}\" in {name}{}; list_desktop_tools lists them.",
                    if near.is_empty() {
                        String::new()
                    } else {
                        format!(" (did you mean {}?)", near.join(", "))
                    }
                )
            }
            Some((_, Some(t))) if is_argument_error(&failure) => format!(
                "\n\n{tool} arguments (* = required): {}\nFull schema: list_desktop_tools with tool=\"{tool}\".",
                arguments(&t.input_schema),
            ),
            _ => String::new(),
        };
        self.noted(
            &name,
            CallToolResult::error(vec![ContentBlock::text(failure + &hint)]),
        )
    }

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

    /// Forget `failed`, if it is still the VM's connection: a call that failed on an old one
    /// must not throw away the replacement another call has made since.
    async fn drop_client(&self, name: &str, failed: &Client) {
        let slot = self.sessions.lock().unwrap().get(name).cloned();
        if let Some(slot) = slot {
            let mut slot = slot.lock().await;
            if slot.as_ref().is_some_and(|c| Arc::ptr_eq(c, failed)) {
                *slot = None;
            }
        }
    }

    #[cfg(not(test))]
    async fn dial(&self, name: &str) -> Result<RunningService<RoleClient, ()>> {
        connect(name).await
    }

    #[cfg(test)]
    async fn dial(&self, name: &str) -> Result<RunningService<RoleClient, ()>> {
        let fake = self.dials.lock().unwrap().pop_front();
        match fake {
            Some(c) => Ok(c),
            None => connect(name).await,
        }
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
        let c = Arc::new(self.dial(name).await?);
        *slot = Some(c.clone());
        // A new connection may be a restarted guest and driver: what the old one marked
        // read-only says nothing about this one.
        self.read_only.lock().unwrap().remove(name);
        if !self.connected.lock().unwrap().insert(name.to_string()) {
            self.reconnected.lock().unwrap().insert(name.to_string());
        }
        Ok(c)
    }

    /// Runs `f` on the instance's session. A transport failure usually means the guest
    /// restarted underneath us, so reconnect once and, with `retry`, try again. Without it (a
    /// click, typing: the call may have taken effect before the reply was lost) say so instead.
    async fn with_session<T, F>(
        &self,
        name: &str,
        what: &str,
        retry: bool,
        f: impl Fn(Client) -> F,
    ) -> Result<T>
    where
        F: Future<Output = Result<T, ServiceError>>,
    {
        self.with_session_gated(name, what, || retry, f).await
    }

    /// `with_session`, with the decision to re-send taken when a call fails rather than before
    /// it, so it can rest on what the attempts themselves found out.
    async fn with_session_gated<T, F>(
        &self,
        name: &str,
        what: &str,
        retry: impl Fn() -> bool,
        f: impl Fn(Client) -> F,
    ) -> Result<T>
    where
        F: Future<Output = Result<T, ServiceError>>,
    {
        let unknown = |why: String| {
            anyhow!(
                "{what} in {name}: {why}, so whether it took effect is unknown. Look first (a new \
                 screenshot or get_window_state) before repeating it."
            )
        };
        for attempt in 1..=2 {
            let client = self.session(name).await?;
            let used = client.clone();
            // A hung desktop tool must not wedge the call forever; treat a stall like a
            // transport failure so we reconnect once, then give up.
            let call = tokio::time::timeout(DESKTOP_CALL_TIMEOUT, f(client)).await;
            match call {
                Ok(Ok(v)) => return Ok(v),
                Ok(Err(ServiceError::McpError(e))) => {
                    bail!("{what} failed in {name}: {}", e.message)
                }
                Ok(Err(e)) => {
                    self.drop_client(name, &used).await;
                    if !retry() {
                        return Err(unknown(format!(
                            "the connection to the desktop driver dropped ({e})"
                        )));
                    }
                    if attempt == 2 {
                        bail!("{what} failed in {name}: {e}");
                    }
                }
                Err(_) => {
                    self.drop_client(name, &used).await;
                    if !retry() {
                        return Err(unknown(format!(
                            "no reply within {}s",
                            DESKTOP_CALL_TIMEOUT.as_secs()
                        )));
                    }
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

/// Which desktop route a call came in by.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DesktopRoute {
    /// `use_desktop_tool`: any tool, never re-sent after a drop unless read-only.
    Use,
    /// `inspect_desktop`: only tools the driver explicitly marks read-only.
    Inspect,
}

/// What one desktop attempt came to: the driver's reply, or `inspect_desktop`'s refusal made
/// before anything was sent.
enum Dispatch {
    Done(CallToolResult),
    Refused(Refusal),
}

struct Refusal {
    message: String,
    /// The driver has no tool of that name (so the reply can suggest close names).
    unknown_tool: bool,
}

/// Whether `inspect_desktop` may run `tool` given the active driver's tool list: only with an
/// explicit `readOnlyHint: true`. A missing tool, annotations or hint all fail closed.
fn inspect_verdict(tools: &[rmcp::model::Tool], vm: &str, tool: &str) -> Result<(), Refusal> {
    let Some(t) = tools.iter().find(|t| t.name == tool) else {
        return Err(Refusal {
            message: format!(
                "inspect_desktop did not run \"{tool}\" in {vm}: the desktop driver has no such \
                 tool. Nothing was sent to the VM."
            ),
            unknown_tool: true,
        });
    };
    let mark = match t.annotations.as_ref().and_then(|a| a.read_only_hint) {
        Some(true) => return Ok(()),
        Some(false) => "marks it as not read-only",
        None => "does not say whether it is read-only",
    };
    Err(Refusal {
        message: format!(
            "inspect_desktop did not run \"{tool}\" in {vm}: the desktop driver {mark}, and \
             inspect_desktop runs only tools it explicitly marks read-only. Nothing was sent to \
             the VM. If the tool is meant to act, call it with use_desktop_tool."
        ),
        unknown_tool: false,
    })
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

/// A single desktop-control tool call may take a while (typing, waits) but must not hang forever.
const DESKTOP_CALL_TIMEOUT: Duration = if cfg!(test) {
    // Short, so a test driver that never answers exercises the reconnect path quickly.
    Duration::from_millis(1500)
} else {
    Duration::from_secs(120)
};

/// Low-level command for agentpc's own scripts (job status, stop, probes): the plain SSH
/// session, untracked, killed at `timeout`. User commands go through `run_tracked`.
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
    let raw = ssh_raw(&inst, &command, timeout).await?;
    let text = raw.text(timeout);
    if raw.timed_out || raw.code != Some(0) {
        bail!("{text}");
    }
    Ok(text)
}

/// What one plain SSH command did.
struct Raw {
    /// ssh's exit code (255 when it couldn't connect; the command's own otherwise); None
    /// when it timed out or died of a signal.
    code: Option<i32>,
    timed_out: bool,
    stdout: Captured,
    stderr: Captured,
}

impl Raw {
    /// The reply text `exec` has always given: `exit code: N` (or the timeout), then the
    /// output sections.
    fn text(&self, timeout: u64) -> String {
        let head = if self.timed_out {
            format!(
                "timed out after {timeout}s. The SSH session was closed, but the command may still \
                 be running in the VM (find it with ps or Get-Process). Use background: true for \
                 long jobs.\n(partial output below)"
            )
        } else {
            format!("exit code: {}", self.code.unwrap_or(-1))
        };
        head + &execution::sections(&self.stdout.text(), &self.stderr.text())
    }
}

async fn ssh_raw(inst: &Instance, remote: &str, timeout: u64) -> Result<Raw> {
    let mut child = tokio::process::Command::new("ssh")
        .args(ops::ssh_args(inst, remote))
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
    let code = match &status {
        Ok(Ok(s)) => s.code(),
        _ => None,
    };
    Ok(Raw {
        code,
        timed_out,
        stdout,
        stderr,
    })
}

/// Run a user's foreground command, tracked (see `execution`): recorded in the VM before it
/// starts, stopped there at its timeout, and judged without guessing from text.
async fn run_tracked(name: &str, command: &str, timeout: u64) -> Reply {
    let inst = match load(name) {
        Ok(inst) => inst,
        Err(e) => return Reply::not_started(format!("{e:#}"), None),
    };
    // Too big to send: refused before any id is issued or anything reaches the VM.
    if matches!(inst.os, Os::Ubuntu | Os::Arch)
        && let Some(refused) = execution::linux_foreground_too_large(command)
    {
        return refused;
    }
    if !inst.running() {
        return Reply::not_started(
            format!("VM {name} is not running; start_vm first"),
            Some(RecoveryAction::StartVm),
        );
    }
    let id = job_id();
    let remote = match inst.os {
        Os::Ubuntu | Os::Arch => execution::linux_foreground(id, command, ops::LINUX_SESSION_ENV),
        Os::Windows => execution::windows_foreground(id, command),
    };
    let mut cmd = tokio::process::Command::new("ssh");
    cmd.args(ops::ssh_args(&inst, &remote));
    let limits = execution::Limits::DEFAULT;
    let facts = execution::run_foreground(
        cmd,
        id,
        Duration::from_secs(timeout),
        limits,
        || cancel_execution(&inst, id, limits.cancel),
        || probe_execution(&inst, id, limits.probe),
    )
    .await;
    match facts {
        Ok(f) => execution::conclude(f, id, name, timeout),
        Err(e) => Reply::spawn_failed(id, format!("run ssh: {e}")),
    }
}

/// Stop a timed-out foreground execution with `stop_job`'s script in cancel mode; its exit
/// code (not its text) says what it found.
async fn cancel_execution(inst: &Instance, id: u64, deadline: Duration) -> Cancel {
    let script = stop_job_script(inst.os, id, true);
    let remote = match inst.os {
        Os::Ubuntu | Os::Arch => script,
        Os::Windows => ops::windows_command(&script),
    };
    let raw = match ssh_raw(inst, &remote, deadline.as_secs()).await {
        Ok(raw) => raw,
        Err(e) => return Cancel::Unconfirmed(format!("{e:#}")),
    };
    Cancel::from_stop(
        raw.code,
        raw.timed_out,
        &format!("{}\n{}", raw.stdout.text(), raw.stderr.text()),
        deadline.as_secs(),
    )
}

/// After a failed connection: what the VM's records say (and make sure an execution that
/// never recorded itself never starts).
async fn probe_execution(inst: &Instance, id: u64, deadline: Duration) -> Probe {
    let remote = match inst.os {
        Os::Ubuntu | Os::Arch => execution::linux_probe(id),
        Os::Windows => ops::windows_command(&execution::windows_probe(id)),
    };
    match ssh_raw(inst, &remote, deadline.as_secs()).await {
        Err(e) => Probe::Failed(format!("{e:#}")),
        Ok(raw) if raw.timed_out => {
            Probe::Failed(format!("no answer within {}s", deadline.as_secs()))
        }
        Ok(raw) if raw.code != Some(0) => Probe::Failed(match raw.code {
            Some(c) => format!("ssh exited with code {c}"),
            None => "ssh ended without an exit code".into(),
        }),
        Ok(raw) => Probe::parse(&raw.stdout.text()),
    }
}

/// Start `command` detached from the SSH session, which would otherwise take it down when it
/// ends. On Windows that means a scheduled task in the logged-in session (so GUI apps show).
/// The command travels base64-encoded, so no quoting can break it.
async fn exec_background(name: &str, command: &str) -> Reply {
    use base64::Engine;
    let refused = |text: String, recovery| {
        let mut r = Reply::not_started(text, recovery);
        r.outcome.background = true;
        r
    };
    let inst = match load(name) {
        Ok(inst) => inst,
        Err(e) => return refused(format!("{e:#}"), None),
    };
    if !inst.running() {
        return refused(
            format!("VM {name} is not running; start_vm first"),
            Some(RecoveryAction::StartVm),
        );
    }
    let id = job_id();
    // On Windows, as in the foreground: a failed last command (a cmdlet error sets no exit
    // code) ends the job with its native code, or 1, rather than a 0 that looks like success.
    // The job first records its PowerShell's pid and creation time, so stop_job can still
    // find what it started (a server) after it exits, even once Windows reuses that pid.
    let script = match inst.os {
        Os::Windows => format!(
            "Set-Content -Encoding ascii \"$env:USERPROFILE\\agentpc-bg\\{id}.pid\" \"$PID $([Diagnostics.Process]::GetCurrentProcess().StartTime.ToFileTimeUtc())\"\n\
             {command}\n$__agentpc_ok = $?\nif (-not $__agentpc_ok) {{ if ($LASTEXITCODE) {{ exit $LASTEXITCODE }}; exit 1 }}"
        ),
        Os::Ubuntu | Os::Arch => command.to_string(),
    };
    let b64 = base64::engine::general_purpose::STANDARD.encode(script);
    let script = match inst.os {
        // See `execution::linux_background`.
        Os::Ubuntu | Os::Arch => execution::linux_background(id, &b64, ops::LINUX_SESSION_ENV),
        Os::Windows => ops::windows_command(&format!(
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
$log"#
        )),
    };
    let raw = match ssh_raw(&inst, &script, 60).await {
        Ok(raw) => raw,
        Err(e) => return execution::background_failed(id, format!("{e:#}"), false, name),
    };
    let failure = raw.text(60);
    match raw.code {
        Some(0) if !raw.timed_out => {}
        // The connection failed or hung: the job may have started anyway.
        Some(255) | None => return execution::background_failed(id, failure, true, name),
        Some(_) => return execution::background_failed(id, failure, false, name),
    }
    let printed = raw.stdout.text();
    let lines: Vec<&str> = printed.lines().map(str::trim).collect();
    let hints = format!(
        "Poll it with get_job_status name={name} id={id}. Output: {}. Stop it with stop_job \
         name={name} id={id}.",
        lines.last().copied().unwrap_or("?")
    );
    let (text, log) = match (inst.os, lines.as_slice()) {
        (Os::Ubuntu | Os::Arch, [pid, log]) => (
            format!("started in the background (id {id}, pid {pid}). {hints}"),
            Some(log.to_string()),
        ),
        (Os::Windows, [log]) => (
            format!(
                "started in the background as scheduled task agentpc-bg-{id} (id {id}). {hints}"
            ),
            Some(log.to_string()),
        ),
        // Started (the launch succeeded), but it printed something else.
        _ => (
            format!("started in the background (id {id}). {hints}"),
            None,
        ),
    };
    execution::background_started(id, text, log)
}

/// Report a job's state and log (see `jobs`): the guest's read-only status script is run over
/// SSH, once or repeatedly within `wait_seconds`, and its machine-readable report is turned
/// into the structured result and the readable text. A foreground execution's id (an <id>.fg
/// record, see `execution`) reports its state the same way, without a log.
async fn job_status(a: JobArgs) -> CallToolResult {
    let fail = |code, message: String| jobs::JobStatus::failure(a.id, code, message).into_result();
    let wait = match jobs::validate_wait(a.id, a.wait_seconds) {
        Ok(w) => w,
        Err(e) => return e.into_result(),
    };
    let resume = match a
        .cursor
        .as_deref()
        .map(|c| jobs::parse_cursor(c, &a.name, a.id))
    {
        None => None,
        Some(Ok(r)) => Some(r),
        Some(Err(e)) => return fail(jobs::JobErrorCode::InvalidCursor, e),
    };
    let inst = match load(&a.name) {
        Ok(inst) => inst,
        Err(e) => return fail(jobs::JobErrorCode::NoSuchVm, format!("{e:#}")),
    };
    if !inst.running() {
        return fail(
            jobs::JobErrorCode::VmNotRunning,
            format!("VM {} is not running; start_vm first", a.name),
        );
    }
    let generation = jobs::vm_generation(&inst.dir, inst.slot);
    let req = jobs::Request::new(&a.name, a.id, a.tail_lines, wait, resume, generation);
    let inst = &inst;
    jobs::observe(&req, jobs::Pacing::DEFAULT, |q, timeout| {
        guest_status(inst, a.id, q.clone(), timeout)
    })
    .await
    .into_result()
}

/// One status check: the read-only script over SSH, within `timeout`.
async fn guest_status(
    inst: &Instance,
    id: u64,
    query: jobs::Query,
    timeout: Duration,
) -> Result<jobs::GuestReport, jobs::Failure> {
    let script = match inst.os {
        Os::Ubuntu | Os::Arch => jobs::linux_script(id, &query),
        Os::Windows => ops::windows_command(&jobs::windows_script(id, &query)),
    };
    let secs = timeout.as_secs_f64().ceil().max(1.0) as u64;
    let raw = ssh_raw(inst, &script, secs)
        .await
        .map_err(|e| jobs::Failure::unreachable(&format!("{e:#}")))?;
    let why = |what: &str| {
        let stderr = raw.stderr.text();
        let stderr = stderr.lines().next().unwrap_or("").trim();
        if stderr.is_empty() {
            what.to_string()
        } else {
            format!("{what}: {}", execution::clip(stderr, 200))
        }
    };
    if raw.timed_out {
        return Err(jobs::Failure::timeout(&format!(
            "no answer within {secs} s"
        )));
    }
    match raw.code {
        Some(0) => {}
        Some(255) | None => {
            return Err(jobs::Failure::unreachable(&why(
                "ssh could not reach the VM",
            )));
        }
        Some(c) => {
            return Err(jobs::Failure::bad_reply(&why(&format!(
                "the status script exited {c}"
            ))));
        }
    }
    let bytes = raw
        .stdout
        .whole()
        .ok_or_else(|| jobs::Failure::bad_reply("the report was larger than expected"))?;
    jobs::parse_report(&bytes).map_err(|e| jobs::Failure::bad_reply(&e))
}

/// Stop a background job and everything it started (see exec_background), and mark it stopped
/// so get_job_status says so. For a job that already exited, it stops only what the job left
/// running and keeps its exit code.
async fn stop_job(name: &str, id: u64) -> Result<String> {
    let inst = load(name)?;
    if !inst.running() {
        bail!("VM {name} is not running; start_vm first");
    }
    let out = exec(name, &stop_job_script(inst.os, id, false), 60, false).await?;
    Ok(out
        .split_once("--- stdout ---\n")
        .map_or_else(|| out.clone(), |(_, s)| s.trim().to_string()))
}

/// The stop script (see `execution::linux_stop_script`), for stop_job or (`cancel`) for a
/// foreground execution at its timeout. Exit codes: 0 when none of the job's identified
/// processes is left (stopped, already exited, or not running), 1 when some would not stop,
/// 3 for no such job, and in cancel mode 4 when the execution never recorded itself: the
/// `<id>.cancel` written first means it now never starts.
fn stop_job_script(os: Os, id: u64, cancel: bool) -> String {
    match os {
        Os::Ubuntu | Os::Arch => execution::linux_stop_script(id, cancel),
        Os::Windows => execution::windows_stop_script(id, cancel),
    }
}

/// A background job's id, unique in the VM: two jobs sharing one would share their script,
/// log and exit files. The time in milliseconds, times 1024, plus the low bits of this
/// process's id (another MCP server may start one in the same millisecond); within this
/// process each id is above the last.
fn job_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let candidate = (now << 10) | u64::from(std::process::id() & 0x3ff);
    let next = |last: u64| {
        if candidate > last {
            candidate
        } else {
            last + 1024
        }
    };
    // A compare-exchange loop: fetch_update is deprecated on newer toolchains (try_update),
    // and try_update is missing on older ones down to the 1.88 minimum.
    let mut last = LAST.load(Ordering::SeqCst);
    loop {
        let id = next(last);
        match LAST.compare_exchange(last, id, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return id,
            Err(seen) => last = seen,
        }
    }
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
    /// The largest Linux foreground command agentpc accepts still fits the kernel's 128 KiB
    /// cap on one argument or environment string (MAX_ARG_STRLEN, counting its NUL) as the
    /// whole wrapper SSH runs, with the real session environment and the longest id.
    #[test]
    fn the_largest_accepted_linux_payload_fits_one_argument() {
        use crate::execution::{LINUX_FOREGROUND_MAX_BYTES, linux_foreground};
        const MAX_ARG_STRLEN: usize = 128 * 1024;
        let payload = "é".repeat(LINUX_FOREGROUND_MAX_BYTES / 2);
        assert_eq!(payload.len(), LINUX_FOREGROUND_MAX_BYTES);
        let wrapper = linux_foreground(u64::MAX, &payload, crate::ops::LINUX_SESSION_ENV);
        let margin = MAX_ARG_STRLEN - (wrapper.len() + 1);
        assert!(
            wrapper.len() < MAX_ARG_STRLEN && margin >= 1024,
            "wrapper {} bytes",
            wrapper.len()
        );
        // The payload's own copies: `AGENTPC_CMD=<payload>` and bash's `-c` argument.
        assert!("AGENTPC_CMD=".len() + payload.len() + 1 < MAX_ARG_STRLEN);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ownership_claim_does_not_block_the_async_executor() {
        use std::time::Duration;
        let gateway = super::Gateway::new();
        let (release, wait) = std::sync::mpsc::channel();
        let (claimed, _) = tokio::join!(
            gateway.claim_with("synthetic-vm", move || {
                wait.recv_timeout(Duration::from_secs(1))
                    .map_err(anyhow::Error::from)?;
                Ok(())
            }),
            async {
                tokio::time::sleep(Duration::from_millis(10)).await;
                let _ = release.send(());
            }
        );
        claimed.unwrap();
        assert!(gateway.owned.lock().unwrap().contains("synthetic-vm"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn failed_ownership_claim_is_propagated_and_not_tracked() {
        let gateway = super::Gateway::new();
        let error = gateway
            .claim_with("synthetic-vm", || {
                Err(anyhow::anyhow!("synthetic claim failure"))
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("synthetic claim failure"));
        assert!(!gateway.owned.lock().unwrap().contains("synthetic-vm"));
    }

    #[test]
    fn browser_open_argument_defaults_off_and_is_optional_in_schema() {
        let default: super::CreateArgs =
            serde_json::from_value(serde_json::json!({"os": "ubuntu"})).unwrap();
        assert!(!default.open_in_browser);
        let requested: super::CreateArgs =
            serde_json::from_value(serde_json::json!({"os": "ubuntu", "open_in_browser": true}))
                .unwrap();
        assert!(requested.open_in_browser);
        assert!(
            serde_json::from_value::<super::CreateArgs>(
                serde_json::json!({"os": "ubuntu", "open_in_browser": "true"})
            )
            .is_err()
        );
        let schema = serde_json::to_value(schemars::schema_for!(super::CreateArgs)).unwrap();
        assert_eq!(schema["properties"]["open_in_browser"]["type"], "boolean");
        assert!(
            !schema["required"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("open_in_browser"))
        );
    }

    /// tools/list declares run_command's output schema (the typed outcome, root type object),
    /// and only run_command's: other tools' results carry no structured content to match one.
    #[test]
    fn run_command_declares_its_output_schema() {
        let tools = super::Gateway::tool_router().list_all();
        let run = tools.iter().find(|t| t.name == "run_command").unwrap();
        let schema = run.output_schema.as_ref().expect("an output schema");
        assert_eq!(schema["type"], "object");
        for field in [
            "state",
            "exit_code",
            "timed_out",
            "termination_confirmed",
            "job_id",
        ] {
            assert!(schema["properties"].get(field).is_some(), "{field}");
        }
        // Review finding 4: what Windows can stop is best effort, and the tool says so.
        assert!(
            run.description
                .as_deref()
                .unwrap()
                .contains("best effort on Windows"),
            "{:?}",
            run.description
        );
        for t in tools
            .iter()
            .filter(|t| !["run_command", "get_job_status"].contains(&&*t.name))
        {
            assert!(t.output_schema.is_none(), "{}", t.name);
        }
        // The job tools accept a foreground execution's job_id too.
        for name in ["get_job_status", "stop_job"] {
            let t = tools.iter().find(|t| t.name == name).unwrap();
            assert!(
                t.description.as_deref().unwrap().contains("job_id"),
                "{name}"
            );
        }
    }

    /// Stop and status scripts know foreground records, check identity by proof before
    /// trusting a recorded pid, and cancel mode stops a not-yet-started execution for good.
    #[test]
    fn job_scripts_handle_foreground_records() {
        use crate::instance::Os;
        let stop = super::stop_job_script(Os::Ubuntu, 42, false);
        assert!(stop.contains("[ -f $d/42.log ] || [ -f $d/42.fg ]"));
        assert!(stop.contains("if own; then echo \"$p\"; pgrep -s \"$p\"; fi"));
        assert!(stop.contains("$d/42.proof"));
        assert!(!stop.contains("never started"));
        assert!(stop.contains("echo stop_job > $d/42.stopped"));
        let cancel = super::stop_job_script(Os::Ubuntu, 42, true);
        let tomb = cancel.find(": > $d/42.cancel").unwrap();
        let check = cancel
            .find("[ -f $d/42.pid ] || { echo 'job 42 never started'; exit 4; }")
            .unwrap();
        assert!(tomb < check);
        assert!(cancel.contains("echo timeout > $d/42.stopped"));
        let win = super::stop_job_script(Os::Windows, 42, true);
        let tomb = win.find("42.cancel").unwrap();
        let check = win.find("never started\"; exit 4").unwrap();
        assert!(tomb < check);
        assert!(win.contains("Set-Content -Encoding ascii \"$d\\42.stopped\" 'timeout'"));
        // get_job_status's scripts know foreground records, and say when one stopped at its
        // timeout (the `stop` word is the one stop_job and the cancel mode write).
        let q = crate::jobs::Query {
            mode: crate::jobs::Mode::Tail,
            cap: crate::jobs::REPLY_BYTES,
        };
        for st in [
            crate::jobs::linux_script(42, &q),
            crate::jobs::windows_script(42, &q),
        ] {
            assert!(
                st.contains("42.fg") && st.contains("foreground") && st.contains("timeout"),
                "{st}"
            );
        }
    }

    #[test]
    fn job_args_keep_the_old_shape_and_add_wait_and_cursor() {
        let a: super::JobArgs = serde_json::from_str(r#"{"name":"vm","id":7}"#).unwrap();
        assert_eq!(a.wait_seconds, 0);
        assert!(a.cursor.is_none());
        assert_eq!(a.tail_lines, 50);
        let a: super::JobArgs = serde_json::from_str(
            r#"{"name":"vm","id":7,"tail_lines":5,"wait_seconds":30,"cursor":"start"}"#,
        )
        .unwrap();
        assert_eq!((a.tail_lines, a.wait_seconds), (5, 30));
        assert_eq!(a.cursor.as_deref(), Some("start"));
        // Negative or fractional waits are not integers: the protocol refuses them.
        assert!(
            serde_json::from_str::<super::JobArgs>(r#"{"name":"vm","id":7,"wait_seconds":-1}"#)
                .is_err()
        );
    }

    /// tools/list: get_job_status stays read-only and declares its structured result (success
    /// and error alike, one schema), and its input schema gained exactly the two options.
    #[test]
    fn get_job_status_declares_read_only_and_its_schemas() {
        let tools = super::Gateway::tool_router().list_all();
        let t = tools.iter().find(|t| t.name == "get_job_status").unwrap();
        let ann = t.annotations.as_ref().unwrap();
        assert_eq!(ann.read_only_hint, Some(true));
        assert!(!ann.destructive_hint.unwrap_or(false));
        let out = t.output_schema.as_ref().expect("an output schema");
        assert_eq!(out["type"], "object");
        for f in [
            "state",
            "exit_code",
            "log_tail",
            "output",
            "next_cursor",
            "cursor_reset",
            "wait_timed_out",
            "output_truncated",
            "error",
            "error_code",
        ] {
            assert!(out["properties"].get(f).is_some(), "{f}");
        }
        let input = &t.input_schema;
        for f in ["name", "id", "tail_lines", "wait_seconds", "cursor"] {
            assert!(input["properties"].get(f).is_some(), "{f}");
        }
        let required = super::required(input);
        assert_eq!(required, ["name", "id"], "the new options are optional");
        // The schema declares the wait bound; the handler still enforces it (a client that
        // does not validate must get the structured error).
        let w = &input["properties"]["wait_seconds"];
        assert_eq!(w["maximum"], crate::jobs::MAX_WAIT_SECONDS, "{w}");
        assert_eq!(w["minimum"], 0, "{w}");
        let d = t.description.as_deref().unwrap();
        assert!(
            d.contains("wait_seconds")
                && d.contains("without ever stopping it")
                && d.contains("Read-only"),
            "{d}"
        );
        // stop_job stays a mutation.
        let stop = tools.iter().find(|t| t.name == "stop_job").unwrap();
        assert_eq!(
            stop.annotations.as_ref().unwrap().read_only_hint,
            Some(false)
        );
    }

    /// get_job_status's path (the handler, `job_status` and `guest_status`) is a read: it
    /// never calls the probe that tombstones an unstarted execution, the cancellation, or
    /// the stop script.
    #[test]
    fn get_job_status_never_reaches_the_mutating_probe_or_cancel() {
        let src = include_str!("mcp.rs");
        let prod = &src[..src.find("#[cfg(test)]\nmod tests").unwrap()];
        let start = prod.find("async fn job_status(a: JobArgs)").unwrap();
        let end = start + prod[start..].find("/// Stop a background job").unwrap();
        let path = &prod[start..end];
        assert!(path.contains("guest_status"), "the slice is the right one");
        for name in [
            "probe_execution",
            "cancel_execution",
            "stop_job",
            "run_tracked",
            "linux_probe",
            "windows_probe",
            "stop_job_script",
            ".cancel",
        ] {
            assert!(!path.contains(name), "{name} in get_job_status's path");
        }
    }

    /// The tool itself, through its handler, with no VM: a wait above 30 and a bad cursor are
    /// structured errors that need no SSH, and the schema's fields are all there.
    #[tokio::test(flavor = "current_thread")]
    async fn get_job_status_refuses_bad_arguments_with_structured_errors() {
        let call = |wait: u64, cursor: Option<&str>| {
            super::job_status(super::JobArgs {
                name: "no-such-vm-for-jobs-test".into(),
                id: 9,
                tail_lines: 5,
                wait_seconds: wait,
                cursor: cursor.map(str::to_string),
            })
        };
        let r = call(31, None).await;
        assert_eq!(r.is_error, Some(true));
        let v = r.structured_content.unwrap();
        assert_eq!(v["error_code"], "invalid_wait_seconds");
        assert_eq!(v["state"], "unknown");
        assert_eq!(v["job_id"], 9);
        let r = call(0, Some("garbage")).await;
        assert_eq!(
            r.structured_content.unwrap()["error_code"],
            "invalid_cursor"
        );
        // Valid arguments for a VM that does not exist: its own error, and still structured.
        let r = call(0, Some("start")).await;
        assert_eq!(r.structured_content.unwrap()["error_code"], "no_such_vm");
        let r = call(30, None).await;
        assert_eq!(r.structured_content.unwrap()["error_code"], "no_such_vm");
    }

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
        // Each keeps this process's bits, which another server's ids don't share.
        let me = u64::from(std::process::id() & 0x3ff);
        assert!(ids.iter().all(|id| id & 0x3ff == me));
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

    // ---- inspect_desktop: dispatch tests against a fake desktop driver -------------------

    use rmcp::handler::server::wrapper::Parameters;
    use rmcp::model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool, ToolAnnotations,
    };
    use rmcp::service::RequestContext;
    use rmcp::{RoleServer, ServerHandler, ServiceExt};
    use std::sync::{Arc, Mutex};

    /// A stand-in for cua-driver, served over an in-memory pipe: it lists `tools`, records the
    /// name of every call that reaches it, and answers by tool name (or never, with `hang`).
    struct Fake {
        tools: Vec<Tool>,
        calls: Arc<Mutex<Vec<String>>>,
        hang: bool,
    }

    impl ServerHandler for Fake {
        fn get_info(&self) -> ServerConfig {
            ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
        }

        async fn list_tools(
            &self,
            _: Option<PaginatedRequestParams>,
            _: RequestContext<RoleServer>,
        ) -> Result<ListToolsResult, rmcp::ErrorData> {
            Ok(ListToolsResult::with_all_items(self.tools.clone()))
        }

        async fn call_tool(
            &self,
            req: CallToolRequestParams,
            _: RequestContext<RoleServer>,
        ) -> Result<CallToolResponse, rmcp::ErrorData> {
            self.calls.lock().unwrap().push(req.name.to_string());
            if self.hang {
                std::future::pending::<()>().await;
            }
            let r = match req.name.as_ref() {
                "get_window_state" => {
                    let mut r = CallToolResult::success(vec![
                        ContentBlock::text("window tree"),
                        ContentBlock::image("iVBORw0KGgo=", "image/png"),
                    ]);
                    r.structured_content = Some(serde_json::json!({"snapshot_id": "s1"}));
                    r
                }
                "needs_pid" => CallToolResult::error(vec![ContentBlock::text(
                    "Missing required parameter: pid",
                )]),
                other => CallToolResult::success(vec![ContentBlock::text(format!("ran {other}"))]),
            };
            Ok(r.into())
        }
    }

    fn tool(name: &'static str, read_only: Option<bool>, annotated: bool) -> Tool {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"pid": {"type": "integer"}},
            "required": ["pid"]
        });
        let schema = schema.as_object().unwrap().clone();
        let t = Tool::new(name, "a fake driver tool", schema);
        if !annotated {
            return t;
        }
        let mut a = ToolAnnotations::new().destructive(false);
        a.read_only_hint = read_only;
        t.annotate(a)
    }

    /// The driver's tools: `get_window_state` and `needs_pid` read-only, `click` not,
    /// `type_text` with no annotations at all, `odd` annotated but silent on read-only.
    fn driver_tools(window_state_read_only: bool) -> Vec<Tool> {
        vec![
            tool("get_window_state", Some(window_state_read_only), true),
            tool("needs_pid", Some(true), true),
            tool("click", Some(false), true),
            tool("type_text", None, false),
            tool("odd", None, true),
        ]
    }

    async fn fake(
        tools: Vec<Tool>,
        calls: &Arc<Mutex<Vec<String>>>,
        hang: bool,
    ) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
        let (server_io, client_io) = tokio::io::duplex(1 << 16);
        let driver = Fake {
            tools,
            calls: calls.clone(),
            hang,
        };
        tokio::spawn(async move {
            if let Ok(running) = driver.serve(server_io).await {
                let _ = running.waiting().await;
            }
        });
        ().serve(client_io).await.unwrap()
    }

    /// A gateway whose next desktop connections are these fake drivers, in order.
    async fn gateway_with(
        drivers: Vec<(Vec<Tool>, bool)>,
        calls: &Arc<Mutex<Vec<String>>>,
    ) -> super::Gateway {
        let gateway = super::Gateway::new();
        for (tools, hang) in drivers {
            let c = fake(tools, calls, hang).await;
            gateway.dials.lock().unwrap().push_back(c);
        }
        gateway
    }

    fn desktop(tool: &str, arguments: serde_json::Value) -> super::DesktopArgs {
        super::DesktopArgs {
            name: "fake-vm".into(),
            tool: tool.into(),
            arguments: arguments.as_object().cloned(),
        }
    }

    fn texts(r: &CallToolResult) -> String {
        r.content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.as_str()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn inspect_runs_an_explicitly_read_only_tool_and_matches_use_desktop_tool() {
        let calls = Arc::default();
        let gw = gateway_with(vec![(driver_tools(true), false)], &calls).await;
        let args = serde_json::json!({"pid": 7});
        let inspected = gw
            .inspect_desktop(Parameters(desktop("get_window_state", args.clone())))
            .await;
        let used = gw
            .use_desktop_tool(Parameters(desktop("get_window_state", args)))
            .await;
        assert_eq!(inspected.is_error, Some(false));
        // Both routes: the image survives, the structured content is kept and mirrored as text.
        assert!(inspected.content.iter().any(|c| c.as_image().is_some()));
        assert_eq!(
            inspected.structured_content,
            Some(serde_json::json!({"snapshot_id": "s1"}))
        );
        assert!(texts(&inspected).contains("\"snapshot_id\": \"s1\""));
        assert_eq!(
            serde_json::to_value(&inspected).unwrap(),
            serde_json::to_value(&used).unwrap(),
            "the two routes answer identically"
        );
        assert_eq!(*calls.lock().unwrap(), ["get_window_state"; 2]);
    }

    #[tokio::test]
    async fn inspect_refuses_everything_but_explicit_read_only_before_dispatch() {
        let calls: Arc<Mutex<Vec<String>>> = Arc::default();
        let gw = gateway_with(vec![(driver_tools(true), false)], &calls).await;
        // Valid arguments throughout: the refusal is about classification, not the arguments.
        for (name, why) in [
            ("click", "marks it as not read-only"),
            ("type_text", "does not say whether it is read-only"),
            ("odd", "does not say whether it is read-only"),
            ("launch_app", "has no such tool"),
        ] {
            let r = gw
                .inspect_desktop(Parameters(desktop(name, serde_json::json!({"pid": 1}))))
                .await;
            assert_eq!(r.is_error, Some(true), "{name}");
            let msg = texts(&r);
            assert!(msg.contains(why), "{name}: {msg}");
            assert!(msg.contains("Nothing was sent to the VM"), "{name}: {msg}");
            assert!(
                calls.lock().unwrap().is_empty(),
                "{name} reached the driver"
            );
        }
        // A near-miss for an unknown name still gets the close-name hint.
        let r = gw
            .inspect_desktop(Parameters(desktop("clik", serde_json::json!({}))))
            .await;
        assert!(texts(&r).contains("did you mean click?"), "{}", texts(&r));
        assert!(calls.lock().unwrap().is_empty());
        // No fallback: the mutation route is a separate, explicit call.
        let r = gw
            .use_desktop_tool(Parameters(desktop("click", serde_json::json!({"pid": 1}))))
            .await;
        assert_eq!(r.is_error, Some(false));
        assert_eq!(*calls.lock().unwrap(), ["click"]);
    }

    #[tokio::test]
    async fn inspect_keeps_the_error_and_argument_hints_of_use_desktop_tool() {
        let calls = Arc::default();
        let gw = gateway_with(vec![(driver_tools(true), false)], &calls).await;
        let a = gw
            .inspect_desktop(Parameters(desktop("needs_pid", serde_json::json!({}))))
            .await;
        let b = gw
            .use_desktop_tool(Parameters(desktop("needs_pid", serde_json::json!({}))))
            .await;
        assert_eq!(a.is_error, Some(true));
        let msg = texts(&a);
        assert!(msg.contains("Missing required parameter: pid"), "{msg}");
        assert!(msg.contains("needs_pid arguments (* = required)"), "{msg}");
        assert_eq!(
            serde_json::to_value(&a).unwrap(),
            serde_json::to_value(&b).unwrap()
        );
    }

    /// The first driver says the tool is read-only but never answers; the retry after the
    /// timeout lands on a reconnected driver that says it is not. The retry must be refused
    /// on the new driver's word, and nothing may reach it.
    #[tokio::test]
    async fn reconnect_reclassifies_before_the_retry_and_denies_a_changed_tool() {
        let calls: Arc<Mutex<Vec<String>>> = Arc::default();
        let gw = gateway_with(
            vec![(driver_tools(true), true), (driver_tools(false), false)],
            &calls,
        )
        .await;
        let r = gw
            .inspect_desktop(Parameters(desktop(
                "get_window_state",
                serde_json::json!({"pid": 1}),
            )))
            .await;
        assert_eq!(r.is_error, Some(true));
        let msg = texts(&r);
        assert!(msg.contains("marks it as not read-only"), "{msg}");
        assert!(msg.contains("reconnected to the desktop driver"), "{msg}");
        // Only the first (read-only at the time) attempt was ever sent; the second driver
        // never saw the call.
        assert_eq!(*calls.lock().unwrap(), ["get_window_state"]);
        assert!(gw.dials.lock().unwrap().is_empty());
    }

    /// The same drop with an unchanged driver: the retry is allowed (it is read-only on the
    /// new connection too) and answers normally.
    #[tokio::test]
    async fn reconnect_retries_an_inspection_the_new_driver_still_marks_read_only() {
        let calls: Arc<Mutex<Vec<String>>> = Arc::default();
        let gw = gateway_with(
            vec![(driver_tools(true), true), (driver_tools(true), false)],
            &calls,
        )
        .await;
        let r = gw
            .inspect_desktop(Parameters(desktop(
                "get_window_state",
                serde_json::json!({"pid": 1}),
            )))
            .await;
        assert_eq!(r.is_error, Some(false), "{}", texts(&r));
        assert_eq!(*calls.lock().unwrap(), ["get_window_state"; 2]);
    }

    #[tokio::test]
    async fn a_new_connection_forgets_the_cached_read_only_marks() {
        let calls = Arc::default();
        let gw = gateway_with(vec![(driver_tools(true), false)], &calls).await;
        gw.read_only
            .lock()
            .unwrap()
            .insert("fake-vm".into(), ["click".to_string()].into());
        gw.session("fake-vm").await.unwrap();
        assert!(gw.read_only.lock().unwrap().get("fake-vm").is_none());
    }

    #[tokio::test]
    async fn the_full_schema_reply_carries_the_tools_annotations() {
        let calls = Arc::default();
        let gw = gateway_with(vec![(driver_tools(true), false)], &calls).await;
        for (name, expected) in [("get_window_state", true), ("click", false)] {
            let r = gw
                .list_desktop_tools(Parameters(super::ToolsArgs {
                    name: "fake-vm".into(),
                    tool: Some(name.into()),
                }))
                .await;
            let v: serde_json::Value = serde_json::from_str(&texts(&r)).unwrap();
            assert_eq!(v["annotations"]["readOnlyHint"], expected, "{v}");
            assert!(v["input_schema"].is_object());
        }
    }

    #[test]
    fn tools_list_declares_inspect_desktop_read_only_and_use_desktop_tool_destructive() {
        let tools = super::Gateway::tool_router().list_all();
        let inspect = tools.iter().find(|t| t.name == "inspect_desktop").unwrap();
        let used = tools.iter().find(|t| t.name == "use_desktop_tool").unwrap();
        let a = inspect.annotations.as_ref().unwrap();
        assert_eq!(a.read_only_hint, Some(true));
        assert_eq!(a.destructive_hint, Some(false));
        assert_eq!(a.open_world_hint, Some(true));
        let u = used.annotations.as_ref().unwrap();
        assert_eq!(u.read_only_hint, Some(false));
        assert_eq!(u.destructive_hint, Some(true));
        assert_eq!(u.open_world_hint, Some(true));
        // Same input shape: identical schemas, with name and tool required.
        assert_eq!(inspect.input_schema, used.input_schema);
        assert_eq!(super::required(&inspect.input_schema), ["name", "tool"]);
        assert!(inspect.output_schema.is_none());
        let d = inspect.description.as_deref().unwrap();
        assert!(d.contains("explicitly marks it read-only"), "{d}");
        assert!(d.contains("not a\ngu") || d.contains("guarantee"), "{d}");
    }

    // ---- use_desktop_tool: a dropped call is re-sent only on proof ----------------------

    /// The first driver marks the tool read-only and never answers; the one it reconnects to
    /// marks it not read-only. The call was sent once and must not be sent again.
    #[tokio::test]
    async fn use_does_not_resend_a_call_the_reconnected_driver_no_longer_marks_read_only() {
        let calls: Arc<Mutex<Vec<String>>> = Arc::default();
        let gw = gateway_with(
            vec![(driver_tools(true), true), (driver_tools(false), false)],
            &calls,
        )
        .await;
        let r = gw
            .use_desktop_tool(Parameters(desktop(
                "get_window_state",
                serde_json::json!({"pid": 1}),
            )))
            .await;
        assert_eq!(r.is_error, Some(true));
        let msg = texts(&r);
        assert!(msg.contains("did not re-send"), "{msg}");
        assert_eq!(*calls.lock().unwrap(), ["get_window_state"], "sent once");
    }

    /// A stale cache that calls a mutator read-only does not authorize a re-send: the driver
    /// the call was actually sent on says it is not.
    #[tokio::test]
    async fn use_does_not_resend_a_mutator_even_if_the_cache_says_read_only() {
        let calls: Arc<Mutex<Vec<String>>> = Arc::default();
        let gw = gateway_with(
            vec![(driver_tools(true), true), (driver_tools(true), false)],
            &calls,
        )
        .await;
        gw.session("fake-vm").await.unwrap();
        gw.read_only
            .lock()
            .unwrap()
            .insert("fake-vm".into(), ["click".to_string()].into());
        let r = gw
            .use_desktop_tool(Parameters(desktop("click", serde_json::json!({"pid": 1}))))
            .await;
        assert_eq!(r.is_error, Some(true));
        let msg = texts(&r);
        assert!(msg.contains("whether it took effect is unknown"), "{msg}");
        assert_eq!(*calls.lock().unwrap(), ["click"], "sent once");
        // (The error hint's own tool listing reconnects; the call itself was not repeated.)
    }

    /// With the cache empty or false for a mutator, the same: one send, no retry.
    #[tokio::test]
    async fn use_sends_a_mutation_once_and_does_not_retry_after_a_drop() {
        let calls: Arc<Mutex<Vec<String>>> = Arc::default();
        let gw = gateway_with(
            vec![(driver_tools(true), true), (driver_tools(true), false)],
            &calls,
        )
        .await;
        let r = gw
            .use_desktop_tool(Parameters(desktop("click", serde_json::json!({"pid": 1}))))
            .await;
        assert_eq!(r.is_error, Some(true));
        assert_eq!(*calls.lock().unwrap(), ["click"]);
    }

    /// Ordinary behaviour kept: a read-only call lost to a drop is re-sent when the new driver
    /// still marks it read-only.
    #[tokio::test]
    async fn use_still_resends_a_read_only_call_when_the_new_driver_agrees() {
        let calls: Arc<Mutex<Vec<String>>> = Arc::default();
        let gw = gateway_with(
            vec![(driver_tools(true), true), (driver_tools(true), false)],
            &calls,
        )
        .await;
        let r = gw
            .use_desktop_tool(Parameters(desktop(
                "get_window_state",
                serde_json::json!({"pid": 1}),
            )))
            .await;
        assert_eq!(r.is_error, Some(false), "{}", texts(&r));
        assert!(r.content.iter().any(|c| c.as_image().is_some()));
        assert_eq!(*calls.lock().unwrap(), ["get_window_state"; 2]);
    }
}

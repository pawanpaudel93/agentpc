//! Tracked user commands for `run_command`: a typed outcome (the tool's structured content,
//! with the human-readable text kept as its content), the guest-side wrappers that record a
//! foreground command's identity before it runs, and the host-side run that cancels it in the
//! guest at its timeout.
//!
//! A foreground command gets a server-issued id, the same kind as a background job's, and its
//! wrapper records it under `~/agentpc-bg/<id>.*` (`%USERPROFILE%\agentpc-bg` on Windows)
//! before the payload starts: on Linux the wrapper's pid with its boot id and start time, in
//! the job's systemd scope (`agentpc-job-<id>.scope`) when the user manager is up; on Windows
//! the session PowerShell's pid and creation time. The wrapper then tells the host the payload
//! is starting with one marker line on stderr, which the host removes, so neither stream
//! carries agentpc's metadata. At a timeout the host stops the execution through the same
//! script as `stop_job` and reports whether nothing it identified is left; when it can't tell
//! (a dropped connection, a guest that won't answer), it says so and returns the id, which
//! `get_job_status` and `stop_job` accept. Payload code is never run again automatically.
//!
//! Low-level infrastructure commands (probes, job scripts) don't go through here: they use
//! the plain `exec` in `mcp`.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use rmcp::model::{CallToolResult, ContentBlock};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Version of [`CommandOutcome`]'s layout; bumped only for incompatible changes.
pub const SCHEMA_VERSION: u32 = 1;

/// Characters kept from each end of a long stdout or stderr.
pub const OUTPUT_KEEP: usize = 10_000;

/// Bytes kept from each end of a stream: enough for OUTPUT_KEEP characters of any UTF-8.
pub const CAPTURE_KEEP: usize = 4 * OUTPUT_KEEP;

/// Bounds on the steps after a foreground command's own run.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// How long the guest-side cancellation of a timed-out command may take (connecting,
    /// TERM, then KILL rounds, rescans) before its termination is reported unconfirmed.
    pub cancel: Duration,
    /// How long the follow-up check after a dropped connection may take.
    pub probe: Duration,
    /// After a cancellation, how long the command's own SSH session gets to end by itself
    /// (its processes are gone, so the guest closes it) before it is killed here.
    pub session_grace: Duration,
    /// How long reading what is left in the local pipes may take once ssh is gone.
    pub drain_grace: Duration,
}

impl Limits {
    pub const DEFAULT: Limits = Limits {
        cancel: Duration::from_secs(45),
        probe: Duration::from_secs(20),
        session_grace: Duration::from_secs(5),
        drain_grace: Duration::from_secs(2),
    };
}

/// Wrapper bytes kept back while looking for the start marker. The wrapper writes nothing
/// itself; only a login script's noise or an error comes before the marker.
const SCAN_LIMIT: usize = 64 * 1024;

/// Exit codes of the wrappers before the payload starts (no marker was written).
pub const WRAPPER_SETUP_FAILED: i32 = 97;
pub const WRAPPER_CANCELLED: i32 = 98;

/// What happened to a command, as `run_command` reports it in its structured content. The
/// same object describes success and failure (`is_error` is set unless `state` is `exited`
/// with code 0 and the call didn't reach its timeout, or `started` for a background job).
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct CommandOutcome {
    /// Layout version of this object (currently 1).
    pub schema_version: u32,
    /// `exited`: the command ran to its end, `exit_code` is its code. `timed_out`: it was
    /// still running at `timeout` and agentpc tried to stop it (see `termination_confirmed`).
    /// `not_started`: it never ran (and won't). `connection_lost`: the SSH connection failed
    /// while it may have been running (see `effect_unknown`). `started`: a background job was
    /// started.
    pub state: ExecutionState,
    /// Id agentpc gave this execution in the VM (foreground or background); null when the
    /// call failed before one was issued.
    pub execution_id: Option<u64>,
    /// The id to pass to get_job_status and stop_job: set for a background job and whenever a
    /// foreground execution's fate is uncertain; null otherwise.
    pub job_id: Option<u64>,
    /// Whether this was a `background: true` call.
    pub background: bool,
    /// The command's exit code when known (from the reply, or the VM's record of it), as an SSH
    /// client sees it: on Windows the low 8 bits of PowerShell's code (`exit -1` reads 255), as
    /// run_command has always reported it. Null otherwise.
    pub exit_code: Option<i32>,
    /// The command's standard output; past 2 x 10,000 characters only its two ends, with a
    /// note of what was left out between them.
    pub stdout: String,
    /// The command's standard error, trimmed like stdout.
    pub stderr: String,
    /// Bytes the command wrote to stdout in all (as received).
    pub stdout_bytes: u64,
    /// Bytes the command wrote to stderr in all (as received).
    pub stderr_bytes: u64,
    /// Whether part of stdout was left out of `stdout`.
    pub stdout_truncated: bool,
    /// Whether part of stderr was left out of `stderr`.
    pub stderr_truncated: bool,
    /// Either of the above.
    pub output_truncated: bool,
    /// Whether the call reached `timeout`: the command was still running then, or (with `state`
    /// `exited`) it had finished but processes it left running still held its output open.
    pub timed_out: bool,
    /// After a timeout: true once agentpc verified that none of the processes it identified as
    /// the execution's (its systemd scope, its marked or descendant processes, its recorded
    /// root by pid and start time) are left; false when that couldn't be verified (it may
    /// still be running; also whenever the process list couldn't be read). Null when nothing
    /// needed stopping (no timeout, or it never started). Processes that left that tracking (a
    /// system service it started, for one; on Windows, a process whose launcher had already
    /// exited) aren't covered.
    pub termination_confirmed: Option<bool>,
    /// True when it is unknown whether the command ran, finished or is still running (a lost
    /// connection, or a termination that couldn't be confirmed): check before repeating it.
    pub effect_unknown: bool,
    /// What to do next, when something is needed: `get_job_status` / `stop_job` with
    /// `job_id`, `start_vm`, `retry` (it provably never ran), or `inspect_vm` (look at the
    /// VM's state before running it again). Null when nothing is needed.
    pub recovery_action: Option<RecoveryAction>,
    /// A background job's log file in the VM; null otherwise.
    pub log_path: Option<String>,
    /// Why the call failed, when it failed for a reason other than the command's own exit code.
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum ExecutionState {
    Exited,
    TimedOut,
    NotStarted,
    ConnectionLost,
    Started,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum RecoveryAction {
    GetJobStatus,
    StopJob,
    StartVm,
    Retry,
    InspectVm,
}

impl CommandOutcome {
    /// An outcome with no output and nothing known yet, to fill in.
    fn new(state: ExecutionState, execution_id: Option<u64>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            state,
            execution_id,
            job_id: None,
            background: false,
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            stdout_bytes: 0,
            stderr_bytes: 0,
            stdout_truncated: false,
            stderr_truncated: false,
            output_truncated: false,
            timed_out: false,
            termination_confirmed: None,
            effect_unknown: false,
            recovery_action: None,
            log_path: None,
            error: None,
        }
    }

    /// The call failed before anything ran (no such VM, VM stopped, ...).
    pub fn not_started(error: String, recovery: Option<RecoveryAction>) -> Self {
        let mut o = Self::new(ExecutionState::NotStarted, None);
        o.error = Some(error);
        o.recovery_action = recovery;
        o
    }

    fn with_output(mut self, stdout: &Captured, stderr: &Captured) -> Self {
        let (out, out_cut) = stdout.render();
        let (err, err_cut) = stderr.render();
        self.stdout = out;
        self.stderr = err;
        self.stdout_bytes = stdout.total as u64;
        self.stderr_bytes = stderr.total as u64;
        self.stdout_truncated = out_cut;
        self.stderr_truncated = err_cut;
        self.output_truncated = out_cut || err_cut;
        self
    }

    /// Whether the tool call reports an error.
    /// A reply that reached `timeout` is always one, as it always was (even when the
    /// command's own exit code 0 is known).
    pub fn is_error(&self) -> bool {
        self.timed_out
            || !matches!(
                (self.state, self.exit_code),
                (ExecutionState::Exited, Some(0)) | (ExecutionState::Started, _)
            )
    }
}

/// A command's reply: the outcome and the human-readable text clients without structured
/// content show (the same text as before structured results: `exit code: N`, then
/// `--- stdout ---` / `--- stderr ---` sections).
#[derive(Debug, Clone)]
pub struct Reply {
    pub outcome: CommandOutcome,
    pub text: String,
}

impl Reply {
    /// Text plus the outcome as structured content. The text block is the only content, so
    /// clients that concatenate text blocks see what they always saw.
    pub fn into_result(self) -> CallToolResult {
        let error = self.outcome.is_error();
        let structured = serde_json::to_value(&self.outcome).expect("outcome serializes");
        let mut r = if error {
            CallToolResult::error(vec![ContentBlock::text(self.text)])
        } else {
            CallToolResult::success(vec![ContentBlock::text(self.text)])
        };
        r.structured_content = Some(structured);
        r
    }

    /// ssh itself couldn't be started for execution `id`.
    pub fn spawn_failed(id: u64, text: String) -> Self {
        let mut r = Self::not_started(text, Some(RecoveryAction::Retry));
        r.outcome.execution_id = Some(id);
        r
    }

    /// A failure before anything was started, with `text` as its message.
    pub fn not_started(text: String, recovery: Option<RecoveryAction>) -> Self {
        Self {
            outcome: CommandOutcome::not_started(text.clone(), recovery),
            text,
        }
    }
}

/// The output schema `run_command` declares.
pub fn output_schema() -> Arc<rmcp::model::JsonObject> {
    rmcp::handler::server::tool::schema_for_output::<CommandOutcome>()
}

/// The "--- stdout ---" / "--- stderr ---" sections of a reply, for those not empty.
pub fn sections(stdout: &str, stderr: &str) -> String {
    let mut text = String::new();
    for (label, s) in [("stdout", stdout), ("stderr", stderr)] {
        if !s.is_empty() {
            text += &format!("\n--- {label} ---\n{s}");
        }
    }
    text
}

// ---------------------------------------------------------------------------------------------
// Output capture

/// What a command printed on one stream: only its first and last bytes are kept while
/// reading, so a command that prints gigabytes can't exhaust this server's memory.
#[derive(Default, Debug, Clone)]
pub struct Captured {
    pub(crate) head: Vec<u8>,
    pub(crate) tail: std::collections::VecDeque<u8>,
    pub(crate) total: usize,
}

impl Captured {
    pub fn push(&mut self, mut chunk: &[u8]) {
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

    /// Every byte received, or None when some were dropped from the middle (more than the two
    /// kept ends). For machine-readable replies, which must not be cut.
    pub fn whole(&self) -> Option<Vec<u8>> {
        if self.total != self.head.len() + self.tail.len() {
            return None;
        }
        let mut all = self.head.clone();
        all.extend(self.tail.iter());
        Some(all)
    }

    /// The output for a reply: whole if it is short, else its two ends.
    pub fn text(&self) -> String {
        self.render().0
    }

    /// The output for a reply, and whether any of it was left out (trailing whitespace
    /// trimmed from a whole output doesn't count).
    pub fn render(&self) -> (String, bool) {
        let tail: Vec<u8> = self.tail.iter().copied().collect();
        if self.total == self.head.len() + tail.len() {
            let mut all = self.head.clone();
            all.extend_from_slice(&tail);
            let s = String::from_utf8_lossy(&all);
            let s = s.trim_end();
            return (clip(s, OUTPUT_KEEP), s.chars().count() > 2 * OUTPUT_KEEP);
        }
        let head = String::from_utf8_lossy(&self.head);
        let tail = String::from_utf8_lossy(&tail);
        let head: String = head.chars().take(OUTPUT_KEEP).collect();
        let n = tail.chars().count();
        let tail: String = tail.chars().skip(n.saturating_sub(OUTPUT_KEEP)).collect();
        (
            format!(
                "{head}\n[... middle omitted: {} bytes of output in all ...]\n{}",
                self.total,
                tail.trim_end()
            ),
            true,
        )
    }
}

/// Keep the first and last `keep` characters of `s`, noting how much was cut.
pub fn clip(s: &str, keep: usize) -> String {
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

pub async fn drain_ends<R: tokio::io::AsyncRead + Unpin>(mut r: R) -> Captured {
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

/// Whether the wrapper's start marker came through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Start {
    /// The payload was about to start (its identity was recorded first).
    Seen,
    /// The stream ended without it: the payload never started.
    Absent,
    /// Not known (the stream was cut off, or too much came before it).
    Unknown,
}

/// One stream being read, with the start marker taken out of it when `marker` is set.
#[derive(Debug, Default)]
pub(crate) struct Stream {
    captured: Captured,
    marker: Option<Vec<u8>>,
    pending: Vec<u8>,
    start: Option<Start>,
}

impl Stream {
    pub(crate) fn with_marker(marker: Vec<u8>) -> Self {
        Self {
            marker: Some(marker),
            ..Self::default()
        }
    }

    pub(crate) fn feed(&mut self, chunk: &[u8]) {
        let Some(marker) = &self.marker else {
            return self.captured.push(chunk);
        };
        if self.start.is_some() {
            return self.captured.push(chunk);
        }
        self.pending.extend_from_slice(chunk);
        if let Some(at) = find(&self.pending, marker) {
            let after = self.pending.split_off(at + marker.len());
            self.pending.truncate(at);
            self.captured.push(&self.pending);
            self.captured.push(&after);
            self.pending = Vec::new();
            self.start = Some(Start::Seen);
        } else if self.pending.len() > SCAN_LIMIT {
            let pending = std::mem::take(&mut self.pending);
            self.captured.push(&pending);
            self.start = Some(Start::Unknown);
        }
    }

    /// The stream is over: `complete` when it reached its end, not when it was cut off.
    pub(crate) fn finish(&mut self, complete: bool) -> (Captured, Start) {
        let pending = std::mem::take(&mut self.pending);
        self.captured.push(&pending);
        let start = match (self.start, complete) {
            (Some(s), _) => s,
            (None, true) => Start::Absent,
            (None, false) => Start::Unknown,
        };
        (std::mem::take(&mut self.captured), start)
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The line a wrapper writes to stderr just before the payload starts.
pub fn start_marker(id: u64) -> Vec<u8> {
    format!("\x1eagentpc-started:{id}\n").into_bytes()
}

async fn drain_into<R: tokio::io::AsyncRead + Unpin>(mut r: R, s: Arc<Mutex<Stream>>) {
    use tokio::io::AsyncReadExt;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        s.lock().unwrap().feed(&buf[..n]);
    }
}

// ---------------------------------------------------------------------------------------------
// The host side of a foreground run

/// Result of the guest-side cancellation of a timed-out execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cancel {
    /// None of the execution's identified processes is left. `exited`: the payload's own
    /// recorded exit code, when it had already finished and only what it left running was
    /// stopped.
    Stopped { exited: Option<i64> },
    /// It had not started, and now never will.
    NotStarted,
    /// Not verified; why.
    Unconfirmed(String),
}

impl Cancel {
    /// Judges the stop script's run in cancel mode by its exit code (see `linux_stop_script`),
    /// plus its one machine line `AGENTPC_EXITED <code>` when the payload had already exited.
    pub fn from_stop(code: Option<i32>, timed_out: bool, stdout: &str, deadline: u64) -> Self {
        let said = || {
            let s = stdout.trim();
            if s.is_empty() {
                String::new()
            } else {
                format!(": {s}")
            }
        };
        match (timed_out, code) {
            (true, _) => Cancel::Unconfirmed(format!("stopping it took over {deadline}s")),
            (false, Some(0)) => Cancel::Stopped {
                exited: stdout.lines().find_map(|l| {
                    l.trim()
                        .strip_prefix("AGENTPC_EXITED ")
                        .and_then(|c| c.trim().parse().ok())
                }),
            },
            (false, Some(4)) => Cancel::NotStarted,
            (false, Some(1)) => {
                Cancel::Unconfirmed(format!("some of its processes would not stop{}", said()))
            }
            (false, Some(c)) => {
                Cancel::Unconfirmed(format!("the stop ended with code {c}{}", said()))
            }
            (false, None) => Cancel::Unconfirmed("the stop's SSH session was killed".into()),
        }
    }
}

/// What the VM's records say about an execution after its connection failed. Checking also
/// makes sure one that hasn't started never does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Probe {
    /// It ran to its end with this code, as the VM recorded it: on Windows PowerShell's full
    /// code (`exit -1` is -1 or 4294967295), of which an SSH client sees the low 8 bits.
    Exited(i64),
    /// Still running (its recorded root process, by pid and start time).
    Running,
    /// Started, no longer running, no exit code recorded (killed, or on Windows an explicit
    /// `exit`).
    Gone,
    /// It never started, and now never will.
    NotStarted,
    /// The VM couldn't be checked; why.
    Failed(String),
}

impl Probe {
    /// Parses the `AGENTPC_PROBE <state> [code]` line the probe scripts print.
    pub fn parse(stdout: &str) -> Self {
        let Some(line) = stdout
            .lines()
            .find_map(|l| l.trim().strip_prefix("AGENTPC_PROBE "))
        else {
            return Probe::Failed("the check printed no result".into());
        };
        let mut words = line.split_whitespace();
        match (words.next(), words.next()) {
            (Some("exited"), Some(code)) => code
                .parse()
                .map(Probe::Exited)
                .unwrap_or_else(|_| Probe::Failed(format!("unreadable exit code {code:?}"))),
            (Some("running"), _) => Probe::Running,
            (Some("gone"), _) => Probe::Gone,
            (Some("not_started"), _) => Probe::NotStarted,
            _ => Probe::Failed(format!("unexpected check result {line:?}")),
        }
    }
}

/// Everything observed while running one foreground command, before it is judged.
#[derive(Debug)]
pub struct Facts {
    /// The SSH process's exit code; None when it was killed or died of a signal (or never
    /// ended on its own).
    pub code: Option<i32>,
    pub timed_out: bool,
    pub start: Start,
    pub cancel: Option<Cancel>,
    pub probe: Option<Probe>,
    pub stdout: Captured,
    pub stderr: Captured,
}

/// Exit code ssh reports when the connection itself failed.
const SSH_FAILED: i32 = 255;

/// A recorded exit code as an SSH client reports it: its low 8 bits.
fn ssh_code(recorded: i64) -> i32 {
    (recorded & 0xff) as i32
}

/// Whether `code` is one the wrappers exit with before starting the payload.
fn is_wrapper_code(code: i32) -> bool {
    code == WRAPPER_SETUP_FAILED || code == WRAPPER_CANCELLED
}

/// Runs `cmd` (ssh running a tracked wrapper, in production) for up to `timeout`. At the
/// timeout, `cancel` stops the execution in the guest (bounded by `limits.cancel`), then the
/// local process gets a short grace to end before it is killed. When the run ends without a
/// clear answer (ssh's own failure code, a signal, an unknown start), `probe` asks the guest's
/// records (bounded by `limits.probe`). Output reading is bounded too: whatever arrived is
/// kept even if something still holds the pipes open.
pub async fn run_foreground<C, CF, P, PF>(
    mut cmd: tokio::process::Command,
    id: u64,
    timeout: Duration,
    limits: Limits,
    cancel: C,
    probe: P,
) -> std::io::Result<Facts>
where
    C: FnOnce() -> CF,
    CF: Future<Output = Cancel>,
    P: FnOnce() -> PF,
    PF: Future<Output = Probe>,
{
    use std::process::Stdio;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    // The marker goes to stderr; stdout is searched too, in case a guest's SSH server merges
    // the streams: either way it never reaches the reply.
    let out = Arc::new(Mutex::new(Stream::with_marker(start_marker(id))));
    let err = Arc::new(Mutex::new(Stream::with_marker(start_marker(id))));
    let mut read_out = tokio::spawn(drain_into(child.stdout.take().unwrap(), out.clone()));
    let mut read_err = tokio::spawn(drain_into(child.stderr.take().unwrap(), err.clone()));

    let waited = tokio::time::timeout(timeout, child.wait()).await;
    let timed_out = waited.is_err();
    // A failed wait (not a timeout) leaves no status: judged like a signal, by the VM's record.
    let mut status = waited.ok().and_then(Result::ok);
    let mut cancelled = None;
    if timed_out {
        // Stop it in the guest while its session still carries output here; once its
        // processes are gone the guest ends the session and the rest of its output arrives.
        let c = tokio::time::timeout(limits.cancel, cancel())
            .await
            .unwrap_or_else(|_| {
                Cancel::Unconfirmed(format!(
                    "the VM did not answer within {}s",
                    limits.cancel.as_secs()
                ))
            });
        cancelled = Some(c);
        status = tokio::time::timeout(limits.session_grace, child.wait())
            .await
            .ok()
            .and_then(Result::ok);
        if status.is_none() {
            let _ = child.kill().await;
        }
    }
    // ssh is gone, so its pipes are closed; bound the reads anyway (a process it left
    // behind could hold them).
    let complete = tokio::time::timeout(limits.drain_grace, async {
        let _ = (&mut read_out).await;
        let _ = (&mut read_err).await;
    })
    .await
    .is_ok();
    if !complete {
        read_out.abort();
        read_err.abort();
    }
    let (stdout, on_out) = out.lock().unwrap().finish(complete);
    let (stderr, on_err) = err.lock().unwrap().finish(complete);
    let start = match (on_out, on_err) {
        (Start::Seen, _) | (_, Start::Seen) => Start::Seen,
        (Start::Absent, Start::Absent) => Start::Absent,
        _ => Start::Unknown,
    };
    // A timed-out run's status is the kill's, not the command's.
    let code = if timed_out {
        None
    } else {
        status.and_then(|s| s.code())
    };
    // Settled without asking the VM: the command's own exit code after the marker, or the
    // wrapper's own code from before the payload, with no marker. Anything else (ssh's failure
    // code, a signal, no marker with another code) is checked against the VM's record.
    let settled = match (start, code) {
        (Start::Seen, Some(c)) => c != SSH_FAILED,
        (Start::Absent, Some(c)) => is_wrapper_code(c),
        _ => false,
    };
    let needs_probe = !timed_out && !settled;
    let probed = if needs_probe {
        Some(
            tokio::time::timeout(limits.probe, probe())
                .await
                .unwrap_or_else(|_| {
                    Probe::Failed(format!(
                        "the VM did not answer within {}s",
                        limits.probe.as_secs()
                    ))
                }),
        )
    } else {
        None
    };
    Ok(Facts {
        code,
        timed_out,
        start,
        cancel: cancelled,
        probe: probed,
        stdout,
        stderr,
    })
}

/// Judges a foreground run: its outcome and legacy text. `name` is the VM (for the hints),
/// `timeout` the seconds asked for.
pub fn conclude(f: Facts, id: u64, name: &str, timeout: u64) -> Reply {
    use ExecutionState as S;
    let base = CommandOutcome::new(S::Exited, Some(id)).with_output(&f.stdout, &f.stderr);
    let output = sections(&base.stdout, &base.stderr);
    let handles = format!(
        "Check it with get_job_status name={name} id={id}; stop it with stop_job name={name} id={id}."
    );
    let mut o = base;
    let head: String;

    if f.timed_out {
        o.timed_out = true;
        match f
            .cancel
            .unwrap_or(Cancel::Unconfirmed("no cancellation ran".into()))
        {
            // The VM recorded the command's exit code, but the run hadn't ended by the
            // deadline: something it left may have held its output, or it ended right at the
            // timeout. The cleanup can't tell those apart, so the reply says only what is
            // proven.
            Cancel::Stopped { exited: Some(rec) } => {
                let code = ssh_code(rec);
                o.exit_code = Some(code);
                o.termination_confirmed = Some(true);
                head = format!(
                    "exit code: {code} (the VM recorded this exit code, but the run had not \
                     ended by the {timeout}s timeout; agentpc's cleanup then verified that none \
                     of the processes it identified is left (execution id {id}). To keep a \
                     server running, start it with background: true.)"
                );
            }
            Cancel::Stopped { exited: None } => {
                o.state = S::TimedOut;
                o.termination_confirmed = Some(true);
                head = format!(
                    "timed out after {timeout}s. agentpc stopped the command and every process it \
                     started that it could identify (execution id {id}). Use background: true for \
                     long jobs.\n(partial output below)"
                );
            }
            Cancel::NotStarted => {
                o.state = S::NotStarted;
                o.recovery_action = Some(RecoveryAction::Retry);
                o.error = Some(format!(
                    "timed out after {timeout}s before the command started"
                ));
                head = format!(
                    "timed out after {timeout}s before the command started; agentpc made sure it \
                     never will (execution id {id})."
                );
            }
            Cancel::Unconfirmed(why) => {
                o.state = S::TimedOut;
                o.termination_confirmed = Some(false);
                o.effect_unknown = true;
                o.job_id = Some(id);
                o.recovery_action = Some(RecoveryAction::StopJob);
                o.error = Some(format!("could not confirm the command stopped: {why}"));
                head = format!(
                    "timed out after {timeout}s. agentpc could not confirm the command stopped \
                     ({why}), so it may still be running in the VM (execution id {id}). {handles} \
                     Use background: true for long jobs.\n(partial output below)"
                );
            }
        }
        return Reply {
            outcome: o,
            text: head + &output,
        };
    }

    let clean = f.code.filter(|&c| c != SSH_FAILED);
    match (f.start, clean, f.probe) {
        // The common case: it started and its exit code came back.
        (Start::Seen, Some(code), _) => {
            o.exit_code = Some(code);
            head = format!("exit code: {code}");
        }
        // The wrapper ended cleanly before the payload: it never ran.
        (Start::Absent, Some(code), _) if is_wrapper_code(code) => {
            o.state = S::NotStarted;
            let why = match code {
                WRAPPER_CANCELLED => "it was cancelled (stop_job) before it started".to_string(),
                _ => format!(
                    "agentpc's command wrapper exited with code {code} before the command started"
                ),
            };
            o.recovery_action = (code != WRAPPER_CANCELLED).then_some(RecoveryAction::Retry);
            head = format!("the command did not run: {why} (execution id {id}).");
            o.error = Some(why);
        }
        (start, _, probe) => {
            let ssh = match f.code {
                Some(c) => format!("ssh exited with code {c}"),
                None => "ssh ended without an exit code".to_string(),
            };
            match probe.unwrap_or(Probe::Failed("no check ran".into())) {
                // ssh's 255 was the command's own code (on Windows also `exit -1`, `exit 511`,
                // ...): its record agrees on what ssh sees, the low 8 bits.
                Probe::Exited(rec) if f.code == Some(ssh_code(rec)) => {
                    let code = ssh_code(rec);
                    o.exit_code = Some(code);
                    head = format!("exit code: {code}");
                }
                Probe::Exited(rec) => {
                    let code = ssh_code(rec);
                    o.state = S::ConnectionLost;
                    o.exit_code = Some(code);
                    o.error = Some(format!(
                        "the connection to {name} failed ({ssh}); the command had finished with \
                         code {code}, but its output may be incomplete"
                    ));
                    head = format!(
                        "exit code: {code} (from the VM's record: the connection failed, {ssh}, so \
                         the output below may be incomplete; execution id {id})"
                    );
                }
                Probe::NotStarted if start != Start::Seen => {
                    o.state = S::NotStarted;
                    o.recovery_action = Some(RecoveryAction::Retry);
                    o.error = Some(format!(
                        "the connection to {name} failed ({ssh}) before the command started; it \
                         never ran and now never will"
                    ));
                    head = format!(
                        "the command did not run: the connection to {name} failed ({ssh}) before it \
                         started, and agentpc made sure it never will (execution id {id})."
                    );
                }
                Probe::Running => {
                    o.state = S::ConnectionLost;
                    o.effect_unknown = true;
                    o.job_id = Some(id);
                    o.recovery_action = Some(RecoveryAction::GetJobStatus);
                    o.error = Some(format!(
                        "the connection to {name} failed ({ssh}) while the command was running; \
                         it is still running"
                    ));
                    head = format!(
                        "the connection to {name} failed ({ssh}) while the command ran; it is still \
                         running in the VM (execution id {id}). {handles}"
                    );
                }
                Probe::Gone | Probe::NotStarted => {
                    o.state = S::ConnectionLost;
                    o.effect_unknown = true;
                    o.job_id = Some(id);
                    o.recovery_action = Some(RecoveryAction::InspectVm);
                    // 255 is also what a command's own `exit 255` (or Windows' `exit -1`) looks
                    // like when the VM couldn't record the code: say both.
                    let also = if start == Start::Seen && f.code == Some(SSH_FAILED) {
                        " (or the command itself exited with code 255, which the VM could not \
                         record)"
                    } else {
                        ""
                    };
                    o.error = Some(format!(
                        "the connection to {name} failed ({ssh}){also}; the command is no longer \
                         running but recorded no exit code, so whether it finished is unknown"
                    ));
                    head = format!(
                        "the connection to {name} failed ({ssh}){also}; the command is no longer \
                         running but recorded no exit code, so whether it finished is unknown \
                         (execution id {id}). Check the VM's state before running it again."
                    );
                }
                Probe::Failed(why) => {
                    o.state = S::ConnectionLost;
                    o.effect_unknown = true;
                    o.job_id = Some(id);
                    o.recovery_action = Some(RecoveryAction::GetJobStatus);
                    o.error = Some(format!(
                        "the connection to {name} failed ({ssh}) and the VM could not be checked \
                         afterwards ({why})"
                    ));
                    head = format!(
                        "the connection to {name} failed ({ssh}) and agentpc could not check the VM \
                         afterwards ({why}), so the command may or may not have run (execution id \
                         {id}). Once the VM answers: {handles} If get_job_status says there is no \
                         such job, it never recorded itself; stop_job then makes sure it never \
                         starts."
                    );
                }
            }
        }
    }
    Reply {
        outcome: o,
        text: head + &output,
    }
}

/// A background start: `started` with where its output goes, and the legacy text.
pub fn background_started(id: u64, text: String, log_path: Option<String>) -> Reply {
    let mut o = CommandOutcome::new(ExecutionState::Started, Some(id));
    o.background = true;
    o.job_id = Some(id);
    o.log_path = log_path;
    o.recovery_action = Some(RecoveryAction::GetJobStatus);
    Reply { outcome: o, text }
}

/// A background start that failed. `uncertain` when the launch's connection failed, so the job
/// may have started anyway.
pub fn background_failed(id: u64, text: String, uncertain: bool, name: &str) -> Reply {
    let mut o = CommandOutcome::new(
        if uncertain {
            ExecutionState::ConnectionLost
        } else {
            ExecutionState::NotStarted
        },
        Some(id),
    );
    o.background = true;
    o.error = Some(text.clone());
    let text = if uncertain {
        o.effect_unknown = true;
        o.job_id = Some(id);
        o.recovery_action = Some(RecoveryAction::GetJobStatus);
        format!(
            "{text}\nThe connection failed while starting the job, so it may have started anyway \
             (id {id}): check with get_job_status name={name} id={id}."
        )
    } else {
        o.recovery_action = Some(RecoveryAction::Retry);
        text
    };
    Reply { outcome: o, text }
}

// ---------------------------------------------------------------------------------------------
// Guest-side wrappers

/// The most command text a Linux foreground run takes, in UTF-8 bytes: 92 KiB. The command
/// goes base64-encoded (4/3 its size) into the wrapper, which is one SSH command line, and then
/// whole into an environment string and the payload shell's `-c` argument. Linux caps each
/// argument or environment string at 128 KiB (MAX_ARG_STRLEN); 92 KiB leaves room for the
/// base64 growth and the wrapper around it. Background jobs have their own, separate limit
/// (their command is base64 in the launch's command line only).
pub const LINUX_FOREGROUND_MAX_BYTES: usize = 92 * 1024;

/// A Linux foreground command over `LINUX_FOREGROUND_MAX_BYTES`, refused before anything is
/// sent or recorded: `not_started` without an execution id, and no recovery action, since the
/// same command would fail the same way.
pub fn linux_foreground_too_large(command: &str) -> Option<Reply> {
    let n = command.len();
    (n > LINUX_FOREGROUND_MAX_BYTES).then(|| {
        Reply::not_started(
            format!(
                "the command is {n} bytes of text; a Linux foreground run_command takes at most \
                 {LINUX_FOREGROUND_MAX_BYTES} bytes (92 KiB, counted as UTF-8), so nothing was \
                 sent to the VM. Save the script to a file, upload_file it to the VM and run that \
                 file (for example `bash /home/agent/script.sh`)."
            ),
            None,
        )
    })
}

/// The bash a Linux foreground command runs as (over SSH, in the agent's login shell), for
/// execution `id`. It records the execution before the payload starts and reports the
/// payload's own exit code:
/// - `<id>.fg` marks a foreground execution; the payload itself is never written to disk: it
///   travels base64-encoded in this script, then in the wrapper's environment (unset before
///   the payload runs), and runs as `bash -c "<payload>"` like an SSH command always has;
/// - in the job's systemd scope when the user manager's bus is up (else a plain child), the
///   wrapper writes its pid to `<id>.pid` and its boot id and start time to `<id>.proof`;
/// - only then it checks for `<id>.cancel` (written by a cancellation or a check that found
///   no record: it never starts after one), writes the start marker to stderr, runs the
///   payload with the desktop session's env, and records its code in `<id>.exit`.
///
/// If the scope can't be made, the wrapper never recorded itself and runs again without one
/// (the payload hasn't run, so this repeats no payload code).
pub fn linux_foreground(id: u64, command: &str, session_env: &str) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(command);
    // Runs as `bash -c "$i" agentpc-job-<id> <dir>` with the payload in AGENTPC_CMD; no
    // single quotes, it is quoted in them.
    let inner = format!(
        r#"d=$1; cmd=$AGENTPC_CMD; unset AGENTPC_CMD
read -r b < /proc/sys/kernel/random/boot_id
read -r s < /proc/$$/stat; s=${{s##*) }}; set -- $s
echo $$ > $d/{id}.pid && echo "$b ${{20}}" > $d/{id}.proof || exit {WRAPPER_SETUP_FAILED}
[ -e $d/{id}.cancel ] && exit {WRAPPER_CANCELLED}
printf "\036agentpc-started:{id}\n" >&2
bash -c "$cmd"; c=$?
echo $c > $d/{id}.exit
exit $c"#
    );
    format!(
        r#"export {session_env}
d=~/agentpc-bg; i='{inner}'
cmd=$(echo {b64} | base64 -d && echo .) && cmd=${{cmd%.}} && {{ [ -d $d ] || mkdir -p $d; }} && : > $d/{id}.fg || exit {WRAPPER_SETUP_FAILED}
if [ $((RANDOM % 32)) = 0 ]; then ( exec >/dev/null 2>&1 </dev/null; {gc} ) & fi
run=; if [ -S /run/user/1000/bus ]; then run='systemd-run --user --scope --quiet --collect --unit=agentpc-job-{id}'; {literal}; run="$run --"; fi
AGENTPC_JOB={id} AGENTPC_CMD="$cmd" $run bash -c "$i" agentpc-job-{id} $d; c=$?
if [ -n "$run" ] && [ ! -e $d/{id}.pid ]; then AGENTPC_JOB={id} AGENTPC_CMD="$cmd" bash -c "$i" agentpc-job-{id} $d; c=$?; fi
exit $c"#,
        gc = LINUX_GC,
        literal = SCOPE_LITERAL,
    )
}

/// Appends `--expand-environment=no` to the systemd-run command in `$run` when this guest's
/// systemd-run accepts it, so the scope's command line reaches bash exactly as written.
/// systemd-run expands `$$`, `${NAME}` and a lone `$NAME` argument in the command line
/// itself; for a scope, systemd 254 to 257 left that off by default, and 258 turned it on (its
/// NEWS), which broke the wrappers' `$$` and `${20}` (Arch). The switch only exists from 254
/// on, and an older systemd-run (Ubuntu 22.04's 249) rejects it but never expands a scope's
/// line: asking this systemd-run to parse it (`--version` then ends the parse) decides, so
/// neither a release number nor a default is assumed and an old guest keeps its scope.
pub(crate) const SCOPE_LITERAL: &str = r#"systemd-run --expand-environment=no --version >/dev/null 2>&1 && run="$run --expand-environment=no""#;

/// The bash a Linux background job is launched with (over SSH, in the agent's login shell),
/// for job `id` whose script is `b64` (base64). Separate lines: `a && b &` would background the
/// whole list, and that shell would hold the SSH session open. The wrapper records the exit
/// code in <id>.exit so get_job_status can report it after the job ends. The job runs in its
/// own systemd scope (a cgroup no process can leave, even one that clears its environment and
/// detaches), so stop_job finds everything it started; AGENTPC_JOB, which every process it
/// starts inherits, covers a guest without a user systemd. <id>.proof (boot id and start time)
/// lets get_job_status and stop_job tell the wrapper from a process that got its pid later. It
/// prints the wrapper's pid and the log's path for the reply.
pub fn linux_background(id: u64, b64: &str, session_env: &str) -> String {
    format!(
        "d=~/agentpc-bg; mkdir -p $d && echo {b64} | base64 -d > $d/{id}.sh || exit 1\n\
         run=; if {env} systemctl --user show-environment >/dev/null 2>&1; then \
         run='systemd-run --user --scope --quiet --unit=agentpc-job-{id}'; {literal}; run=\"$run --\"; fi\n\
         {env} AGENTPC_JOB={id} setsid nohup $run bash -c 'bash \"$0\"; echo $? > \"$1\"' \
         $d/{id}.sh $d/{id}.exit > $d/{id}.log 2>&1 < /dev/null &\n\
         j=$!; echo $j > $d/{id}.pid\n\
         echo \"$(cat /proc/sys/kernel/random/boot_id) $(sed 's/.*) //' /proc/$j/stat 2>/dev/null | awk '{{ print $20 }}')\" > $d/{id}.proof\n\
         echo $j; echo $d/{id}.log",
        env = session_env,
        literal = SCOPE_LITERAL,
    )
}

/// Now and then, a foreground wrapper drops day-old records of foreground executions that
/// are over (exited, stopped, or never started) and day-old tombstones; background jobs' (with
/// a log) are kept.
pub(crate) const LINUX_GC: &str = r#"find $d -maxdepth 1 \( -name '*.fg' -o -name '*.cancel' \) -mmin +1440 | while read -r f; do j=${f%.*}; [ -f $j.log ] && continue; if [ ! -f $j.fg ] || [ -f $j.exit ] || [ -f $j.stopped ] || [ ! -f $j.pid ]; then rm -f $j.sh $j.fg $j.pid $j.proof $j.exit $j.stopped $j.cancel; fi; done"#;

/// The PowerShell a Windows foreground command runs as, in the SSH session's PowerShell.
/// Like `ops::windows_command` (base64 UTF-8, dot-sourced, `-Command` exit semantics), plus:
/// first `<id>.fg`, and `<id>.pid` with this PowerShell's pid and creation time (100 ns),
/// then the `<id>.cancel` check and the start marker on stderr; after a payload that didn't
/// `exit` itself, its code in `<id>.exit`.
pub fn windows_foreground(id: u64, script: &str) -> String {
    let enc = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
    let prelude = format!(
        r#"try {{
  $__agentpc_d = "$env:USERPROFILE\agentpc-bg"
  if (-not [IO.Directory]::Exists($__agentpc_d)) {{ [void][IO.Directory]::CreateDirectory($__agentpc_d) }}
  try {{ if (([Environment]::TickCount -band 31) -eq 0) {{ {gc} }} }} catch {{ }}
  [IO.File]::WriteAllText("$__agentpc_d\{id}.fg", '')
  $__agentpc_me = [Diagnostics.Process]::GetCurrentProcess()
  [IO.File]::WriteAllText("$__agentpc_d\{id}.pid", "$($__agentpc_me.Id) $($__agentpc_me.StartTime.ToFileTimeUtc())")
}} catch {{ [Console]::Error.Write("agentpc could not record this command: $_`n"); exit {WRAPPER_SETUP_FAILED} }}
if ([IO.File]::Exists("$__agentpc_d\{id}.cancel")) {{ exit {WRAPPER_CANCELLED} }}
{exiting}
[Console]::Error.Write([string][char]30 + "agentpc-started:{id}`n")"#,
        gc = WINDOWS_GC,
        exiting = windows_exit_record(id),
    );
    let body = format!("{script}\n$__agentpc_ok = $?");
    format!(
        "[Console]::OutputEncoding = [Text.Encoding]::UTF8; \
         . ([scriptblock]::Create([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{}')))); \
         . ([scriptblock]::Create([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{}')))); \
         $__agentpc_c = 0; if (-not $__agentpc_ok) {{ $__agentpc_c = 1 }}; \
         try {{ [IO.File]::WriteAllText(\"$__agentpc_d\\{id}.exit\", \"$__agentpc_c\") }} catch {{ }}; \
         exit $__agentpc_c",
        enc(&prelude),
        enc(&body),
    )
}

/// An explicit `exit N` in the payload ends PowerShell before the wrapper writes `<id>.exit`,
/// and neither try/finally (it can't see N, and inside `try` a statement-terminating error
/// would skip the rest of the payload) nor `trap` can intercept it. PowerShell's own Exiting
/// event does run then: its action reads the exit code the console host is about to return
/// and records it, unless the wrapper already wrote one. `-SupportEvent` keeps the
/// subscription and its job out of the payload's `Get-EventSubscriber` / `Get-Job`. The code
/// is read from the host's private fields (`_exitCodeFromRunspace` and `_setShouldExitCalled`
/// in PowerShell 7, the same names without the underscore in Windows PowerShell 5.1); when
/// they can't be read, nothing is recorded and a 255 from ssh stays reported as ambiguous.
/// `[Environment]::Exit()` skips the event, as does a killed PowerShell.
fn windows_exit_record(id: u64) -> String {
    format!(
        r#"try {{ Register-EngineEvent -SourceIdentifier PowerShell.Exiting -SupportEvent -Action {{
  try {{
    $p = "$env:USERPROFILE\agentpc-bg\{id}.exit"
    if (-not [IO.File]::Exists($p)) {{
      $f = [Reflection.BindingFlags]'NonPublic,Public,Instance'
      $x = $Host.GetType().GetProperty('ExternalHost', $f).GetValue($Host)
      $t = $x.GetType(); $called = $null; $code = $null
      foreach ($n in '_setShouldExitCalled', 'setShouldExitCalled') {{ $i = $t.GetField($n, $f); if ($i) {{ $called = $i.GetValue($x) }} }}
      foreach ($n in '_exitCodeFromRunspace', 'exitCodeFromRunspace') {{ $i = $t.GetField($n, $f); if ($i) {{ $code = $i.GetValue($x) }} }}
      if ($called -eq $true -and $null -ne $code) {{ [IO.File]::WriteAllText($p, [string][int64]$code) }}
    }}
  }} catch {{ }}
}} }} catch {{ }}"#
    )
}

/// Windows's counterpart of LINUX_GC.
const WINDOWS_GC: &str = r#"$__agentpc_old = (Get-Date).AddDays(-1); Get-ChildItem -LiteralPath $__agentpc_d -Filter *.fg -ErrorAction SilentlyContinue | Where-Object { $_.LastWriteTime -lt $__agentpc_old } | ForEach-Object { $j = $_.FullName -replace '\.fg$', ''; if (-not (Test-Path -LiteralPath "$j.log") -and ((Test-Path -LiteralPath "$j.exit") -or (Test-Path -LiteralPath "$j.stopped") -or -not (Test-Path -LiteralPath "$j.pid"))) { Remove-Item -LiteralPath "$j.fg", "$j.pid", "$j.exit", "$j.stopped", "$j.cancel" -ErrorAction SilentlyContinue } }; Get-ChildItem -LiteralPath $__agentpc_d -Filter *.cancel -ErrorAction SilentlyContinue | Where-Object { $_.LastWriteTime -lt $__agentpc_old } | ForEach-Object { $j = $_.FullName -replace '\.cancel$', ''; if (-not (Test-Path -LiteralPath "$j.fg") -and -not (Test-Path -LiteralPath "$j.log") -and -not (Test-Path -LiteralPath "$j.pid")) { Remove-Item -LiteralPath $_.FullName -ErrorAction SilentlyContinue } }"#;

/// The check after a failed connection (bash): writes `<id>.cancel` first, so an execution
/// that hasn't recorded itself yet never starts, then reports one `AGENTPC_PROBE` line.
pub fn linux_probe(id: u64) -> String {
    format!(
        r#"d=~/agentpc-bg
mkdir -p $d && : > $d/{id}.cancel || {{ echo 'AGENTPC_PROBE error'; exit 0; }}
if [ -f $d/{id}.exit ]; then echo "AGENTPC_PROBE exited $(cat $d/{id}.exit)"; exit 0; fi
p=$(cat $d/{id}.pid 2>/dev/null); case $p in ''|*[!0-9]*) echo 'AGENTPC_PROBE not_started'; exit 0 ;; esac
{own}
if own; then echo 'AGENTPC_PROBE running'; else echo 'AGENTPC_PROBE gone'; fi"#,
        own = linux_own(id),
    )
}

/// Windows's counterpart of linux_probe.
pub fn windows_probe(id: u64) -> String {
    format!(
        r#"$d = "$env:USERPROFILE\agentpc-bg"
try {{ [void][IO.Directory]::CreateDirectory($d); [IO.File]::WriteAllText("$d\{id}.cancel", '') }} catch {{ 'AGENTPC_PROBE error'; exit 0 }}
if ([IO.File]::Exists("$d\{id}.exit")) {{ "AGENTPC_PROBE exited $([IO.File]::ReadAllText("$d\{id}.exit").Trim())"; exit 0 }}
if (-not [IO.File]::Exists("$d\{id}.pid")) {{ 'AGENTPC_PROBE not_started'; exit 0 }}
{own}
if (Own) {{ 'AGENTPC_PROBE running' }} else {{ 'AGENTPC_PROBE gone' }}"#,
        own = windows_own(id),
    )
}

/// Shell functions `st <pid>` (a process's start time, field 22 of /proc/<pid>/stat) and
/// `own`: whether `$p` (read from `<id>.pid` by the caller) is still the recorded process.
/// With a `<id>.proof` the boot id and start time must match, so a reused pid (or one from
/// before a reboot) never counts; a background job from an agentpc without proofs falls back
/// to its command line naming `<id>.sh`.
pub fn linux_own(id: u64) -> String {
    format!(
        r#"st() {{ sed 's/.*) //' /proc/$1/stat 2>/dev/null | awk '{{ print $20 }}'; }}
own() {{
  [ -n "$p" ] || return 1
  if [ -f $d/{id}.proof ]; then
    read -r b s < $d/{id}.proof
    [ -n "$s" ] && [ "$b" = "$(cat /proc/sys/kernel/random/boot_id)" ] && [ "$(st "$p")" = "$s" ]
  else grep -qs {id}.sh /proc/$p/cmdline; fi
}}"#
    )
}

/// PowerShell function `Own`: whether the process recorded in `<id>.pid` (pid and creation
/// time) is still running, without a WMI query.
pub fn windows_own(id: u64) -> String {
    format!(
        r#"function Own {{
  $rec = "$(try {{ [IO.File]::ReadAllText("$d\{id}.pid") }} catch {{ }})".Trim() -split ' '
  if ($rec.Count -ne 2 -or $rec[0] -notmatch '^\d+$' -or $rec[1] -notmatch '^\d+$') {{ return $false }}
  $pr = Get-Process -Id ([int]$rec[0]) -ErrorAction SilentlyContinue
  if (-not $pr) {{ return $false }}
  try {{ [math]::Abs($pr.StartTime.ToFileTimeUtc() - [int64]$rec[1]) -lt 10 }} catch {{ $false }}
}}"#
    )
}

// ---------------------------------------------------------------------------------------------
// Job scripts (stop_job and the foreground cancellation; get_job_status's are in `jobs`)

/// stop_job's bash for job `id`, or (`cancel`) the cancellation of a timed-out foreground
/// execution. Exit codes: 0 when none of the job's identified processes is left (stopped,
/// already exited, or not running), 1 when some would not stop, 3 for no such job, and in
/// cancel mode 4 when the execution never recorded itself: the `<id>.cancel` written first
/// means it now never starts. A foreground execution (`<id>.fg`) gets `<id>.cancel` from
/// stop_job too.
pub fn linux_stop_script(id: u64, cancel: bool) -> String {
    let why = if cancel { "timeout" } else { "stop_job" };
    // The job's processes: everything in its systemd scope (agentpc-job-<id>.scope, a
    // cgroup only the job's processes are in), its recorded wrapper and that wrapper's
    // session while it is provably the same process (boot id and start time in
    // <id>.proof; an older job's, its command line), and
    // every process carrying its
    // AGENTPC_JOB marker plus their descendants (sudo resets the environment, so what
    // sudo starts is found as a child). A session id alone is not trusted once the
    // wrapper is gone: after a reboot or pid reuse it can name someone else's session.
    // Each round rescans, so a pid that exits and is reused is never signalled; .stopped
    // is written only once nothing is left. root (sudo) reads every environment and can
    // signal what the job started as root.
    format!(
        r#"d=~/agentpc-bg
{prefix}[ -f $d/{id}.log ] || [ -f $d/{id}.fg ] || {{ mkdir -p $d && : > $d/{id}.cancel; echo 'no such job {id} (nothing has recorded itself under that id; a foreground run_command given this id can no longer start)'; exit 3; }}
[ -f $d/{id}.fg ] && : > $d/{id}.cancel
S=; sudo -n true 2>/dev/null && S='sudo -n'
p=$(cat $d/{id}.pid 2>/dev/null); case $p in ''|*[!0-9]*) p= ;; esac
{own}
cg=$(find /sys/fs/cgroup/user.slice -maxdepth 6 -type d -name agentpc-job-{id}.scope 2>/dev/null | head -n 1)
members() {{
  {{
    [ -n "$cg" ] && find "$cg" -name cgroup.procs -exec cat {{}} + 2>/dev/null
    if own; then echo "$p"; pgrep -s "$p"; fi
    marked=$($S grep -lsxz 'AGENTPC_JOB={id}' /proc/[0-9]*/environ 2>/dev/null | sed 's|^/proc/||; s|/environ$||')
    if [ -n "$marked" ]; then
      ps -eo pid=,ppid= | awk -v seed="$(echo $marked)" '
        BEGIN {{ n = split(seed, a, " "); for (i = 1; i <= n; i++) s[a[i]] = 1 }}
        {{ pid[NR] = $1; pp[$1] = $2 }}
        END {{ do {{ c = 0; for (i in pid) {{ q = pid[i]; if (!(q in s) && (pp[q] in s)) {{ s[q] = 1; c = 1 }} }} }} while (c)
              for (q in s) print q }}'
    fi
  }} | grep -vx -e '' -e "$$" | sort -un
}}
first=$(for q in $(members); do echo "$q:$(st $q)"; done)
# The first scan's processes stay targets while they live (pid and start time unchanged),
# even once the wrapper is gone: one that ignores TERM still gets KILL.
targets() {{
  {{ members; for e in $first; do q=${{e%%:*}}; [ "$(st $q)" = "${{e#*:}}" ] && echo "$q"; done; }} | sort -un
}}
n=$(echo "$first" | grep -c .)
if [ "$n" -eq 0 ]; then
  if [ -f $d/{id}.exit ]; then echo "job {id} already exited with code $(cat $d/{id}.exit)"; {exited}else echo "job {id} is not running"; fi
  exit 0
fi
$S kill -TERM $(targets) 2>/dev/null
i=0; while [ -n "$(targets)" ] && [ $i -lt 25 ]; do sleep 0.2; i=$((i + 1)); done
for r in 1 2 3; do
  left=$(targets); [ -z "$left" ] && break
  $S kill -KILL $left 2>/dev/null
  [ -n "$cg" ] && [ -e "$cg/cgroup.kill" ] && $S sh -c 'echo 1 > "$1"' sh "$cg/cgroup.kill" 2>/dev/null
  sleep 0.3
done
if [ -n "$(targets)" ]; then echo "job {id}: some of its processes would not stop"; exit 1; fi
if [ -f $d/{id}.exit ]; then echo "job {id} had exited with code $(cat $d/{id}.exit); stopped $n process(es) it left running"; {exited}
else echo {why} > $d/{id}.stopped; echo "stopped job {id} ($n process(es))"; fi"#,
        prefix = if cancel {
            format!(
                "mkdir -p $d && : > $d/{id}.cancel || exit 5\n\
                     [ -f $d/{id}.pid ] || {{ echo 'job {id} never started'; exit 4; }}\n"
            )
        } else {
            String::new()
        },
        own = linux_own(id),
        // In cancel mode, the payload's recorded code for the reply (see Cancel::from_stop).
        exited = if cancel {
            format!("echo \"AGENTPC_EXITED $(cat $d/{id}.exit)\"; ")
        } else {
            String::new()
        },
    )
}

/// stop_job's PowerShell, or (`cancel`) the cancellation of a foreground execution, with the
/// same exit codes as `linux_stop_script`.
pub fn windows_stop_script(id: u64, cancel: bool) -> String {
    let why = if cancel { "timeout" } else { "stop_job" };
    // The job is a scheduled task whose PowerShell runs <id>.ps1 and records its pid and
    // creation time in <id>.pid (a foreground execution: the SSH session's PowerShell,
    // recorded the same way, and no task). The task is disabled first, so a job not yet
    // started never starts. A process counts as the job's only if created after its parent,
    // and, when the parent is gone and Windows has given its pid to another process, before
    // that one: a reused parent id never pulls in unrelated processes. Rounds rescan until
    // nothing is left; .stopped is written only then, and only for a job without an exit
    // code (one that already exited keeps it, and only what it left running is stopped).
    format!(
        r#"$d = "$env:USERPROFILE\agentpc-bg"; $task = 'agentpc-bg-{id}'
{prefix}$fg = Test-Path "$d\{id}.fg"
$t = if (-not $fg) {{ Get-ScheduledTask -TaskName $task -ErrorAction SilentlyContinue }}
if (-not (Test-Path "$d\{id}.log") -and -not $t -and -not $fg) {{
  try {{ [void][IO.Directory]::CreateDirectory($d); [IO.File]::WriteAllText("$d\{id}.cancel", '') }} catch {{ }}
  "no such job {id} (nothing has recorded itself under that id; a foreground run_command given this id can no longer start)"; exit 3
}}
if ($fg) {{ [IO.File]::WriteAllText("$d\{id}.cancel", '') }}
if ($t) {{ Disable-ScheduledTask -TaskName $task -ErrorAction SilentlyContinue | Out-Null }}
$known = @{{}}
$rec = "$((Get-Content "$d\{id}.pid" -ErrorAction SilentlyContinue))".Trim() -split ' '
if ($rec.Count -eq 2 -and $rec[0] -match '^\d+$' -and $rec[1] -match '^\d+$') {{ $known[[uint32]$rec[0]] = [int64]$rec[1] }}
# Creation times compared to within 1 us: the job records its own to 100 ns, WMI to 1 us.
function Same($a, $b) {{ [math]::Abs($a - $b) -lt 10 }}
# A process list that can't be read (WMI failing) or comes back empty proves nothing: the
# stop is then unconfirmed, never "not running".
$global:__agentpc_blind = $false
function Live {{
  try {{ $all = @(Get-CimInstance Win32_Process -ErrorAction Stop) }} catch {{ $global:__agentpc_blind = $true; return }}
  if (-not $all.Count) {{ $global:__agentpc_blind = $true; return }}
  $holder = @{{}}
  foreach ($r in $all) {{ $holder[$r.ProcessId] = $r.CreationDate.ToFileTimeUtc() }}
  foreach ($r in $all) {{ if ($r.CommandLine -like "*\{id}.ps1*" -and -not $known.ContainsKey($r.ProcessId)) {{ $known[$r.ProcessId] = $holder[$r.ProcessId] }} }}
  do {{
    $added = $false
    foreach ($c in $all) {{
      $q = $c.ParentProcessId; $born = $holder[$c.ProcessId]
      if ($known.ContainsKey($c.ProcessId) -or -not $known.ContainsKey($q) -or $born -lt $known[$q]) {{ continue }}
      if ($holder.ContainsKey($q) -and -not (Same $holder[$q] $known[$q]) -and $born -ge $holder[$q]) {{ continue }}
      $known[$c.ProcessId] = $born; $added = $true
    }}
  }} while ($added)
  @($all | Where-Object {{ $known.ContainsKey($_.ProcessId) -and (Same $known[$_.ProcessId] $holder[$_.ProcessId]) }})
}}
$stopped = @{{}}
for ($i = 0; $i -lt 10; $i++) {{
  $live = Live
  if (-not $live.Count) {{ break }}
  foreach ($q in $live) {{ $stopped[$q.ProcessId] = 1; Stop-Process -Id $q.ProcessId -Force -ErrorAction SilentlyContinue }}
  if ($i -eq 0 -and $t) {{ Stop-ScheduledTask -TaskName $task -ErrorAction SilentlyContinue }}
  Start-Sleep -Milliseconds 300
}}
$left = Live
# The recorded root is checked on its own (Get-Process, pid and creation time), whatever the
# process list said; blind, it is still stopped if it is provably the job's.
{own}
if ($global:__agentpc_blind -and (Own)) {{ Stop-Process -Id ([int]$rec[0]) -Force -ErrorAction SilentlyContinue; Start-Sleep -Milliseconds 300 }}
if ($global:__agentpc_blind) {{ "job {id}: its processes could not be listed, so whether it stopped is unknown"; exit 1 }}
if ($left.Count) {{ "job {id}: $($left.Count) of its processes would not stop"; exit 1 }}
if (Own) {{ "job {id}: its recorded process is still running"; exit 1 }}
$n = $stopped.Count
if (Test-Path "$d\{id}.exit") {{
  $code = (Get-Content "$d\{id}.exit" -Raw).Trim()
  if ($n) {{ "job {id} had exited with code $code; stopped $n process(es) it left running" }} else {{ "job {id} already exited with code $code" }}
  {exited}exit 0
}}
if (-not $n -and ($fg -or ($t -and ($t | Get-ScheduledTaskInfo).LastRunTime.Year -gt 2000))) {{ "job {id} is not running"; exit 0 }}
Set-Content -Encoding ascii "$d\{id}.stopped" '{why}'
if ($n) {{ "stopped job {id} ($n process(es))" }} else {{ "stopped job {id} before it started" }}"#,
        prefix = if cancel {
            format!(
                "try {{ [void][IO.Directory]::CreateDirectory($d); [IO.File]::WriteAllText(\"$d\\{id}.cancel\", '') }} catch {{ exit 5 }}\n\
                     if (-not (Test-Path \"$d\\{id}.pid\")) {{ \"job {id} never started\"; exit 4 }}\n"
            )
        } else {
            String::new()
        },
        own = windows_own(id),
        exited = if cancel {
            "\"AGENTPC_EXITED $code\"; "
        } else {
            ""
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn captured(s: &str) -> Captured {
        let mut c = Captured::default();
        c.push(s.as_bytes());
        c
    }

    fn facts(code: Option<i32>, start: Start) -> Facts {
        Facts {
            code,
            timed_out: false,
            start,
            cancel: None,
            probe: None,
            stdout: captured("out\n"),
            stderr: captured(""),
        }
    }

    fn sc(r: &CallToolResult) -> &serde_json::Value {
        r.structured_content.as_ref().unwrap()
    }

    fn text(r: &CallToolResult) -> String {
        r.content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn success_is_structured_and_keeps_the_legacy_text() {
        let r = conclude(facts(Some(0), Start::Seen), 7, "vm", 120).into_result();
        assert_eq!(r.is_error, Some(false));
        // One text block, exactly the old reply: text-only clients see no change.
        assert_eq!(r.content.len(), 1);
        assert_eq!(text(&r), "exit code: 0\n--- stdout ---\nout");
        let s = sc(&r);
        assert_eq!(s["schema_version"], 1);
        assert_eq!(s["state"], "exited");
        assert_eq!(s["exit_code"], 0);
        assert_eq!(s["stdout"], "out");
        assert_eq!(s["stderr"], "");
        assert_eq!(s["execution_id"], 7);
        assert_eq!(s["job_id"], serde_json::Value::Null);
        assert_eq!(s["timed_out"], false);
        assert_eq!(s["termination_confirmed"], serde_json::Value::Null);
        assert_eq!(s["effect_unknown"], false);
        assert_eq!(s["recovery_action"], serde_json::Value::Null);
        assert_eq!(s["output_truncated"], false);
        assert_eq!(s["stdout_bytes"], 4);
    }

    #[test]
    fn nonzero_exit_is_a_tool_error_with_its_code() {
        let r = conclude(facts(Some(3), Start::Seen), 7, "vm", 120).into_result();
        assert_eq!(r.is_error, Some(true));
        assert!(text(&r).starts_with("exit code: 3\n--- stdout ---"));
        assert_eq!(sc(&r)["state"], "exited");
        assert_eq!(sc(&r)["exit_code"], 3);
        assert_eq!(sc(&r)["effect_unknown"], false);
    }

    #[test]
    fn confirmed_timeout_reports_partial_output() {
        let mut f = facts(None, Start::Seen);
        f.timed_out = true;
        f.cancel = Some(Cancel::Stopped { exited: None });
        let r = conclude(f, 9, "vm", 3).into_result();
        assert_eq!(r.is_error, Some(true));
        assert!(r.content.iter().any(|c| c.as_text().is_some()));
        let t = text(&r);
        assert!(t.starts_with("timed out after 3s"), "{t}");
        assert!(
            t.contains("(partial output below)\n--- stdout ---\nout"),
            "{t}"
        );
        let s = sc(&r);
        assert_eq!(s["state"], "timed_out");
        assert_eq!(s["timed_out"], true);
        assert_eq!(s["termination_confirmed"], true);
        assert_eq!(s["effect_unknown"], false);
        assert_eq!(s["exit_code"], serde_json::Value::Null);
        assert_eq!(s["job_id"], serde_json::Value::Null);
    }

    #[test]
    fn unconfirmed_timeout_hands_out_the_job_id() {
        let mut f = facts(None, Start::Seen);
        f.timed_out = true;
        f.cancel = Some(Cancel::Unconfirmed("the VM did not answer".into()));
        let r = conclude(f, 9, "vm", 3).into_result();
        assert_eq!(r.is_error, Some(true));
        let s = sc(&r);
        assert_eq!(s["timed_out"], true);
        assert_eq!(s["termination_confirmed"], false);
        assert_eq!(s["effect_unknown"], true);
        assert_eq!(s["job_id"], 9);
        assert_eq!(s["recovery_action"], "stop_job");
        let t = text(&r);
        assert!(t.starts_with("timed out after 3s"), "{t}");
        assert!(t.contains("get_job_status name=vm id=9") && t.contains("stop_job name=vm id=9"));
    }

    #[test]
    fn timeout_before_the_start_is_not_started() {
        let mut f = facts(None, Start::Absent);
        f.timed_out = true;
        f.cancel = Some(Cancel::NotStarted);
        let s = conclude(f, 9, "vm", 3).outcome;
        assert_eq!(s.state, ExecutionState::NotStarted);
        assert!(s.timed_out && !s.effect_unknown);
        // Nothing was terminated: it never started.
        assert_eq!(s.termination_confirmed, None);
        assert_eq!(s.recovery_action, Some(RecoveryAction::Retry));
    }

    #[test]
    fn a_wrapper_that_ends_before_the_marker_never_ran_the_command() {
        let o = conclude(facts(Some(WRAPPER_SETUP_FAILED), Start::Absent), 9, "vm", 3).outcome;
        assert_eq!(o.state, ExecutionState::NotStarted);
        assert_eq!(o.exit_code, None);
        assert!(!o.effect_unknown);
        assert_eq!(o.recovery_action, Some(RecoveryAction::Retry));
        let o = conclude(facts(Some(WRAPPER_CANCELLED), Start::Absent), 9, "vm", 3).outcome;
        assert_eq!(o.state, ExecutionState::NotStarted);
        assert_eq!(o.recovery_action, None);
    }

    #[test]
    fn ssh_failure_is_judged_by_the_vm_record_never_by_text() {
        let with = |code, start, probe| {
            let mut f = facts(code, start);
            f.probe = Some(probe);
            conclude(f, 5, "vm", 120).outcome
        };
        // 255 that the record confirms is the command's own code.
        let o = with(Some(255), Start::Seen, Probe::Exited(255));
        assert_eq!((o.state, o.exit_code), (ExecutionState::Exited, Some(255)));
        // Dropped after the command finished: code known, output maybe not.
        let o = with(Some(255), Start::Seen, Probe::Exited(0));
        assert_eq!(o.state, ExecutionState::ConnectionLost);
        assert_eq!(o.exit_code, Some(0));
        assert!(!o.effect_unknown);
        // Dropped while it runs: a handle, nothing hidden.
        let o = with(None, Start::Seen, Probe::Running);
        assert_eq!(o.state, ExecutionState::ConnectionLost);
        assert!(o.effect_unknown);
        assert_eq!(o.job_id, Some(5));
        assert_eq!(o.recovery_action, Some(RecoveryAction::GetJobStatus));
        // Never connected: the check made sure it never starts.
        let o = with(Some(255), Start::Absent, Probe::NotStarted);
        assert_eq!(o.state, ExecutionState::NotStarted);
        assert!(!o.effect_unknown);
        // A record-less answer contradicting a marker that was seen is not trusted.
        let o = with(Some(255), Start::Seen, Probe::NotStarted);
        assert!(o.effect_unknown);
        // Nothing could be checked: the reply says how to make sure it never starts.
        let mut f = facts(Some(255), Start::Absent);
        f.probe = Some(Probe::Failed("down".into()));
        assert!(conclude(f, 5, "vm", 120).text.contains("stop_job"));
        let o = with(Some(255), Start::Absent, Probe::Failed("down".into()));
        assert_eq!(o.state, ExecutionState::ConnectionLost);
        assert!(o.effect_unknown);
        assert_eq!(o.job_id, Some(5));
        let o = with(None, Start::Seen, Probe::Gone);
        assert!(o.effect_unknown);
        assert_eq!(o.recovery_action, Some(RecoveryAction::InspectVm));
    }

    /// Review finding 1: a Windows payload's explicit `exit 255`, `exit -1` or `exit 511` all
    /// reach the Mac's ssh as 255. The VM's record of the full code settles it as the command's
    /// own exit, reported as the 8-bit code ssh gives (as before); a record whose low byte
    /// differs means the connection really failed.
    #[test]
    fn exit_codes_ssh_sees_as_255_are_the_commands_own_when_recorded() {
        for rec in ["-1", "255", "511", "4294967295"] {
            let mut f = facts(Some(255), Start::Seen);
            f.probe = Some(Probe::parse(&format!("AGENTPC_PROBE exited {rec}")));
            let r = conclude(f, 5, "vm", 120);
            assert_eq!(
                (r.outcome.state, r.outcome.exit_code),
                (ExecutionState::Exited, Some(255)),
                "{rec}"
            );
            assert!(!r.outcome.effect_unknown, "{rec}");
            assert!(r.text.starts_with("exit code: 255\n"), "{rec}: {}", r.text);
        }
        let mut f = facts(Some(255), Start::Seen);
        f.probe = Some(Probe::parse("AGENTPC_PROBE exited 3"));
        let o = conclude(f, 5, "vm", 120).outcome;
        assert_eq!(
            (o.state, o.exit_code),
            (ExecutionState::ConnectionLost, Some(3))
        );
        // No record at all after a 255: ambiguous, and the reply says both readings.
        let mut f = facts(Some(255), Start::Seen);
        f.probe = Some(Probe::Gone);
        let r = conclude(f, 5, "vm", 120);
        assert!(r.outcome.effect_unknown);
        assert!(r.text.contains("exited with code 255"), "{}", r.text);
    }

    /// Final review: an oversized Linux payload is refused before SSH (sshd's exec would fail
    /// the same way every time), counted in UTF-8 bytes, not characters.
    #[test]
    fn an_oversized_linux_payload_is_refused_before_it_is_sent() {
        let max = LINUX_FOREGROUND_MAX_BYTES;
        assert_eq!(max, 94_208);
        assert!(linux_foreground_too_large("echo hi").is_none());
        assert!(linux_foreground_too_large(&"x".repeat(max)).is_none());
        // Two-byte characters: as many bytes as the limit fit, one character more doesn't,
        // though it is far under the limit in characters.
        assert!(linux_foreground_too_large(&"é".repeat(max / 2)).is_none());
        let big = "é".repeat(max / 2 + 1);
        assert!(big.chars().count() < max);
        for command in ["x".repeat(max + 1), big] {
            let r = linux_foreground_too_large(&command).expect("refused");
            assert!(
                r.text.contains(&format!("{} bytes", command.len())),
                "{}",
                r.text
            );
            assert!(r.text.contains("upload_file") && r.text.contains("nothing was sent"));
            assert!(!r.text.to_lowercase().contains("retry"), "{}", r.text);
            let r = r.into_result();
            assert_eq!(r.is_error, Some(true));
            let sc = r.structured_content.unwrap();
            assert_eq!(sc["state"], "not_started");
            assert!(sc["execution_id"].is_null() && sc["job_id"].is_null());
            assert!(sc["recovery_action"].is_null());
            assert_eq!(sc["effect_unknown"], false);
            assert!(sc["error"].as_str().unwrap().contains("upload_file"));
        }
    }

    /// Final review: a cancel that found nothing left (the command ended right at the timeout,
    /// or what held its output ended by itself first) and one that stopped leftovers give the
    /// same known exit, so the reply claims neither a held pipe nor a kill, only what is
    /// proven: the recorded code, the deadline, and the verified absence.
    #[test]
    fn a_late_known_exit_claims_no_held_pipe_or_kill() {
        for stop in [
            "job 9 already exited with code 2\nAGENTPC_EXITED 2\n",
            "job 9 had exited with code 2; stopped 1 process(es) it left running\nAGENTPC_EXITED 2\n",
        ] {
            let cancel = Cancel::from_stop(Some(0), false, stop, 45);
            assert_eq!(cancel, Cancel::Stopped { exited: Some(2) });
            let mut f = facts(None, Start::Seen);
            f.timed_out = true;
            f.cancel = Some(cancel);
            let r = conclude(f, 9, "vm", 5);
            assert!(r.text.starts_with("exit code: 2 ("), "{}", r.text);
            for claim in [
                "held",
                "something it started",
                "killed",
                "stopped",
                "left running",
            ] {
                assert!(!r.text.contains(claim), "{claim}: {}", r.text);
            }
            for fact in [
                "recorded",
                "5s timeout",
                "none of the processes it identified is left",
            ] {
                assert!(r.text.contains(fact), "{fact}: {}", r.text);
            }
            assert_eq!(r.outcome.exit_code, Some(2));
            assert!(r.outcome.timed_out);
            assert_eq!(r.outcome.termination_confirmed, Some(true));
            let r = r.into_result();
            assert_eq!(r.is_error, Some(true));
            let sc = r.structured_content.unwrap();
            assert_eq!(
                (sc["timed_out"].as_bool(), sc["exit_code"].as_i64()),
                (Some(true), Some(2))
            );
        }
    }

    /// Review finding 3: the command had finished, but something it left running held its
    /// output open until the timeout; the cancellation stopped only the leftovers. Its exit
    /// code is known and reported.
    #[test]
    fn a_finished_command_whose_leftovers_held_the_session_keeps_its_code() {
        let cancel = Cancel::from_stop(
            Some(0),
            false,
            "job 9 had exited with code 0; stopped 1 process(es) it left running\nAGENTPC_EXITED 0\n",
            45,
        );
        assert_eq!(cancel, Cancel::Stopped { exited: Some(0) });
        let mut f = facts(None, Start::Seen);
        f.timed_out = true;
        f.cancel = Some(cancel);
        let r = conclude(f, 9, "vm", 5);
        assert_eq!(r.outcome.state, ExecutionState::Exited);
        assert_eq!(r.outcome.exit_code, Some(0));
        assert_eq!(r.outcome.termination_confirmed, Some(true));
        assert!(r.outcome.timed_out);
        assert!(r.text.starts_with("exit code: 0"), "{}", r.text);
        // Fix2 B: the cleanup may have found nothing left to stop (the leftovers can end by
        // themselves before it runs), so the reply claims no killing, only the verified end.
        assert!(
            !r.text.contains("stopped them") && !r.text.contains("killed"),
            "{}",
            r.text
        );
        assert!(
            r.text
                .contains("none of the processes it identified is left"),
            "{}",
            r.text
        );
        // Fix2 A: every reply that reached the timeout stays a tool error, as it always was,
        // even with the command's own exit code 0 kept.
        let r = r.into_result();
        assert_eq!(r.is_error, Some(true));
        assert_eq!(r.structured_content.as_ref().unwrap()["timed_out"], true);
        assert_eq!(r.structured_content.as_ref().unwrap()["exit_code"], 0);
        // The stop's other answers, by exit code; text never decides.
        assert_eq!(
            Cancel::from_stop(Some(0), false, "stopped job 9 (2 process(es))", 45),
            Cancel::Stopped { exited: None }
        );
        assert_eq!(
            Cancel::from_stop(Some(4), false, "", 45),
            Cancel::NotStarted
        );
        assert!(matches!(
            Cancel::from_stop(Some(1), false, "", 45),
            Cancel::Unconfirmed(_)
        ));
        assert!(matches!(
            Cancel::from_stop(Some(3), false, "", 45),
            Cancel::Unconfirmed(_)
        ));
        assert!(matches!(
            Cancel::from_stop(Some(0), true, "", 45),
            Cancel::Unconfirmed(_)
        ));
        assert!(matches!(
            Cancel::from_stop(None, false, "", 45),
            Cancel::Unconfirmed(_)
        ));
        // A malformed exit line is not a code.
        assert_eq!(
            Cancel::from_stop(Some(0), false, "AGENTPC_EXITED x", 45),
            Cancel::Stopped { exited: None }
        );
    }

    /// Review finding 6: an id was issued before ssh failed to start; keep it.
    #[test]
    fn a_spawn_failure_keeps_the_issued_id() {
        let o = Reply::spawn_failed(12, "run ssh: no such file".into()).outcome;
        assert_eq!(o.state, ExecutionState::NotStarted);
        assert_eq!(o.execution_id, Some(12));
        assert_eq!(o.recovery_action, Some(RecoveryAction::Retry));
    }

    /// Review finding 6: foreground payloads are not written to the guest's disk, and the
    /// Windows GC drops orphan tombstones like the Linux one.
    #[test]
    fn foreground_payloads_stay_off_disk_and_gc_drops_orphan_tombstones() {
        let l = linux_foreground(77, "echo secret", "A=1");
        assert!(!l.contains("77.sh"), "{l}");
        assert!(WINDOWS_GC.contains("*.cancel"), "{WINDOWS_GC}");
    }

    #[test]
    fn probe_lines_parse_strictly() {
        assert_eq!(Probe::parse("AGENTPC_PROBE exited 3\n"), Probe::Exited(3));
        assert_eq!(Probe::parse("noise\nAGENTPC_PROBE running"), Probe::Running);
        assert_eq!(Probe::parse("AGENTPC_PROBE gone"), Probe::Gone);
        assert_eq!(Probe::parse("AGENTPC_PROBE not_started"), Probe::NotStarted);
        assert!(matches!(
            Probe::parse("AGENTPC_PROBE exited x"),
            Probe::Failed(_)
        ));
        assert!(matches!(
            Probe::parse("AGENTPC_PROBE error"),
            Probe::Failed(_)
        ));
        assert!(matches!(Probe::parse(""), Probe::Failed(_)));
    }

    #[test]
    fn background_results_are_structured() {
        let r = background_started(11, "started".into(), Some("/l".into())).into_result();
        assert_eq!(r.is_error, Some(false));
        assert_eq!(text(&r), "started");
        let s = sc(&r);
        assert_eq!(s["state"], "started");
        assert_eq!(s["background"], true);
        assert_eq!(s["job_id"], 11);
        assert_eq!(s["log_path"], "/l");
        let r = background_failed(11, "exit code: 1".into(), true, "vm").into_result();
        assert_eq!(r.is_error, Some(true));
        assert_eq!(sc(&r)["state"], "connection_lost");
        assert_eq!(sc(&r)["effect_unknown"], true);
        assert!(text(&r).starts_with("exit code: 1\n"));
        let r = background_failed(11, "exit code: 1".into(), false, "vm").into_result();
        assert_eq!(sc(&r)["state"], "not_started");
        assert_eq!(text(&r), "exit code: 1");
    }

    #[test]
    fn early_failures_keep_their_message_as_text() {
        let r = Reply::not_started(
            "VM x is not running; start_vm first".into(),
            Some(RecoveryAction::StartVm),
        )
        .into_result();
        assert_eq!(r.is_error, Some(true));
        assert_eq!(text(&r), "VM x is not running; start_vm first");
        assert_eq!(sc(&r)["state"], "not_started");
        assert_eq!(sc(&r)["recovery_action"], "start_vm");
        assert_eq!(sc(&r)["execution_id"], serde_json::Value::Null);
    }

    /// Every outcome serializes to exactly the properties the declared schema lists, with
    /// enum values the schema allows: what a client validating structuredContent checks.
    #[test]
    fn outcomes_match_the_declared_schema() {
        let schema = serde_json::Value::Object((*output_schema()).clone());
        assert_eq!(schema["type"], "object");
        let props = schema["properties"].as_object().unwrap();
        for field in [
            "schema_version",
            "execution_id",
            "job_id",
            "state",
            "exit_code",
            "stdout",
            "stderr",
            "timed_out",
            "output_truncated",
            "termination_confirmed",
            "effect_unknown",
            "recovery_action",
        ] {
            assert!(props.contains_key(field), "schema lacks {field}");
        }
        let states = schema.to_string();
        for s in [
            "exited",
            "timed_out",
            "not_started",
            "connection_lost",
            "started",
        ] {
            assert!(states.contains(&format!("\"{s}\"")), "{s}");
        }
        let mut samples = vec![
            conclude(facts(Some(0), Start::Seen), 1, "vm", 1).outcome,
            background_started(2, String::new(), None).outcome,
            CommandOutcome::not_started("x".into(), None),
        ];
        let mut f = facts(None, Start::Seen);
        f.timed_out = true;
        f.cancel = Some(Cancel::Unconfirmed("x".into()));
        samples.push(conclude(f, 3, "vm", 1).outcome);
        for o in samples {
            let v = serde_json::to_value(&o).unwrap();
            let obj = v.as_object().unwrap();
            assert_eq!(obj.len(), props.len(), "{v}");
            for k in obj.keys() {
                assert!(props.contains_key(k), "{k} not in schema");
            }
            let back: CommandOutcome = serde_json::from_value(v).unwrap();
            assert_eq!(back, o);
        }
        // No $ref indirection a simple client validator might not resolve.
        assert!(!schema.to_string().contains("$ref"), "{schema}");
    }

    #[test]
    fn truncation_is_reported_per_stream() {
        let mut big = Captured::default();
        for _ in 0..100 {
            big.push(&[b'x'; 4096]);
        }
        let o = CommandOutcome::new(ExecutionState::Exited, None).with_output(&big, &captured("e"));
        assert!(o.stdout_truncated && o.output_truncated && !o.stderr_truncated);
        assert_eq!(o.stdout_bytes, 409_600);
        // Long but fully kept bytes still clipped by characters.
        let o = CommandOutcome::new(ExecutionState::Exited, None)
            .with_output(&captured(&"y".repeat(2 * OUTPUT_KEEP + 1)), &captured(""));
        assert!(o.stdout_truncated);
        // Trailing whitespace trimmed isn't truncation.
        let o = CommandOutcome::new(ExecutionState::Exited, None)
            .with_output(&captured("a\n\n"), &captured(""));
        assert!(!o.output_truncated);
    }

    #[test]
    fn the_start_marker_is_taken_out_of_stderr_wherever_it_splits() {
        let m = start_marker(42);
        let mut whole = b"login noise\n".to_vec();
        whole.extend_from_slice(&m);
        whole.extend_from_slice(b"payload err\n");
        for cut in 0..whole.len() {
            let mut s = Stream::with_marker(m.clone());
            s.feed(&whole[..cut]);
            s.feed(&whole[cut..]);
            let (c, start) = s.finish(true);
            assert_eq!(start, Start::Seen, "cut at {cut}");
            assert_eq!(c.text(), "login noise\npayload err");
            assert_eq!(c.total, whole.len() - m.len());
        }
        // Another execution's marker is payload output, not ours.
        let mut s = Stream::with_marker(m.clone());
        s.feed(&start_marker(43));
        assert_eq!(s.finish(true).1, Start::Absent);
        // Cut off before an answer: unknown, not "never started".
        let mut s = Stream::with_marker(m.clone());
        s.feed(b"x");
        let (c, start) = s.finish(false);
        assert_eq!((start, c.text().as_str()), (Start::Unknown, "x"));
        // Too much before any marker: stop holding output back.
        let mut s = Stream::with_marker(m);
        s.feed(&vec![b'z'; SCAN_LIMIT + 1]);
        assert_eq!(s.start, Some(Start::Unknown));
        assert!(s.pending.is_empty());
    }

    #[test]
    fn wrappers_record_before_the_marker_and_check_cancel_in_between() {
        let l = linux_foreground(77, "echo 'hi'", "A=1 B=2");
        assert!(l.starts_with("export A=1 B=2\n"));
        // The payload travels encoded, never in a command line where pgrep -f sees it.
        assert!(!l.contains("echo 'hi'"));
        let rec = l.find("echo $$ > $d/77.pid").unwrap();
        let proof = l.find("> $d/77.proof").unwrap();
        let cancel = l.find("[ -e $d/77.cancel ]").unwrap();
        let marker = l.find("agentpc-started:77").unwrap();
        let payload = l.find("bash -c \"$cmd\"").unwrap();
        assert!(rec < proof && proof < cancel && cancel < marker && marker < payload);
        assert!(l.contains("--unit=agentpc-job-77"));
        let w = windows_foreground(77, "Write-Output 'hi'");
        let prelude = decoded(&w, 0);
        let rec = prelude.find("77.pid").unwrap();
        let cancel = prelude.find("77.cancel").unwrap();
        let marker = prelude.find("agentpc-started:77").unwrap();
        assert!(rec < cancel && cancel < marker);
        assert!(prelude.contains("StartTime.ToFileTimeUtc()"));
        assert_eq!(decoded(&w, 1), "Write-Output 'hi'\n$__agentpc_ok = $?");
        // -Command semantics: 1 when the last statement failed, else 0 (an explicit exit
        // in the payload ends PowerShell with its own code before this).
        assert!(w.ends_with(
            "$__agentpc_c = 0; if (-not $__agentpc_ok) { $__agentpc_c = 1 }; \
             try { [IO.File]::WriteAllText(\"$__agentpc_d\\77.exit\", \"$__agentpc_c\") } catch { }; \
             exit $__agentpc_c"
        ));
    }

    fn decoded(w: &str, n: usize) -> String {
        let b64 = w.split("FromBase64String('").nth(n + 1).unwrap();
        let b64 = &b64[..b64.find('\'').unwrap()];
        String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .unwrap(),
        )
        .unwrap()
    }
}

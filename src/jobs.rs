//! `get_job_status`: a typed job state, bounded waits, and incremental log reads.
//!
//! The guest side is a read-only script per OS (`linux_script`, `windows_script`). It prints
//! one machine line per fact, `AGENTPC_JOB <key> <value>`, and the log bytes of this reply as
//! base64, so the reply never depends on human text and survives any byte in the log. It
//! never writes anything in the VM: no tombstone, no `.cancel`, no probe. The host parses
//! those lines (`parse_report`), turns them into a [`JobStatus`] (`assemble`) and polls them
//! within a deadline (`observe`).

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use rmcp::model::{CallToolResult, ContentBlock};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::execution::{linux_own, linux_own_by, windows_own, windows_own_by};

/// Layout version of [`JobStatus`]; bumped only for incompatible changes.
pub const SCHEMA_VERSION: u32 = 1;

/// The longest `wait_seconds` get_job_status accepts.
pub const MAX_WAIT_SECONDS: u64 = 30;

/// The most log bytes one reply carries (a tail or an incremental read). A longer log is read
/// in several calls, each continuing from `next_cursor`.
pub const REPLY_BYTES: usize = 16 * 1024;

/// How much of the start of a log a cursor fingerprints, to notice a log that was truncated
/// and rewritten in place.
pub const HEAD_BYTES: usize = 64;

/// The longest cursor string accepted (a real one is about 250 characters).
pub const CURSOR_MAX_CHARS: usize = 1024;

/// Pause between status checks while waiting.
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// While waiting, a status check gets what is left of the wait plus this much for the
/// transport (an SSH round trip), so a call overruns its wait by at most this plus the
/// pacing's `grace` (and by nothing when SSH answers in time).
pub const TRANSPORT_GRACE: Duration = Duration::from_secs(2);

/// The longest a single status check may take (the whole budget of a no-wait call).
pub const ATTEMPT_CEILING: Duration = Duration::from_secs(30);

/// Shown instead of a log for a foreground execution. No quotes: it is plain text only.
pub const FOREGROUND_NOTE: &str =
    "(a foreground run_command: its output went to the reply of that call, so there is no log)";

/// Said of a job whose exit record is not a valid exit code.
const UNKNOWN_NOTE: &str = "A record of this job is not valid (an exit record that is not an \
     exit code), so its state is unknown, not ended. Treat it as unresolved.";

/// Pacing of a status call: tests use short ones.
#[derive(Clone, Copy, Debug)]
pub struct Pacing {
    /// Pause between status checks while waiting.
    pub poll: Duration,
    /// While waiting, a check gets the time left in the wait plus this (never a fresh
    /// minimum): the explicit allowance for the transport.
    pub transport: Duration,
    /// The most a check gets (all of a no-wait call's budget).
    pub ceiling: Duration,
    /// How long past its budget a check may take to report its own timeout before it is
    /// abandoned here.
    pub grace: Duration,
}

impl Pacing {
    pub const DEFAULT: Pacing = Pacing {
        poll: POLL_INTERVAL,
        transport: TRANSPORT_GRACE,
        ceiling: ATTEMPT_CEILING,
        grace: Duration::from_millis(250),
    };
}

// ---------------------------------------------------------------------------------------------
// The result

#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum JobState {
    /// Launched, but not yet running (a scheduled task that has not started, or a launch still
    /// recording itself). Keep polling.
    Queued,
    Running,
    /// Ran to its end; `exit_code` is its code.
    Exited,
    /// Stopped by `stop_job`, at a foreground command's timeout, or when its run_command
    /// request was cancelled (`stop_reason`).
    Stopped,
    /// Gone without an exit code: killed, or the VM restarted.
    Ended,
    /// A foreground execution whose command never started and now never will: a
    /// cancellation (its timeout, its request's cancellation, stop_job, or the check after a
    /// lost connection) was recorded before it did. Without that record a foreground launch
    /// is `queued`.
    NotStarted,
    /// Nothing in the VM has recorded itself under this id (yet).
    Missing,
    /// The state could not be read (see `error`), or a record exists that is not valid (an exit
    /// record that is not an exit code: then `note` says so and the job is not guessed to have
    /// ended).
    #[default]
    Unknown,
}

impl JobState {
    fn terminal(self) -> bool {
        matches!(
            self,
            JobState::Exited | JobState::Stopped | JobState::Ended | JobState::NotStarted
        )
    }
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum JobKind {
    Background,
    Foreground,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum StopReason {
    /// A foreground command stopped at its run_command timeout.
    Timeout,
    /// Stopped by stop_job.
    StopJob,
    /// A foreground command stopped because its run_command request was cancelled (the
    /// client cancelled it or disconnected, or the MCP server shut down).
    RequestCancelled,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum ResetReason {
    /// The VM was reset or restored from a checkpoint, or is not the one the cursor came from.
    VmChanged,
    /// The guest rebooted since the cursor was issued.
    Reboot,
    /// The log file was replaced by another.
    LogReplaced,
    /// The log is now shorter than the cursor's position.
    LogShrunk,
    /// The log's beginning is no longer what the cursor saw (truncated and rewritten).
    LogRewritten,
    /// The log the cursor pointed into no longer exists.
    LogMissing,
}

impl ResetReason {
    fn name(self) -> &'static str {
        match self {
            ResetReason::VmChanged => "vm_changed",
            ResetReason::Reboot => "reboot",
            ResetReason::LogReplaced => "log_replaced",
            ResetReason::LogShrunk => "log_shrunk",
            ResetReason::LogRewritten => "log_rewritten",
            ResetReason::LogMissing => "log_missing",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "reboot" => ResetReason::Reboot,
            "log_replaced" => ResetReason::LogReplaced,
            "log_shrunk" => ResetReason::LogShrunk,
            "log_rewritten" => ResetReason::LogRewritten,
            _ => return None,
        })
    }
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum JobErrorCode {
    /// `wait_seconds` is above 30.
    InvalidWaitSeconds,
    /// The cursor is malformed, or belongs to another VM or job.
    InvalidCursor,
    NoSuchVm,
    VmNotRunning,
    /// The VM did not answer (SSH failed or timed out): the job's state is unknown, not ended.
    GuestUnreachable,
    /// The VM answered, but not with a status report this server understands.
    BadGuestReply,
}

/// What get_job_status reports, as its structured content. The same object describes every
/// answer and every failure; a failure has `error` set (and is a tool error) and `state`
/// `unknown`. Asking about a job that exited with a nonzero code is not an error.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct JobStatus {
    /// Layout version of this object (currently 1).
    pub schema_version: u32,
    /// The job's state; see the variants. `unknown` together with `error` (the state could
    /// not be read) or with a `note` (an invalid exit record).
    pub state: JobState,
    /// The id asked about.
    pub job_id: u64,
    /// `background` (a `background: true` job, with a log) or `foreground` (an execution
    /// whose output went to its run_command reply, so there is no log); null while nothing
    /// has recorded itself.
    pub kind: Option<JobKind>,
    /// The job's exit code when `state` is `exited`; null otherwise. A foreground handle's is
    /// the one run_command reports (the recorded code's low 8 bits, so a Windows `exit -1`
    /// reads 255). A background job's is its record's: on Windows PowerShell's own code, which
    /// can be negative or above 255.
    pub exit_code: Option<i64>,
    /// Why a `stopped` job stopped; null otherwise.
    pub stop_reason: Option<StopReason>,
    /// Whether the job has a log file in the VM that was read.
    pub log_available: bool,
    /// Without a `cursor`: the end of the log (the last `tail_lines` lines, at most 16 KiB),
    /// from the same instant `next_cursor` points to. Null when a cursor was given or the job
    /// has no log.
    pub log_tail: Option<String>,
    /// With a `cursor`: the log text from the cursor on (at most 16 KiB). Null when no cursor
    /// was given or the job has no log.
    pub output: Option<String>,
    /// How many bytes of the log this reply covers (a byte-order mark counts).
    pub output_bytes: u64,
    /// Whether the log has text this reply left out: the 16 KiB cap cut it (the beginning of a
    /// tail; the end of an incremental read, then `has_more` is true too).
    pub output_truncated: bool,
    /// Whether the text contains U+FFFD standing for bytes that were not valid UTF-8.
    pub invalid_utf8: bool,
    /// With a cursor: whether the log has more bytes past this reply (the cap was hit).
    pub has_more: bool,
    /// Pass this as `cursor` to continue exactly where this reply ends. Null for a job with no
    /// log. It is metadata, not a credential.
    pub next_cursor: Option<String>,
    /// Whether the cursor no longer matched the log (the VM was reset, the guest rebooted, the
    /// log was replaced, truncated or deleted): the output then restarts at the beginning of
    /// the log, and what the client holds from before is stale.
    pub cursor_reset: bool,
    /// Why, when `cursor_reset`.
    pub cursor_reset_reason: Option<ResetReason>,
    /// Whether a requested wait ran out with the job still going: the job was not touched.
    pub wait_timed_out: bool,
    /// How long this call took, in milliseconds.
    pub waited_ms: u64,
    /// A remark that is not a failure.
    pub note: Option<String>,
    /// Why the call failed (null on success), with what to do about it.
    pub error: Option<String>,
    /// The kind of failure.
    pub error_code: Option<JobErrorCode>,
}

impl JobStatus {
    fn base(job_id: u64, state: JobState) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            state,
            job_id,
            kind: None,
            exit_code: None,
            stop_reason: None,
            log_available: false,
            log_tail: None,
            output: None,
            output_bytes: 0,
            output_truncated: false,
            invalid_utf8: false,
            has_more: false,
            next_cursor: None,
            cursor_reset: false,
            cursor_reset_reason: None,
            wait_timed_out: false,
            waited_ms: 0,
            note: None,
            error: None,
            error_code: None,
        }
    }

    /// The call failed: nothing is known about the job's state.
    pub fn failure(job_id: u64, code: JobErrorCode, message: String) -> Self {
        let mut s = Self::base(job_id, JobState::Unknown);
        s.error = Some(message);
        s.error_code = Some(code);
        s
    }

    /// The readable text plus the structured status. The text is the only content block (no
    /// JSON mirror), so clients that read text see what they always saw.
    pub fn into_result(self) -> CallToolResult {
        let error = self.error.is_some();
        let text = render_text(&self);
        let structured = serde_json::to_value(&self).expect("status serializes");
        let mut r = if error {
            CallToolResult::error(vec![ContentBlock::text(text)])
        } else {
            CallToolResult::success(vec![ContentBlock::text(text)])
        };
        r.structured_content = Some(structured);
        r
    }
}

/// The output schema get_job_status declares.
pub fn output_schema() -> Arc<rmcp::model::JsonObject> {
    rmcp::handler::server::tool::schema_for_output::<JobStatus>()
}

/// Reject a `wait_seconds` above the maximum with an error that says what to do instead.
pub fn validate_wait(job_id: u64, wait_seconds: u64) -> Result<Duration, Box<JobStatus>> {
    if wait_seconds > MAX_WAIT_SECONDS {
        return Err(Box::new(JobStatus::failure(
            job_id,
            JobErrorCode::InvalidWaitSeconds,
            format!(
                "wait_seconds must be between 0 and {MAX_WAIT_SECONDS} (got {wait_seconds}). \
                 To wait longer, call get_job_status again with the previous reply's next_cursor: \
                 a wait only waits and never stops the job."
            ),
        )));
    }
    Ok(Duration::from_secs(wait_seconds))
}

/// The words for a state, shared by get_job_status's text and list_jobs's.
fn state_label(state: JobState, exit_code: Option<i64>, stop: Option<StopReason>) -> String {
    match state {
        JobState::Queued => "queued (launched, not running yet)".to_string(),
        JobState::Running => "running".to_string(),
        JobState::Exited => format!("exited {}", exit_code.unwrap_or(-1)),
        JobState::Stopped => match stop {
            Some(StopReason::Timeout) => "stopped at its run_command timeout".to_string(),
            Some(StopReason::RequestCancelled) => {
                "stopped when its run_command request was cancelled or its MCP session ended"
                    .to_string()
            }
            _ => "stopped by stop_job".to_string(),
        },
        JobState::Ended => "ended without an exit code (killed, or the VM restarted)".to_string(),
        JobState::NotStarted => "not started".to_string(),
        JobState::Missing => "no such job".to_string(),
        JobState::Unknown => "unknown".to_string(),
    }
}

/// The text a client without structured content reads: the legacy `STATE:` line and log tail
/// (or, for a cursor read, the new output), then one line per fact worth knowing.
pub fn render_text(s: &JobStatus) -> String {
    if let Some(e) = &s.error {
        return e.clone();
    }
    let label = state_label(s.state, s.exit_code, s.stop_reason);
    let mut out = format!("STATE: {label}");
    if s.kind == Some(JobKind::Foreground) {
        out.push('\n');
        out.push_str(FOREGROUND_NOTE);
    } else if s.state != JobState::Missing {
        if let Some(tail) = &s.log_tail {
            out.push_str("\n--- log tail ---\n");
            out.push_str(tail.trim_end());
        } else if let Some(o) = &s.output {
            out.push_str(&format!("\n--- output ({} bytes) ---\n", s.output_bytes));
            out.push_str(o.trim_end_matches('\n'));
        }
    }
    let mut facts: Vec<String> = Vec::new();
    if let Some(c) = &s.next_cursor {
        facts.push(format!("next_cursor: {c}"));
    }
    if let Some(r) = s.cursor_reset_reason {
        facts.push(format!(
            "cursor_reset: true ({}): the log no longer matches the cursor, so this output \
             restarts at the beginning of the log",
            r.name()
        ));
    }
    if s.has_more {
        facts.push("more output is waiting: call again with next_cursor".to_string());
    } else if s.output_truncated {
        facts.push(format!(
            "output_truncated: the log is longer than the {REPLY_BYTES}-byte reply cap; only its \
             end is shown"
        ));
    }
    if s.invalid_utf8 {
        facts.push("invalid_utf8: some bytes were not valid UTF-8 and show as U+FFFD".to_string());
    }
    if s.wait_timed_out {
        facts.push(format!(
            "wait_timed_out: the job is still {} after {} ms; it was not touched",
            if s.state == JobState::Missing {
                "unrecorded"
            } else {
                "going"
            },
            s.waited_ms
        ));
    }
    if let Some(n) = &s.note
        && (s.kind != Some(JobKind::Foreground) || s.state == JobState::Unknown)
    {
        facts.push(n.clone());
    }
    if !facts.is_empty() {
        out.push_str("\n\n");
        out.push_str(&facts.join("\n"));
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Cursors

/// A position in a job's log, bound to the VM it came from (name and generation), the job,
/// the guest boot, the log file (its identity and the fingerprint of its first bytes) and a
/// byte offset. It is metadata, not a capability: nothing here is secret or signed, and a
/// forged one can only read the job's own log at another offset.
///
/// Reset detection is best effort, not universal. It cannot see: a guest clock step across
/// the Windows boot estimate (made from `TickCount`, which also wraps every 49.7 days, and
/// compared within 60 s); a Windows log recreated under the same name within the file
/// system's tunneling window, which can keep its creation time; a log truncated and
/// rewritten in place whose first 64 bytes are identical and which is at least as long as
/// the offset. Log identity and offset checks catch the rest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub vm: String,
    pub generation: String,
    pub job: u64,
    pub boot: String,
    pub log: String,
    pub head_len: u32,
    pub head_hash: String,
    pub offset: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resume {
    /// `cursor: "start"`: from the beginning of the log, whatever it is.
    Start,
    At(Cursor),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    v: u8,
    vm: String,
    g: String,
    j: u64,
    b: String,
    l: String,
    hl: u32,
    hh: String,
    o: u64,
}

const WIRE_VERSION: u8 = 1;
const TOKEN_MAX: usize = 64;
const MAX_OFFSET: u64 = 1 << 53;

fn token_ok(s: &str) -> bool {
    s.len() <= TOKEN_MAX
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '.' | '_' | '-'))
}

fn hash_ok(s: &str) -> bool {
    s.len() == 16 && s.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}

impl Cursor {
    /// The cursor string: base64url (no padding) of a small JSON object.
    pub fn encode(&self) -> String {
        let wire = Wire {
            v: WIRE_VERSION,
            vm: self.vm.clone(),
            g: self.generation.clone(),
            j: self.job,
            b: self.boot.clone(),
            l: self.log.clone(),
            hl: self.head_len,
            hh: self.head_hash.clone(),
            o: self.offset,
        };
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&wire).expect("cursor serializes"))
    }
}

/// Decode and check a `cursor` argument for job `job` of VM `vm`. The error says what is wrong
/// and what to pass instead.
pub fn parse_cursor(s: &str, vm: &str, job: u64) -> Result<Resume, String> {
    const HINT: &str =
        "Omit cursor to read the end of the log, or pass \"start\" for its beginning.";
    if s == "start" {
        return Ok(Resume::Start);
    }
    if s.is_empty() || s.len() > CURSOR_MAX_CHARS {
        return Err(format!(
            "cursor must be a next_cursor value from get_job_status (1 to {CURSOR_MAX_CHARS} \
             characters) or \"start\". {HINT}"
        ));
    }
    let malformed = || format!("cursor is not one get_job_status issued (malformed). {HINT}");
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .map_err(|_| malformed())?;
    if raw.len() > CURSOR_MAX_CHARS {
        return Err(malformed());
    }
    let w: Wire = serde_json::from_slice(&raw).map_err(|_| malformed())?;
    if w.v != WIRE_VERSION
        || ![&w.vm, &w.g, &w.b, &w.l].iter().all(|t| token_ok(t))
        || w.vm.is_empty()
        || w.hl as usize > HEAD_BYTES
        || (w.hl > 0) != hash_ok(&w.hh)
        || (w.hh.is_empty() && w.hl > 0)
        || w.o > MAX_OFFSET
        || (w.l.is_empty() && (w.o > 0 || w.hl > 0))
    {
        return Err(malformed());
    }
    if w.vm != vm {
        return Err(format!(
            "this cursor belongs to VM '{}', not '{vm}'. Use the next_cursor from a reply about \
             '{vm}', or pass \"start\".",
            w.vm
        ));
    }
    if w.j != job {
        return Err(format!(
            "this cursor belongs to job {}, not job {job}. Use the next_cursor from a reply about \
             job {job}, or pass \"start\".",
            w.j
        ));
    }
    Ok(Resume::At(Cursor {
        vm: w.vm,
        generation: w.g,
        job: w.j,
        boot: w.b,
        log: w.l,
        head_len: w.hl,
        head_hash: w.hh,
        offset: w.o,
    }))
}

/// What identifies this incarnation of a VM's disk: its slot, when its directory was created,
/// and the disk file's inode and creation time. `reset_vm` and `restore_vm` replace the disk
/// file, and a deleted and recreated VM has a new directory, so a cursor from before either
/// no longer matches. Not secret, and nothing is stored: it is read from the file system.
pub fn vm_generation(dir: &Path, slot: u16) -> String {
    use std::os::unix::fs::MetadataExt;
    let birth = |p: &Path| {
        std::fs::metadata(p)
            .ok()
            .and_then(|m| m.created().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs())
    };
    let disk = dir.join("disk.qcow2");
    let ino = std::fs::metadata(&disk).map_or(0, |m| m.ino());
    format!("{slot}:{}:{ino}:{}", birth(dir), birth(&disk))
}

// ---------------------------------------------------------------------------------------------
// Request and guest query

/// One get_job_status call, validated.
#[derive(Clone, Debug)]
pub struct Request {
    pub name: String,
    pub id: u64,
    pub tail_lines: usize,
    pub wait: Duration,
    /// None: the tail mode of a call without a cursor.
    pub resume: Option<Resume>,
    /// The VM's generation now (see `vm_generation`).
    pub generation: String,
    /// The most log bytes this reply may carry.
    pub cap: usize,
}

impl Request {
    pub fn new(
        name: &str,
        id: u64,
        tail_lines: usize,
        wait: Duration,
        resume: Option<Resume>,
        generation: String,
    ) -> Self {
        Self {
            name: name.to_string(),
            id,
            tail_lines,
            wait,
            resume,
            generation,
            cap: REPLY_BYTES,
        }
    }

    /// The cursor's bound position, when it still applies: an unbound cursor (nothing read
    /// yet) and `start` mean the beginning, and one from another VM generation points into a
    /// disk that is gone.
    fn bound(&self) -> Option<&Cursor> {
        match &self.resume {
            Some(Resume::At(c)) if !c.log.is_empty() => Some(c),
            _ => None,
        }
    }

    fn forced_reset(&self) -> Option<ResetReason> {
        self.bound()
            .filter(|c| c.generation != self.generation)
            .map(|_| ResetReason::VmChanged)
    }

    pub fn query(&self) -> Query {
        let mode = match (&self.resume, self.bound()) {
            (None, _) => Mode::Tail,
            (Some(_), Some(c)) if c.generation == self.generation => Mode::Read(Expect {
                boot: c.boot.clone(),
                log: c.log.clone(),
                head_len: c.head_len,
                head_hash: c.head_hash.clone(),
                offset: c.offset,
            }),
            (Some(_), _) => Mode::Read(Expect::default()),
        };
        Query {
            mode,
            cap: self.cap,
        }
    }
}

/// What the guest is asked for.
#[derive(Clone, Debug)]
pub struct Query {
    pub mode: Mode,
    pub cap: usize,
}

#[derive(Clone, Debug)]
pub enum Mode {
    /// The last `cap` bytes of the log, with the identity and size they were read at.
    Tail,
    /// From a position, if the log is still the one it came from; else from the start.
    Read(Expect),
}

/// What a cursor expects of the log. Empty strings and zero mean "no expectation", the
/// state of a read from the start.
#[derive(Clone, Debug, Default)]
pub struct Expect {
    pub boot: String,
    pub log: String,
    pub head_len: u32,
    pub head_hash: String,
    pub offset: u64,
}

// ---------------------------------------------------------------------------------------------
// The guest report

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LogPresence {
    /// Not applicable (a foreground execution, or no record at all).
    #[default]
    None,
    Absent,
    Present,
    Unreadable,
}

/// The facts a status script printed, as `AGENTPC_JOB <key> <value>` lines.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GuestReport {
    pub state: JobState,
    pub kind: Option<JobKind>,
    pub exit_code: Option<i64>,
    pub stop: Option<StopReason>,
    pub boot: String,
    pub log: LogPresence,
    pub log_id: String,
    pub size: u64,
    pub head_len: u32,
    pub head_hash: String,
    pub reset: Option<ResetReason>,
    /// Where `data` starts in the log.
    pub start: u64,
    pub data: Vec<u8>,
}

/// Parse a status script's output. Only lines that begin with the marker count, and the report
/// is complete only when its `end` line arrived, so a connection that died midway is an error
/// instead of a state.
pub fn parse_report(out: &[u8]) -> Result<GuestReport, String> {
    let text = String::from_utf8_lossy(out);
    let mut r = GuestReport::default();
    let (mut state, mut end, mut reset_flag, mut why) = (None, false, false, None);
    for line in text.lines() {
        let Some(rest) = line.trim_end().strip_prefix("AGENTPC_JOB ") else {
            continue;
        };
        let (key, val) = match rest.split_once(' ') {
            Some((k, v)) => (k, v.trim()),
            None => (rest.trim(), ""),
        };
        let num = |v: &str| v.parse::<u64>().map_err(|_| format!("bad {key} '{v}'"));
        match key {
            "state" => {
                state = Some(match val {
                    "queued" => JobState::Queued,
                    "running" => JobState::Running,
                    "exited" => JobState::Exited,
                    "stopped" => JobState::Stopped,
                    "ended" => JobState::Ended,
                    "not_started" => JobState::NotStarted,
                    "missing" => JobState::Missing,
                    "unknown" => JobState::Unknown,
                    other => return Err(format!("unknown state '{other}'")),
                })
            }
            "kind" => {
                r.kind = Some(match val {
                    "background" => JobKind::Background,
                    "foreground" => JobKind::Foreground,
                    other => return Err(format!("unknown kind '{other}'")),
                })
            }
            "exit" => {
                r.exit_code = Some(val.parse().map_err(|_| format!("bad exit code '{val}'"))?)
            }
            "stop" => {
                r.stop = Some(match val {
                    "timeout" => StopReason::Timeout,
                    "request_cancelled" => StopReason::RequestCancelled,
                    _ => StopReason::StopJob,
                })
            }
            "boot" => r.boot = val.to_string(),
            "log" => {
                r.log = match val {
                    "present" => LogPresence::Present,
                    "absent" => LogPresence::Absent,
                    "none" => LogPresence::None,
                    "unreadable" => LogPresence::Unreadable,
                    other => return Err(format!("unknown log '{other}'")),
                }
            }
            "logid" => r.log_id = val.to_string(),
            "size" => r.size = num(val)?,
            "head" => {
                let (l, h) = val.split_once(' ').unwrap_or((val, ""));
                r.head_len = l.parse().map_err(|_| format!("bad head '{val}'"))?;
                r.head_hash = h.trim().to_string();
            }
            "reset" => reset_flag = val == "1",
            "why" => why = ResetReason::parse(val),
            "start" => r.start = num(val)?,
            "data" => {
                r.data = base64::engine::general_purpose::STANDARD
                    .decode(val)
                    .map_err(|_| "the log data was not valid base64".to_string())?
            }
            "end" => end = true,
            _ => {}
        }
    }
    if !end {
        return Err("the report was cut off before its end".to_string());
    }
    r.state = state.ok_or("the report has no state")?;
    if r.state == JobState::Exited && r.exit_code.is_none() {
        return Err("the job exited but no exit code was reported".to_string());
    }
    if reset_flag {
        r.reset = Some(why.ok_or("a reset without a reason")?);
    }
    if r.log == LogPresence::Present {
        if r.log_id.is_empty() || !token_ok(&r.log_id) {
            return Err("the log has no usable identity".to_string());
        }
        if r.data.len() > REPLY_BYTES || r.start + r.data.len() as u64 > r.size {
            return Err("the log data does not fit the log".to_string());
        }
        if r.head_len as usize > HEAD_BYTES || (r.head_len > 0) != hash_ok(&r.head_hash) {
            return Err("the log fingerprint is malformed".to_string());
        }
        if !token_ok(&r.boot) {
            return Err("the boot id is malformed".to_string());
        }
    }
    Ok(r)
}

// ---------------------------------------------------------------------------------------------
// Decoding log bytes

/// Log bytes as text.
#[derive(Debug, PartialEq)]
pub struct Decoded {
    pub text: String,
    /// How many of the bytes the text accounts for. What is left over (a character cut by the
    /// end of the chunk) is read again next time.
    pub consumed: usize,
    /// Whether some bytes were not valid UTF-8 and show as U+FFFD.
    pub invalid: bool,
}

const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// Decode `data`, which starts at the beginning of the log when `at_start` (a UTF-8 byte-order
/// mark there is consumed but not shown: Windows PowerShell writes one). A character cut by
/// the end of the chunk is left unconsumed, unless `last` says nothing more can follow it
/// (the job is over and this reached the end of the log): then it counts as invalid.
pub fn decode(data: &[u8], at_start: bool, last: bool) -> Decoded {
    let mut text = String::new();
    let (mut consumed, mut invalid) = (0usize, false);
    let mut rest = data;
    if at_start && rest.starts_with(&BOM) {
        rest = &rest[3..];
        consumed = 3;
    }
    loop {
        match std::str::from_utf8(rest) {
            Ok(s) => {
                text.push_str(s);
                consumed += rest.len();
                break;
            }
            Err(e) => {
                let ok = e.valid_up_to();
                text.push_str(std::str::from_utf8(&rest[..ok]).expect("valid prefix"));
                consumed += ok;
                rest = &rest[ok..];
                match e.error_len() {
                    Some(n) => {
                        text.push('\u{FFFD}');
                        invalid = true;
                        consumed += n;
                        rest = &rest[n..];
                    }
                    None => {
                        if last {
                            text.push('\u{FFFD}');
                            invalid = true;
                            consumed += rest.len();
                        }
                        break;
                    }
                }
            }
        }
    }
    Decoded {
        text,
        consumed,
        invalid,
    }
}

/// The last `n` lines of `text` (kept whole, line ends included), and whether lines are
/// missing from it that the byte cap cut: the window began mid-log, so its first line may be
/// partial, which only matters when fewer than `n` lines were found.
fn last_lines(text: &str, n: usize, window_cut: bool) -> (String, bool) {
    if n == 0 {
        return (String::new(), false);
    }
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    if lines.len() > n {
        (lines[lines.len() - n..].concat(), false)
    } else {
        (text.to_string(), window_cut)
    }
}

/// Turn a guest report into the status for `req`.
pub fn assemble(req: &Request, rep: &GuestReport) -> JobStatus {
    let mut s = JobStatus::base(req.id, rep.state);
    s.kind = rep.kind;
    s.exit_code = rep.exit_code;
    s.stop_reason = rep.stop;
    let cursor_mode = req.resume.is_some();
    if rep.state == JobState::Unknown {
        s.note = Some(UNKNOWN_NOTE.to_string());
    }
    if rep.kind == Some(JobKind::Foreground) {
        // The code run_command reports for this execution: the recorded one as an SSH client
        // sees it, its low 8 bits (a Windows `exit -1` or `exit 511` reads 255).
        s.exit_code = rep.exit_code.map(|c| c & 0xff);
        if rep.state != JobState::Unknown {
            s.note = Some(FOREGROUND_NOTE.trim_matches(['(', ')']).to_string());
        }
        return s;
    }
    if rep.state == JobState::Missing {
        s.note = Some(format!(
            "No record of job {} in this VM (yet). A background launch whose connection was \
             lost may still arrive, so poll again before doing anything a second time; a wrong \
             id or a reset VM never will.",
            req.id
        ));
    }
    let forced = req.forced_reset();
    let bound = req.bound().is_some();
    let next = |offset: u64, rep: &GuestReport| {
        let unbound = rep.log != LogPresence::Present;
        Cursor {
            vm: req.name.clone(),
            generation: req.generation.clone(),
            job: req.id,
            boot: rep.boot.clone(),
            log: if unbound {
                String::new()
            } else {
                rep.log_id.clone()
            },
            head_len: if unbound { 0 } else { rep.head_len },
            head_hash: if unbound {
                String::new()
            } else {
                rep.head_hash.clone()
            },
            offset: if unbound { 0 } else { offset },
        }
        .encode()
    };
    if rep.log != LogPresence::Present {
        // Queued, missing, or a log that cannot be read: no text, and a cursor that starts
        // at the beginning of whatever log appears.
        if rep.log == LogPresence::Unreadable {
            // Appended: an invalid exit record's explanation (state unknown, not ended) stays.
            let unreadable = "The job's log exists but could not be read just now.";
            s.note = Some(match s.note.take() {
                Some(earlier) => format!("{earlier} {unreadable}"),
                None => unreadable.to_string(),
            });
        }
        if bound {
            s.cursor_reset = true;
            s.cursor_reset_reason = forced.or(Some(ResetReason::LogMissing));
        }
        if cursor_mode {
            s.output = Some(String::new());
        } else {
            s.log_tail = Some(String::new());
        }
        s.next_cursor = Some(next(0, rep));
        return s;
    }

    s.log_available = true;
    let end_of_log = rep.start + rep.data.len() as u64 == rep.size;
    let last = rep.state.terminal() && end_of_log;
    let new_offset;
    if cursor_mode {
        let d = decode(&rep.data, rep.start == 0, last);
        new_offset = rep.start + d.consumed as u64;
        s.output_bytes = d.consumed as u64;
        s.invalid_utf8 = d.invalid;
        s.has_more = !end_of_log;
        s.output_truncated = s.has_more;
        s.output = Some(d.text);
        if let Some(why) = forced.or(rep.reset) {
            s.cursor_reset = true;
            s.cursor_reset_reason = Some(why);
        }
    } else {
        // The window may begin in the middle of a character: skip its tail end.
        let mut skip = 0;
        if rep.start > 0 {
            while skip < 3 && rep.data.get(skip).is_some_and(|b| (0x80..0xC0).contains(b)) {
                skip += 1;
            }
        }
        let start = rep.start + skip as u64;
        let d = decode(&rep.data[skip..], start == 0, last);
        new_offset = start + d.consumed as u64;
        let (text, cut) = last_lines(&d.text, req.tail_lines, rep.start > 0);
        s.output_bytes = d.consumed as u64;
        s.invalid_utf8 = d.invalid;
        s.output_truncated = cut;
        s.log_tail = Some(text);
    }
    s.next_cursor = Some(next(new_offset, rep));
    s
}

// ---------------------------------------------------------------------------------------------
// Waiting

/// Why a status check produced no report.
#[derive(Clone, Debug)]
pub struct Failure {
    pub code: JobErrorCode,
    pub message: String,
    /// What went wrong, without the surrounding advice (for callers with their own).
    pub detail: String,
    /// The check ran out of time (as opposed to failing outright): an earlier good snapshot
    /// of a wait stays the answer.
    pub timed_out: bool,
}

impl Failure {
    pub fn unreachable(detail: &str) -> Self {
        Self {
            code: JobErrorCode::GuestUnreachable,
            message: format!(
                "Could not read the job's state: the VM did not answer ({detail}). Its state is \
                 unknown, not ended. Check list_vms, then call get_job_status again."
            ),
            detail: detail.to_string(),
            timed_out: false,
        }
    }

    /// The VM did not answer in the time given.
    pub fn timeout(detail: &str) -> Self {
        Self {
            timed_out: true,
            ..Self::unreachable(detail)
        }
    }

    pub fn bad_reply(detail: &str) -> Self {
        Self {
            code: JobErrorCode::BadGuestReply,
            message: format!(
                "The VM answered, but not with a job status this server understands ({detail}). \
                 The job's state is unknown; call get_job_status again."
            ),
            detail: detail.to_string(),
            timed_out: false,
        }
    }
}

/// Whether there is nothing to wait for: the job is over, a failure came back, or (with a
/// cursor) there is output or a reset to report.
fn settled(s: &JobStatus, cursor_mode: bool) -> bool {
    s.state.terminal()
        || s.state == JobState::Unknown
        || s.error.is_some()
        || (cursor_mode && (s.output_bytes > 0 || s.cursor_reset))
}

/// Ask the guest (through `fetch`, which gets the query and the time the check may take) about
/// the job, and keep asking until it settles or the wait ends. Waiting only waits: nothing here
/// touches the job. Time is Tokio's monotonic clock and every pause is an async sleep, so
/// nothing blocks a runtime thread.
///
/// A check gets the time left in the wait plus `pacing.transport`, capped at the ceiling, and
/// is abandoned `pacing.grace` after that, so a hung SSH round trip near the deadline cannot
/// stretch the call beyond the wait plus those two allowances. A check that fails during a
/// wait is retried while time remains. At the end: a check that ran out of time keeps the
/// last good snapshot (flagged `wait_timed_out`, with a note), a check that failed outright
/// or a wait that never got a snapshot reports the guest as unavailable (state unknown, never
/// guessed).
pub async fn observe<F, Fut>(req: &Request, pacing: Pacing, mut fetch: F) -> JobStatus
where
    F: FnMut(&Query, Duration) -> Fut,
    Fut: Future<Output = Result<GuestReport, Failure>>,
{
    let started = Instant::now();
    let deadline = started + req.wait;
    let waiting = !req.wait.is_zero();
    let query = req.query();
    let cursor_mode = req.resume.is_some();
    let mut last_good: Option<JobStatus> = None;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let budget = if waiting {
            (remaining + pacing.transport).min(pacing.ceiling)
        } else {
            pacing.ceiling
        };
        let result = match tokio::time::timeout(budget + pacing.grace, fetch(&query, budget)).await
        {
            Ok(r) => r,
            Err(_) => Err(Failure::timeout(&format!(
                "no answer within {:.1} s",
                budget.as_secs_f64()
            ))),
        };
        let expired = Instant::now() >= deadline;
        match result {
            Ok(rep) => {
                let mut st = assemble(req, &rep);
                let done = settled(&st, cursor_mode);
                if done || !waiting || expired {
                    st.wait_timed_out = waiting && !done;
                    st.waited_ms = started.elapsed().as_millis() as u64;
                    return st;
                }
                last_good = Some(st);
            }
            Err(f) => {
                if !waiting || expired {
                    let waited_ms = started.elapsed().as_millis() as u64;
                    if let (true, Some(mut st)) = (f.timed_out, last_good.take()) {
                        st.wait_timed_out = true;
                        st.waited_ms = waited_ms;
                        let late = "The last check, at the end of the wait, did not answer in time, \
                                    so this is the state from an earlier check during the wait.";
                        st.note = Some(match st.note.take() {
                            Some(n) => format!("{n} {late}"),
                            None => late.to_string(),
                        });
                        return st;
                    }
                    let mut st = JobStatus::failure(req.id, f.code, f.message);
                    st.waited_ms = waited_ms;
                    return st;
                }
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        tokio::time::sleep(pacing.poll.min(remaining)).await;
    }
}

// ---------------------------------------------------------------------------------------------
// Guest scripts (read-only)

/// A token from a cursor as it goes into a script: it was validated when the cursor was
/// parsed, and anything else becomes a string that matches nothing (so it resets).
fn token(s: &str) -> String {
    if token_ok(s) {
        s.to_string()
    } else {
        "invalid-token".to_string()
    }
}

struct Params {
    boot: String,
    log: String,
    head_len: u32,
    head_hash: String,
    offset: u64,
}

fn params(e: &Expect) -> Params {
    Params {
        boot: token(&e.boot),
        log: token(&e.log),
        head_len: e.head_len.min(HEAD_BYTES as u32),
        head_hash: if hash_ok(&e.head_hash) || e.head_hash.is_empty() {
            e.head_hash.clone()
        } else {
            "invalid-hash".to_string()
        },
        offset: e.offset.min(MAX_OFFSET),
    }
}

/// Where the Linux script finds things; tests on other systems point these elsewhere.
#[derive(Clone, Debug)]
pub struct LinuxPaths {
    /// The log opened on descriptor 3, as a path that reopens it (the same file even if
    /// the name was replaced meanwhile).
    pub fd: String,
    pub boot_id: String,
}

impl Default for LinuxPaths {
    fn default() -> Self {
        Self {
            fd: "/proc/self/fd/3".to_string(),
            boot_id: "/proc/sys/kernel/random/boot_id".to_string(),
        }
    }
}

/// Validates `$x` (the text of an exit record, whitespace removed) as an exit code: ASCII digits
/// only, an optional leading minus, at most ten digits and no more than 4294967295 in value.
/// Anything else (`-`, `1-2`, a huge or non-ASCII number) is dropped and sets `xbad=1`, so the
/// job reports `unknown` instead of an exit code nobody wrote.
const LINUX_XCHECK: &str = r#"xbad=0
if [ -n "$x" ]; then
  y=${x#-}
  case $y in
    ''|*[!0123456789]*) x=; xbad=1 ;;
    *) if [ ${#y} -gt 10 ] || { [ ${#y} -eq 10 ] && [ "$y" -gt 4294967295 ]; }; then x=; xbad=1; fi ;;
  esac
fi"#;

/// The state decision of a Linux job, shared by get_job_status and list_jobs so the two never
/// disagree. Reads the variables `d`, `fg`, `p`, `x` and the function `own`, and writes only
/// `emit` lines.
const LINUX_STATE: &str = r#"if [ "$xbad" = 1 ]; then emit state unknown
elif [ -n "$x" ]; then emit state exited; emit exit "$x"
elif own; then emit state running
elif [ -f $d/__ID__.stopped ]; then
  emit state stopped
  if grep -qs request_cancelled $d/__ID__.stopped; then emit stop request_cancelled
  elif grep -qs timeout $d/__ID__.stopped; then emit stop timeout; else emit stop stop_job; fi
elif [ -z "$p" ]; then
  if [ $fg = 1 ] && [ -e $d/__ID__.cancel ]; then emit state not_started; else emit state queued; fi
elif [ $fg = 1 ] && [ ! -f $d/__ID__.proof ] && ps -p "$p" >/dev/null 2>&1; then emit state queued
else emit state ended; fi"#;

const LINUX_HEAD: &str = r#"d=~/agentpc-bg
emit() { printf 'AGENTPC_JOB %s\n' "$*"; }
have=0
for e in log fg pid exit stopped; do [ -e $d/__ID__.$e ] && have=1; done
if [ $have = 0 ]; then emit state missing; emit end ok; exit 0; fi
fg=0; [ -f $d/__ID__.fg ] && fg=1
if [ $fg = 1 ]; then emit kind foreground; else emit kind background; fi
p=$(cat $d/__ID__.pid 2>/dev/null); case $p in ''|*[!0-9]*) p= ;; esac
__OWN__
x=; [ -f $d/__ID__.exit ] && x=$(tr -d ' \r\n\t' < $d/__ID__.exit 2>/dev/null)
__XCHECK__
boot=$(cat __BOOT__ 2>/dev/null)
emit boot "$boot"
__STATE__
if [ $fg = 1 ]; then emit log none; emit end ok; exit 0; fi
f=$d/__ID__.log
if [ -f "$f" ] && [ -r "$f" ] && exec 3< "$f"; then
  fd=__FD__
  S=$(stat -L -c %s $fd 2>/dev/null); lid=$(stat -L -c %d:%i $fd 2>/dev/null)
  case $S in ''|*[!0-9]*) S= ;; esac
  if [ -z "$S" ] || [ -z "$lid" ]; then emit log unreadable; emit end ok; exit 0; fi
  hl=$S; [ $hl -gt 64 ] && hl=64
  hh=; [ $hl -gt 0 ] && hh=$(head -c $hl $fd | sha256sum | cut -c1-16)
  emit log present; emit logid "$lid"; emit size "$S"; emit head "$hl $hh"
__SECTION__
  emit start $start
  emit data "$(tail -c +$((start+1)) $fd | head -c $n | base64 -w0)"
elif [ -e "$f" ]; then emit log unreadable
else emit log absent; fi
emit end ok"#;

const LINUX_TAIL: &str = r#"  w=$S; [ $w -gt __CAP__ ] && w=__CAP__
  start=$((S-w)); n=$w"#;

const LINUX_READ: &str = r#"  eb='__EB__'; el='__EL__'; eo=__EO__; ehl=__EHL__; ehh='__EHH__'
  reset=0; why=
  if [ -n "$eb" ] && [ "$eb" != "$boot" ]; then reset=1; why=reboot
  elif [ -n "$el" ] && [ "$el" != "$lid" ]; then reset=1; why=log_replaced
  elif [ $eo -gt $S ] || [ $ehl -gt $S ]; then reset=1; why=log_shrunk
  elif [ $ehl -gt 0 ] && [ "$(head -c $ehl $fd | sha256sum | cut -c1-16)" != "$ehh" ]; then reset=1; why=log_rewritten
  fi
  start=$eo; [ $reset = 1 ] && start=0
  n=$((S-start)); [ $n -gt __CAP__ ] && n=__CAP__
  emit reset $reset
  [ -n "$why" ] && emit why $why"#;

/// get_job_status's bash for job `id`. It only reads: no file is created, changed or removed.
pub fn linux_script(id: u64, q: &Query) -> String {
    linux_script_with(id, q, &LinuxPaths::default())
}

pub fn linux_script_with(id: u64, q: &Query, paths: &LinuxPaths) -> String {
    let cap = q.cap.to_string();
    let section = match &q.mode {
        Mode::Tail => fill(LINUX_TAIL, &[("__CAP__", &cap)]),
        Mode::Read(e) => {
            let p = params(e);
            fill(
                LINUX_READ,
                &[
                    ("__EB__", &p.boot),
                    ("__EL__", &p.log),
                    ("__EO__", &p.offset.to_string()),
                    ("__EHL__", &p.head_len.to_string()),
                    ("__EHH__", &p.head_hash),
                    ("__CAP__", &cap),
                ],
            )
        }
    };
    fill(
        LINUX_HEAD,
        &[
            ("__OWN__", &linux_own(id)),
            ("__XCHECK__", LINUX_XCHECK),
            (
                "__STATE__",
                &fill(LINUX_STATE, &[("__ID__", &id.to_string())]),
            ),
            ("__BOOT__", &token_path(&paths.boot_id)),
            ("__FD__", &token_path(&paths.fd)),
            ("__CAP__", &cap),
            ("__ID__", &id.to_string()),
            ("__SECTION__", &section),
        ],
    )
}

/// Fill a template's `__NAME__` placeholders in one pass: a value is copied as it is and never
/// scanned again, so text that looks like another placeholder (a cursor field may; it is a
/// valid token) can neither expand nor break the script. Unknown `__` text is left alone.
fn fill(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len() + 1024);
    let mut rest = template;
    while let Some(at) = rest.find("__") {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        match values.iter().find(|(k, _)| tail.starts_with(k)) {
            Some((k, v)) => {
                out.push_str(v);
                rest = &tail[k.len()..];
            }
            None => {
                out.push_str("__");
                rest = &tail[2..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// A path that goes into a script unquoted: only the characters of a plain path.
fn token_path(p: &str) -> String {
    if p.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | ':'))
    {
        p.to_string()
    } else {
        "/nonexistent".to_string()
    }
}

/// Windows's counterpart of `LINUX_XCHECK` (`[0-9]`, not `\d`, which matches other scripts'
/// digits too).
const WINDOWS_XCHECK: &str = r#"$xbad = $false
if ($x -ne '') { if (-not (($x -cmatch '^-?[0-9]{1,10}$') -and ([math]::Abs([int64]$x) -le 4294967295))) { $x = ''; $xbad = $true } }"#;

/// The state decision of a Windows job, shared by get_job_status and list_jobs. Reads `$d`,
/// `$fg`, `$x`, `$t`, `$tstate`, `$ran`, `$log` and the function `Own`, and writes only `Emit`
/// lines.
const WINDOWS_STATE: &str = r#"if ($xbad) { Emit state unknown }
elseif ($x -ne '') { Emit state exited; Emit exit $x }
elseif ($fg -and (Own)) { Emit state running }
elseif ((-not $fg) -and $t -and $tstate -ne 'Disabled' -and ($tstate -eq 'Running' -or $tstate -eq 'Queued' -or -not $ran)) { if ($tstate -eq 'Running' -or [IO.File]::Exists($log)) { Emit state running } else { Emit state queued } }
elseif ([IO.File]::Exists("$d\__ID__.stopped")) { Emit state stopped; $why = ''; try { $why = [IO.File]::ReadAllText("$d\__ID__.stopped") } catch { }; if ($why -match 'request_cancelled') { Emit stop request_cancelled } elseif ($why -match 'timeout') { Emit stop timeout } else { Emit stop stop_job } }
elseif ($fg -and -not [IO.File]::Exists("$d\__ID__.pid")) { if ([IO.File]::Exists("$d\__ID__.cancel")) { Emit state not_started } else { Emit state queued } }
elseif ($fg -and "$(try { [IO.File]::ReadAllText("$d\__ID__.pid") } catch { })".Trim() -notmatch '^\d+ \d+$') { Emit state queued }
else { Emit state ended }"#;

const WINDOWS_HEAD: &str = r#"$d = "$env:USERPROFILE\agentpc-bg"
function Emit($k, $v) { "AGENTPC_JOB $k $v" }
$log = "$d\__ID__.log"; $exitp = "$d\__ID__.exit"
$fg = [IO.File]::Exists("$d\__ID__.fg")
$t = $null
if (-not $fg) { $t = Get-ScheduledTask -TaskName 'agentpc-bg-__ID__' -ErrorAction SilentlyContinue }
if (-not ($fg -or $t -or [IO.File]::Exists($log) -or [IO.File]::Exists($exitp) -or [IO.File]::Exists("$d\__ID__.stopped") -or [IO.File]::Exists("$d\__ID__.pid"))) { Emit state missing; Emit end ok; exit 0 }
if ($fg) { Emit kind foreground } else { Emit kind background }
__OWN__
$boot = ''
try { $tc = [Environment]::TickCount; if ($tc -lt 0) { $tc += 4294967296 }; $boot = 'w' + [int64]([DateTimeOffset]::UtcNow.ToUnixTimeSeconds() - [math]::Floor($tc / 1000)) } catch { }
Emit boot $boot
$x = ''
if ([IO.File]::Exists($exitp)) { try { $x = [IO.File]::ReadAllText($exitp).Trim() } catch { } }
__XCHECK__
$ran = $false
if ($t) { try { $ran = (($t | Get-ScheduledTaskInfo).LastRunTime.Year -gt 2000) } catch { } }
$tstate = ''
if ($t) { $tstate = "$($t.State)" }
__STATE__
if ($fg) { Emit log none; Emit end ok; exit 0 }
if (-not [IO.File]::Exists($log)) { Emit log absent; Emit end ok; exit 0 }
function ReadAt($fs, [int64]$pos, [int]$n) {
  $buf = New-Object byte[] $n; $got = 0
  if ($n -gt 0) { [void]$fs.Seek($pos, [IO.SeekOrigin]::Begin) }
  while ($got -lt $n) { $r = $fs.Read($buf, $got, $n - $got); if ($r -le 0) { break }; $got += $r }
  if ($got -lt $n) { $b2 = New-Object byte[] $got; [Array]::Copy($buf, $b2, $got); return ,$b2 }
  return ,$buf
}
function Hex16($bytes) { $h = [Security.Cryptography.SHA256]::Create(); try { ([BitConverter]::ToString($h.ComputeHash($bytes)) -replace '-', '').Substring(0, 16).ToLower() } finally { $h.Dispose() } }
$fs = $null
try {
  $fs = New-Object IO.FileStream($log, [IO.FileMode]::Open, [IO.FileAccess]::Read, ([IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete))
  $S = $fs.Length
  $lid = 'c' + [IO.File]::GetCreationTimeUtc($log).Ticks
  $hl = [int][math]::Min($S, 64)
  $hh = ''; if ($hl -gt 0) { $hh = Hex16 (ReadAt $fs 0 $hl) }
  Emit log present; Emit logid $lid; Emit size $S; Emit head "$hl $hh"
__SECTION__
  Emit start $start
  $bytes = ReadAt $fs $start $n
  Emit data ([Convert]::ToBase64String($bytes))
} catch { Emit log unreadable } finally { if ($fs) { $fs.Dispose() } }
Emit end ok"#;

const WINDOWS_TAIL: &str = r#"  $n = [int][math]::Min($S, __CAP__); $start = $S - $n"#;

const WINDOWS_READ: &str = r#"  $eb = '__EB__'; $el = '__EL__'; $eo = [int64]__EO__; $ehl = [int]__EHL__; $ehh = '__EHH__'
  $reset = 0; $why = ''; $bootDiffers = $false
  if ($eb -and $boot) { try { if ([math]::Abs([int64]$eb.Substring(1) - [int64]$boot.Substring(1)) -gt 60) { $bootDiffers = $true } } catch { $bootDiffers = ($eb -ne $boot) } }
  if ($bootDiffers) { $reset = 1; $why = 'reboot' }
  elseif ($el -and $el -ne $lid) { $reset = 1; $why = 'log_replaced' }
  elseif ($eo -gt $S -or $ehl -gt $S) { $reset = 1; $why = 'log_shrunk' }
  elseif ($ehl -gt 0 -and (Hex16 (ReadAt $fs 0 $ehl)) -ne $ehh) { $reset = 1; $why = 'log_rewritten' }
  $start = $eo; if ($reset -eq 1) { $start = 0 }
  $n = [int][math]::Min($S - $start, __CAP__)
  Emit reset $reset
  if ($why) { Emit why $why }"#;

/// Windows's counterpart of `linux_script` (PowerShell 5.1 compatible): the log is opened
/// shared read/write so the job keeps writing, and nothing is created, changed or removed.
pub fn windows_script(id: u64, q: &Query) -> String {
    let cap = q.cap.to_string();
    let section = match &q.mode {
        Mode::Tail => fill(WINDOWS_TAIL, &[("__CAP__", &cap)]),
        Mode::Read(e) => {
            let p = params(e);
            fill(
                WINDOWS_READ,
                &[
                    ("__EB__", &p.boot),
                    ("__EL__", &p.log),
                    ("__EO__", &p.offset.to_string()),
                    ("__EHL__", &p.head_len.to_string()),
                    ("__EHH__", &p.head_hash),
                    ("__CAP__", &cap),
                ],
            )
        }
    };
    fill(
        WINDOWS_HEAD,
        &[
            ("__OWN__", &windows_own(id)),
            ("__XCHECK__", WINDOWS_XCHECK),
            (
                "__STATE__",
                &fill(WINDOWS_STATE, &[("__ID__", &id.to_string())]),
            ),
            ("__ID__", &id.to_string()),
            ("__SECTION__", &section),
        ],
    )
}

// =============================================================================================
// list_jobs: metadata-only, paginated job discovery
//
// One read-only guest script per OS lists the job records of a VM, newest id first, one page at
// a time, in a single call. Per job it looks at which known record files exist (by name and
// type only), reads the few facts that decide the job's state (a pid, an exit code, a stop
// word: all of them validated numbers or agentpc's own words) with the same rules and the same
// process proof as get_job_status (the state decision is shared text), and takes two file
// modification times. It never opens a launch script (`.sh`, `.ps1`) or a log, never reads a
// command, an environment or any output, and never writes: no tombstone, no probe.
//
// Pages continue by keyset, not by offset: a cursor holds the VM's generation, the newest id
// the traversal began with (its high-water mark) and the last id shown, and the next page is the
// ids below that. A job created after the traversal began has a newer id and never joins it, so
// nothing already shown moves; a job deleted before it is reached simply does not appear. The
// listing is best effort, not an immutable history: records age out.

/// Jobs per page when `limit` is omitted.
pub const LIST_DEFAULT_LIMIT: u64 = 20;

/// The most jobs one page carries.
pub const LIST_MAX_LIMIT: u64 = 100;

/// The most directory entries one listing retains. The guest keeps only the first this many
/// names the file system returns (it does not promise to read no more of the directory than
/// that; the query's deadline bounds the time). A VM with more entries is listed from those,
/// and the reply says it may leave jobs out.
pub const LIST_SCAN_MAX: usize = 20_000;

/// How long the guest script may work on one page before it stops with a cursor to continue
/// (it always reports at least one job when there is one).
pub const LIST_GUEST_BUDGET_SECS: u64 = 20;

/// The longest the whole query (the SSH round trip) may take.
pub const LIST_ATTEMPT: Duration = Duration::from_secs(25);

/// How long past that the query may take to report its own timeout before it is abandoned.
pub const LIST_GRACE: Duration = Duration::from_millis(500);

/// Ids are decimal numbers of at most this many digits (agentpc's are 16): longer names are not
/// ours, and shorter arithmetic in the guest's shell stays exact.
const ID_MAX_DIGITS: usize = 18;

const LIST_HISTORY_NOTE: &str = "Best effort, not an immutable history: records can be added, \
     aged out or deleted between pages. Newest job id first; a page continues below the last id \
     shown, and jobs created after the first page are not part of that traversal.";

#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum ListedKind {
    /// A foreground run_command execution (its output went to that reply: no log).
    Foreground,
    /// A `background: true` job (with a log).
    Background,
    /// The records do not show which.
    Unknown,
}

impl ListedKind {
    fn name(self) -> &'static str {
        match self {
            ListedKind::Foreground => "foreground",
            ListedKind::Background => "background",
            ListedKind::Unknown => "unknown kind",
        }
    }
}

/// A kind of record a job leaves in the VM, by what it is, never by path.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum RecordKind {
    /// The launch script of a background job (Linux). Never read.
    Sh,
    /// The launch script of a background job (Windows). Never read.
    Ps1,
    /// The job's log. Never read by list_jobs.
    Log,
    /// Marks a foreground execution.
    Fg,
    /// The recorded process of the job.
    Pid,
    /// The boot and start time that identify that process (Linux).
    Proof,
    /// The recorded exit code.
    Exit,
    /// Why a stopped job stopped.
    Stopped,
    /// A cancellation tombstone: its command never starts once it exists.
    Cancel,
    /// The job's Windows scheduled task.
    Task,
}

impl RecordKind {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "sh" => RecordKind::Sh,
            "ps1" => RecordKind::Ps1,
            "log" => RecordKind::Log,
            "fg" => RecordKind::Fg,
            "pid" => RecordKind::Pid,
            "proof" => RecordKind::Proof,
            "exit" => RecordKind::Exit,
            "stopped" => RecordKind::Stopped,
            "cancel" => RecordKind::Cancel,
            "task" => RecordKind::Task,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            RecordKind::Sh => "sh",
            RecordKind::Ps1 => "ps1",
            RecordKind::Log => "log",
            RecordKind::Fg => "fg",
            RecordKind::Pid => "pid",
            RecordKind::Proof => "proof",
            RecordKind::Exit => "exit",
            RecordKind::Stopped => "stopped",
            RecordKind::Cancel => "cancel",
            RecordKind::Task => "task",
        }
    }
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum ListErrorCode {
    /// `limit` is not between 1 and 100.
    InvalidLimit,
    /// The cursor is malformed, or belongs to another VM.
    InvalidCursor,
    /// The cursor is from before the VM was reset, restored or recreated; its records may differ now.
    CursorStale,
    NoSuchVm,
    VmNotRunning,
    /// The VM did not answer (SSH failed or timed out): nothing is known about its jobs.
    GuestUnreachable,
    /// The VM answered, but not with a listing this server understands.
    BadGuestReply,
    /// The VM's job directory exists but cannot be read.
    GuestUnreadable,
    /// The request was cancelled; only the listing was abandoned, no job was touched.
    RequestCancelled,
}

/// One job in a listing: identity and state only. It carries no command, environment, output
/// or path.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct JobSummary {
    /// The id get_job_status and stop_job take.
    pub job_id: u64,
    /// `foreground`, `background`, or `unknown` when the records do not show which.
    pub kind: ListedKind,
    /// The state, decided by the same rules as get_job_status (`missing`: only incidental
    /// records exist; `unknown`: a record could not be read).
    pub state: JobState,
    /// The exit code when `state` is `exited` (a foreground one is run_command's: the low 8
    /// bits); null otherwise.
    pub exit_code: Option<i64>,
    /// Why a `stopped` job stopped; null otherwise.
    pub stop_reason: Option<StopReason>,
    /// Whether the job has a log file that can be read (call get_job_status for its text).
    pub log_available: bool,
    /// Which records exist for the job, each kind once.
    pub records: Vec<RecordKind>,
    /// UTC time the job was recorded: the modification time of its foreground marker or launch
    /// script (a file-system time, never derived from the id). Null when no such file exists.
    pub recorded_at: Option<String>,
    /// UTC time the job finished: the modification time of its exit or stop record, for an
    /// `exited` or `stopped` job. Null otherwise.
    pub finished_at: Option<String>,
    /// What to be careful about for an unresolved, incomplete or unreadable record.
    pub note: Option<String>,
}

/// What list_jobs reports, as its structured content. The same object describes every answer
/// and every failure; a failure has `error` set (and is a tool error) and no jobs.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct JobList {
    /// Layout version of this object (currently 1).
    pub schema_version: u32,
    /// The page, newest id first.
    pub jobs: Vec<JobSummary>,
    /// The page size in effect (0 for a failure before one was set).
    pub limit: u32,
    /// Whether older jobs remain below this page.
    pub has_more: bool,
    /// Whether this page continued an earlier one (a `cursor` was given): an empty one then
    /// means no further jobs, not an empty VM.
    pub continuation: bool,
    /// Pass this as `cursor` (with the same `name`) for the next page; null on the last page.
    /// Metadata, not a credential.
    pub next_cursor: Option<String>,
    /// The VM's clock when it listed, in UTC, to read the timestamps against.
    pub listed_at: Option<String>,
    /// Whether the directory holds more entries than one listing retains, so jobs may be left
    /// out.
    pub scan_truncated: bool,
    /// How to read the listing (it is not an immutable history), and what a truncated scan
    /// means.
    pub note: Option<String>,
    /// Why the call failed (null on success), with what to do about it.
    pub error: Option<String>,
    /// The kind of failure.
    pub error_code: Option<ListErrorCode>,
}

impl JobList {
    fn base() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            jobs: Vec::new(),
            limit: 0,
            has_more: false,
            continuation: false,
            next_cursor: None,
            listed_at: None,
            scan_truncated: false,
            note: None,
            error: None,
            error_code: None,
        }
    }

    /// The call failed: no jobs, and the reason.
    pub fn failure(code: ListErrorCode, message: String) -> Self {
        let mut l = Self::base();
        l.error = Some(message);
        l.error_code = Some(code);
        l
    }

    /// The readable text plus the structured listing. The text is the only content block (no
    /// JSON mirror).
    pub fn into_list_result(self) -> CallToolResult {
        let error = self.error.is_some();
        let text = render_list_text(&self);
        let structured = serde_json::to_value(&self).expect("listing serializes");
        let mut r = if error {
            CallToolResult::error(vec![ContentBlock::text(text)])
        } else {
            CallToolResult::success(vec![ContentBlock::text(text)])
        };
        r.structured_content = Some(structured);
        r
    }
}

/// The output schema list_jobs declares.
pub fn list_output_schema() -> Arc<rmcp::model::JsonObject> {
    rmcp::handler::server::tool::schema_for_output::<JobList>()
}

/// `limit` as the page size, or the error that says the bounds.
pub fn validate_limit(limit: Option<u64>) -> Result<usize, Box<JobList>> {
    match limit {
        None => Ok(LIST_DEFAULT_LIMIT as usize),
        Some(n) if (1..=LIST_MAX_LIMIT).contains(&n) => Ok(n as usize),
        Some(n) => Err(Box::new(JobList::failure(
            ListErrorCode::InvalidLimit,
            format!(
                "limit must be between 1 and {LIST_MAX_LIMIT} (got {n}). Omit it for \
                 {LIST_DEFAULT_LIMIT}; use next_cursor to read further pages."
            ),
        ))),
    }
}

// ---------------------------------------------------------------------------------------------
// List cursors

/// A position in a listing: the VM (name and generation), the newest id the traversal began
/// with, and the last id shown. Metadata, not a capability: a forged one can only list jobs of
/// the VM it names, below the id it names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListCursor {
    pub vm: String,
    pub generation: String,
    /// The newest id of the traversal's first page.
    pub high_water: u64,
    /// The last (oldest) id of the page this cursor follows; the next page is below it.
    pub last: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListWire {
    v: u8,
    vm: String,
    g: String,
    h: u64,
    l: u64,
}

/// Ids are bounded so the guest's integer arithmetic is exact.
const MAX_ID: u64 = 999_999_999_999_999_999;

impl ListCursor {
    /// The cursor string: base64url (no padding) of a small JSON object.
    pub fn encode(&self) -> String {
        let wire = ListWire {
            v: WIRE_VERSION,
            vm: self.vm.clone(),
            g: self.generation.clone(),
            h: self.high_water,
            l: self.last,
        };
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&wire).expect("cursor serializes"))
    }
}

/// Decode and check a list_jobs `cursor` for VM `vm`. Nothing here touches the VM.
pub fn parse_list_cursor(s: &str, vm: &str) -> Result<ListCursor, String> {
    const HINT: &str = "Omit cursor to list from the newest job.";
    if s.is_empty() || s.len() > CURSOR_MAX_CHARS {
        return Err(format!(
            "cursor must be a next_cursor value from list_jobs (1 to {CURSOR_MAX_CHARS} \
             characters). {HINT}"
        ));
    }
    let malformed = || format!("cursor is not one list_jobs issued (malformed). {HINT}");
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .map_err(|_| malformed())?;
    if raw.len() > CURSOR_MAX_CHARS {
        return Err(malformed());
    }
    let w: ListWire = serde_json::from_slice(&raw).map_err(|_| malformed())?;
    if w.v != WIRE_VERSION
        || w.vm.is_empty()
        || !token_ok(&w.vm)
        || w.g.is_empty()
        || !token_ok(&w.g)
        || w.l == 0
        || w.l > w.h
        || w.h > MAX_ID
    {
        return Err(malformed());
    }
    if w.vm != vm {
        return Err(format!(
            "this cursor belongs to VM '{}', not '{vm}'. Use the next_cursor from a list_jobs \
             reply about '{vm}', or omit cursor.",
            w.vm
        ));
    }
    Ok(ListCursor {
        vm: w.vm,
        generation: w.g,
        high_water: w.h,
        last: w.l,
    })
}

// ---------------------------------------------------------------------------------------------
// Request and guest query

/// One list_jobs call, validated.
#[derive(Clone, Debug)]
pub struct ListRequest {
    pub name: String,
    pub limit: usize,
    pub cursor: Option<ListCursor>,
    /// The VM's generation now (see `vm_generation`); set by `bind_generation`.
    pub generation: String,
    /// The most the whole query may take.
    pub attempt: Duration,
    /// How long past that it may take to report its own timeout.
    pub grace: Duration,
    /// The guest script's own time budget, in seconds.
    pub budget_secs: u64,
}

impl ListRequest {
    /// Check `limit` and `cursor` (no VM is consulted: a bad one is refused before any guest
    /// work).
    pub fn new(name: &str, limit: Option<u64>, cursor: Option<&str>) -> Result<Self, Box<JobList>> {
        let limit = validate_limit(limit)?;
        let cursor = match cursor.map(|c| parse_list_cursor(c, name)) {
            None => None,
            Some(Ok(c)) => Some(c),
            Some(Err(e)) => {
                return Err(Box::new(JobList::failure(ListErrorCode::InvalidCursor, e)));
            }
        };
        Ok(Self {
            name: name.to_string(),
            limit,
            cursor,
            generation: String::new(),
            attempt: LIST_ATTEMPT,
            grace: LIST_GRACE,
            budget_secs: LIST_GUEST_BUDGET_SECS,
        })
    }

    /// Bind the request to the VM's current generation. A cursor from another one was made for
    /// records that may differ now (a restore can keep some, a reset or recreation none): the
    /// listing cannot safely continue from it, so it is refused, never silently restarted.
    pub fn bind_generation(&mut self, current: &str) -> Result<(), Box<JobList>> {
        if let Some(c) = &self.cursor
            && c.generation != current
        {
            return Err(Box::new(JobList::failure(
                ListErrorCode::CursorStale,
                format!(
                    "this cursor is from before VM '{}' was reset, restored or recreated: its \
                     generation changed, so the job records may differ from the ones the \
                     earlier pages listed (a restored checkpoint can keep some) and the \
                     listing cannot safely continue from it. Call list_jobs without cursor \
                     to list the VM as it is now.",
                    self.name
                ),
            )));
        }
        self.generation = current.to_string();
        Ok(())
    }

    pub fn query(&self) -> ListQuery {
        ListQuery {
            limit: self.limit,
            high_water: self.cursor.as_ref().map_or(0, |c| c.high_water),
            before: self.cursor.as_ref().map_or(0, |c| c.last),
            scan_max: LIST_SCAN_MAX,
            budget_secs: self.budget_secs,
        }
    }
}

/// What the guest script is given: numbers only.
#[derive(Clone, Debug)]
pub struct ListQuery {
    /// The most jobs to report.
    pub limit: usize,
    /// Jobs newer than this are left out (0: no bound).
    pub high_water: u64,
    /// Only jobs older than this are listed (0: no bound).
    pub before: u64,
    /// The most directory entries to look at.
    pub scan_max: usize,
    /// Seconds after which the script stops adding jobs to the page.
    pub budget_secs: u64,
}

// ---------------------------------------------------------------------------------------------
// The guest report

/// One job as the guest reported it.
#[derive(Clone, Debug, PartialEq)]
pub struct GuestEntry {
    pub id: u64,
    pub kind: ListedKind,
    pub state: JobState,
    pub exit_code: Option<i64>,
    pub stop: Option<StopReason>,
    pub log: LogPresence,
    pub records: Vec<RecordKind>,
    /// File modification times (Unix seconds), when the files exist.
    pub recorded: Option<i64>,
    pub finished: Option<i64>,
}

/// A listing page as the guest reported it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GuestList {
    pub entries: Vec<GuestEntry>,
    /// Whether older jobs may remain below the last entry.
    pub more: bool,
    /// The guest's clock (Unix seconds).
    pub now: Option<i64>,
    /// How many directory entries were looked at.
    pub scanned: u64,
    pub scan_truncated: bool,
    /// The job directory exists but cannot be read.
    pub unreadable: bool,
}

/// An exit code as the guests validate it: ASCII digits with an optional minus, at most ten
/// digits, no more than 4294967295 in value.
fn exit_code(s: &str) -> Option<i64> {
    let digits = s.strip_prefix('-').unwrap_or(s);
    if digits.is_empty() || digits.len() > 10 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: i64 = s.parse().ok()?;
    (n.abs() <= 4_294_967_295).then_some(n)
}

fn id_ok(s: &str) -> Option<u64> {
    let digits = !s.is_empty()
        && s.len() <= ID_MAX_DIGITS
        && !s.starts_with('0')
        && s.bytes().all(|b| b.is_ascii_digit());
    if digits { s.parse().ok() } else { None }
}

/// Parse a list script's output (`AGENTPC_LIST <key> <value>` lines), checked against the query
/// it answered: complete (its `end` line arrived), at most `limit` jobs, strictly descending ids
/// inside the cursor's window, and every value a validated number or word.
pub fn parse_list_report(out: &[u8], q: &ListQuery) -> Result<GuestList, String> {
    let text = String::from_utf8_lossy(out);
    let mut rep = GuestList::default();
    let (mut end, mut count, mut more) = (false, None, None);
    let mut cur: Option<GuestEntry> = None;
    let mut state_seen = false;
    let mut exit_bad = false;
    let mut last_id: Option<u64> = None;

    fn finish(
        cur: &mut Option<GuestEntry>,
        state_seen: &mut bool,
        exit_bad: &mut bool,
        into: &mut Vec<GuestEntry>,
    ) -> Result<(), String> {
        if let Some(mut e) = cur.take() {
            if !*state_seen {
                return Err(format!("job {} has no state", e.id));
            }
            // An exit code that is invalid, or missing for an exited job, makes that one job
            // unknown; it is never guessed to have ended.
            if *exit_bad || (e.state == JobState::Exited && e.exit_code.is_none()) {
                e.state = JobState::Unknown;
                e.exit_code = None;
                e.stop = None;
            }
            into.push(e);
        }
        *state_seen = false;
        *exit_bad = false;
        Ok(())
    }

    for line in text.lines() {
        let Some(rest) = line.trim_end().strip_prefix("AGENTPC_LIST ") else {
            continue;
        };
        let (key, val) = match rest.split_once(' ') {
            Some((k, v)) => (k, v.trim()),
            None => (rest.trim(), ""),
        };
        let num = |v: &str| v.parse::<u64>().map_err(|_| format!("bad {key} '{v}'"));
        let time = |v: &str| -> Result<i64, String> {
            let n = num(v)?;
            if n > 253_402_300_799 {
                return Err(format!("bad {key} '{v}'"));
            }
            Ok(n as i64)
        };
        match key {
            "job" => {
                finish(&mut cur, &mut state_seen, &mut exit_bad, &mut rep.entries)?;
                let id = id_ok(val).ok_or_else(|| format!("bad job id '{val}'"))?;
                if last_id.is_some_and(|l| id >= l)
                    || (q.high_water != 0 && id > q.high_water)
                    || (q.before != 0 && id >= q.before)
                {
                    return Err(format!("job {id} is out of order or outside the window"));
                }
                last_id = Some(id);
                cur = Some(GuestEntry {
                    id,
                    kind: ListedKind::Unknown,
                    state: JobState::Unknown,
                    exit_code: None,
                    stop: None,
                    log: LogPresence::None,
                    records: Vec::new(),
                    recorded: None,
                    finished: None,
                });
            }
            "recs" | "kind" | "state" | "exit" | "stop" | "log" | "at" | "done" => {
                let e = cur
                    .as_mut()
                    .ok_or_else(|| format!("'{key}' before any job"))?;
                match key {
                    "recs" => {
                        for r in val.split(',').filter(|r| !r.is_empty()) {
                            let k = RecordKind::parse(r)
                                .ok_or_else(|| format!("unknown record '{r}'"))?;
                            if e.records.contains(&k) {
                                return Err(format!("record '{r}' twice"));
                            }
                            e.records.push(k);
                        }
                    }
                    "kind" => {
                        e.kind = match val {
                            "foreground" => ListedKind::Foreground,
                            "background" => ListedKind::Background,
                            "unknown" => ListedKind::Unknown,
                            other => return Err(format!("unknown kind '{other}'")),
                        }
                    }
                    "state" => {
                        state_seen = true;
                        e.state = match val {
                            "queued" => JobState::Queued,
                            "running" => JobState::Running,
                            "exited" => JobState::Exited,
                            "stopped" => JobState::Stopped,
                            "ended" => JobState::Ended,
                            "not_started" => JobState::NotStarted,
                            "missing" => JobState::Missing,
                            "unknown" => JobState::Unknown,
                            other => return Err(format!("unknown state '{other}'")),
                        }
                    }
                    // A bad exit code degrades this job only (see `finish`): one malformed
                    // record must not make the page unreadable and hide the older jobs.
                    "exit" => match exit_code(val) {
                        Some(c) => e.exit_code = Some(c),
                        None => exit_bad = true,
                    },
                    "stop" => {
                        e.stop = Some(match val {
                            "timeout" => StopReason::Timeout,
                            "request_cancelled" => StopReason::RequestCancelled,
                            _ => StopReason::StopJob,
                        })
                    }
                    "log" => {
                        e.log = match val {
                            "present" => LogPresence::Present,
                            "absent" => LogPresence::Absent,
                            "none" => LogPresence::None,
                            "unreadable" => LogPresence::Unreadable,
                            other => return Err(format!("unknown log '{other}'")),
                        }
                    }
                    "at" => e.recorded = Some(time(val)?),
                    _ => e.finished = Some(time(val)?),
                }
            }
            "now" => rep.now = val.parse::<u64>().ok().map(|n| n as i64),
            "count" => count = Some(num(val)?),
            "more" => {
                more = Some(match val {
                    "0" => false,
                    "1" => true,
                    _ => return Err(format!("bad more '{val}'")),
                })
            }
            "scan" => {
                let (n, t) = val.split_once(' ').unwrap_or((val, "0"));
                rep.scanned = num(n)?;
                rep.scan_truncated = t.trim() == "1";
            }
            "error" => {
                if val == "unreadable" {
                    rep.unreadable = true;
                } else {
                    return Err(format!("unknown error '{val}'"));
                }
            }
            "end" => end = true,
            _ => {}
        }
    }
    finish(&mut cur, &mut state_seen, &mut exit_bad, &mut rep.entries)?;
    if !end {
        return Err("the listing was cut off before its end".to_string());
    }
    if rep.unreadable {
        if !rep.entries.is_empty() {
            return Err("an unreadable directory with jobs".to_string());
        }
        return Ok(rep);
    }
    if rep.entries.len() > q.limit {
        return Err(format!(
            "{} jobs for a limit of {}",
            rep.entries.len(),
            q.limit
        ));
    }
    if count != Some(rep.entries.len() as u64) {
        return Err("the job count does not match".to_string());
    }
    rep.more = more.ok_or("the listing does not say whether more remain")?;
    Ok(rep)
}

// ---------------------------------------------------------------------------------------------
// Assembling the page

/// `secs` since the Unix epoch as `YYYY-MM-DDTHH:MM:SSZ`, for years 1970 to 9999.
pub fn rfc3339(secs: i64) -> Option<String> {
    if !(0..=253_402_300_799).contains(&secs) {
        return None;
    }
    let days = secs / 86_400;
    let rem = secs % 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    ))
}

fn entry_note(e: &GuestEntry) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    match e.state {
        JobState::Missing => {
            let found: Vec<&str> = e.records.iter().map(|r| r.name()).collect();
            parts.push(format!(
                "Only incidental records exist ({}): nothing has recorded the job itself (yet). \
                 That does not show that its command never started or will not start; treat it \
                 as unresolved.",
                found.join(", ")
            ));
        }
        JobState::Queued => parts.push(
            "Launched, or still recording itself. That does not show that its command has not \
             started; treat it as unresolved until get_job_status reports a final state."
                .to_string(),
        ),
        JobState::Unknown => parts.push(
            "A record could not be read or is not valid (a permission or read error, or an exit \
             record that is not an exit code), so its state is unknown, not ended; treat it as \
             unresolved."
                .to_string(),
        ),
        _ => {}
    }
    if e.kind == ListedKind::Unknown && e.state != JobState::Missing {
        parts.push("Its records do not show whether it was a foreground or background job.".into());
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// Turn a guest listing into the page for `req`.
pub fn assemble_list(req: &ListRequest, rep: &GuestList) -> JobList {
    if rep.unreadable {
        return JobList::failure(
            ListErrorCode::GuestUnreadable,
            format!(
                "The job records of VM '{}' could not be read (permission denied or a read \
                 error), so its jobs are unknown, not absent. Check the VM, then call list_jobs \
                 again.",
                req.name
            ),
        );
    }
    let mut l = JobList::base();
    l.limit = req.limit as u32;
    l.continuation = req.cursor.is_some();
    l.scan_truncated = rep.scan_truncated;
    l.listed_at = rep.now.and_then(rfc3339);
    for e in &rep.entries {
        let foreground = e.kind == ListedKind::Foreground;
        l.jobs.push(JobSummary {
            job_id: e.id,
            kind: e.kind,
            state: e.state,
            exit_code: match (e.state, e.exit_code) {
                (JobState::Exited, Some(c)) if foreground => Some(c & 0xff),
                (JobState::Exited, c) => c,
                _ => None,
            },
            stop_reason: (e.state == JobState::Stopped).then_some(e.stop).flatten(),
            log_available: e.log == LogPresence::Present,
            records: e.records.clone(),
            recorded_at: e.recorded.and_then(rfc3339),
            finished_at: e.finished.and_then(rfc3339),
            note: entry_note(e),
        });
    }
    let mut note = LIST_HISTORY_NOTE.to_string();
    if rep.scan_truncated {
        note.push_str(&format!(
            " The job directory holds more entries than one listing retains ({LIST_SCAN_MAX}), so \
             this listing may leave out jobs."
        ));
    }
    l.note = Some(note);
    if rep.more {
        let Some(last) = rep.entries.last() else {
            return JobList::failure(
                ListErrorCode::BadGuestReply,
                "The VM listed no jobs but said more remain, so there is no position to continue \
                 from. Call list_jobs again."
                    .to_string(),
            );
        };
        // The traversal's high-water mark is the first page's newest id.
        let high_water = req
            .cursor
            .as_ref()
            .map_or(rep.entries[0].id, |c| c.high_water);
        l.has_more = true;
        l.next_cursor = Some(
            ListCursor {
                vm: req.name.clone(),
                generation: req.generation.clone(),
                high_water,
                last: last.id,
            }
            .encode(),
        );
    }
    l
}

/// The readable form of a listing: one line per job, newest first, then what to do next. Not a
/// copy of the structured content.
pub fn render_list_text(l: &JobList) -> String {
    if let Some(e) = &l.error {
        return e.clone();
    }
    if l.jobs.is_empty() {
        // An empty page must not read as a complete, empty VM when it is neither.
        let mut out = match (l.continuation, l.scan_truncated) {
            (true, true) => {
                "No further jobs in the portion of the job directory this listing retained."
            }
            (true, false) => {
                "No further jobs below the last one shown (records may have been deleted since)."
            }
            (false, true) => {
                "No jobs found in the portion of the job directory this listing retained."
            }
            (false, false) => "No jobs recorded in this VM.",
        }
        .to_string();
        if let Some(n) = &l.note {
            out.push_str(&format!("\n\n{n}"));
        }
        return out;
    }
    let mut out = format!(
        "{} job{} (newest first, limit {}){}",
        l.jobs.len(),
        if l.jobs.len() == 1 { "" } else { "s" },
        l.limit,
        l.listed_at
            .as_ref()
            .map_or(String::new(), |t| format!(", listed at {t}")),
    );
    for j in &l.jobs {
        out.push_str(&format!(
            "\n{}  {}  {}",
            j.job_id,
            j.kind.name(),
            if j.state == JobState::Missing {
                "unrecorded (incidental records only)".to_string()
            } else {
                state_label(j.state, j.exit_code, j.stop_reason)
            }
        ));
        if j.log_available {
            out.push_str("  log");
        }
        if let Some(t) = &j.recorded_at {
            out.push_str(&format!("  recorded {t}"));
        }
        if let Some(t) = &j.finished_at {
            out.push_str(&format!("  finished {t}"));
        }
        if let Some(n) = &j.note {
            out.push_str(&format!("\n    {n}"));
        }
    }
    if let Some(c) = &l.next_cursor {
        out.push_str(&format!(
            "\n\nMore jobs follow: call list_jobs again with cursor: {c}"
        ));
    }
    if let Some(n) = &l.note {
        out.push_str(&format!("\n\n{n}"));
    }
    out
}

/// Ask the guest (through `fetch`, once) for the page and assemble it, within the request's
/// deadline. `cancelled` (the MCP request's cancellation) abandons only the query: the listing
/// is dropped, with the SSH call it owns, and no job is touched; a request already cancelled
/// asks the guest nothing.
pub async fn list_observe<F, Fut, C>(req: &ListRequest, fetch: F, cancelled: C) -> JobList
where
    F: FnOnce(&ListQuery, Duration) -> Fut,
    Fut: Future<Output = Result<GuestList, Failure>>,
    C: Future<Output = ()>,
{
    use futures::FutureExt;
    let abandoned = || {
        JobList::failure(
            ListErrorCode::RequestCancelled,
            "The list_jobs request was cancelled; only the listing was abandoned. No job was \
             touched."
                .to_string(),
        )
    };
    let mut cancelled = Box::pin(cancelled);
    if cancelled.as_mut().now_or_never().is_some() {
        return abandoned();
    }
    let query = req.query();
    let run = tokio::time::timeout(req.attempt + req.grace, fetch(&query, req.attempt));
    tokio::select! {
        biased;
        _ = &mut cancelled => abandoned(),
        r = run => match r {
            Ok(Ok(rep)) => assemble_list(req, &rep),
            Ok(Err(f)) => {
                let code = match f.code {
                    JobErrorCode::BadGuestReply => ListErrorCode::BadGuestReply,
                    _ => ListErrorCode::GuestUnreachable,
                };
                let message = match code {
                    ListErrorCode::BadGuestReply => format!(
                        "The VM answered, but not with a job listing this server understands \
                         ({}). Its jobs are unknown; call list_jobs again.",
                        f.detail
                    ),
                    _ => format!(
                        "Could not list the jobs: the VM did not answer ({}). Their states are \
                         unknown, not ended. Check list_vms, then call list_jobs again.",
                        f.detail
                    ),
                };
                JobList::failure(code, message)
            }
            Err(_) => JobList::failure(
                ListErrorCode::GuestUnreachable,
                format!(
                    "Could not list the jobs: the VM did not answer within {:.0} s. Their states \
                     are unknown, not ended. Check list_vms, then call list_jobs again.",
                    req.attempt.as_secs_f64()
                ),
            ),
        },
    }
}

// ---------------------------------------------------------------------------------------------
// Guest scripts (read-only)

const LINUX_LIST: &str = r#"d=~/agentpc-bg
jst=
emit() { printf 'AGENTPC_LIST %s\n' "$*"; case $1 in state) jst=$2 ;; esac; }
emit now "$(date +%s 2>/dev/null)"
if [ ! -e "$d" ]; then emit count 0; emit more 0; emit scan 0 0; emit end ok; exit 0; fi
if [ ! -d "$d" ] || [ ! -r "$d" ] || [ ! -x "$d" ]; then emit error unreadable; emit end ok; exit 0; fi
raw=$( { LC_ALL=C ls -fq "$d" 2>&1 1>&3 3>&- | sed 's|^|/AGENTPC_LSERR/ |'; } 3>&1 | head -n $((__SCAN__ + 1)) )
case $raw in "/AGENTPC_LSERR/ "*|*$'\n'"/AGENTPC_LSERR/ "*) emit error unreadable; emit end ok; exit 0 ;; esac
names=$raw
scan=$(printf '%s\n' "$names" | wc -l | tr -d ' ')
trunc=0; [ "$scan" -gt __SCAN__ ] && trunc=1
ids=$(printf '%s\n' "$names" | head -n __SCAN__ | LC_ALL=C awk -F. 'NF == 2 && length($1) <= 18 && $1 ~ /^[1-9][0-9]*$/ && $2 ~ /^(sh|log|fg|pid|proof|exit|stopped|cancel)$/ { print $1 }' | LC_ALL=C sort -u -n -r)
hi=__HI__; lt=__LT__; limit=__LIMIT__
cnt=0; more=0; SECONDS=0
__OWN__
for id in $ids; do
  [ $hi -ne 0 ] && [ $id -gt $hi ] && continue
  [ $lt -ne 0 ] && [ $id -ge $lt ] && continue
  recs=
  for e in sh log fg pid proof exit stopped cancel; do [ -f $d/$id.$e ] && recs="$recs,$e"; done
  [ -n "$recs" ] || continue
  if [ $cnt -ge $limit ]; then more=1; break; fi
  if [ $cnt -gt 0 ] && [ $SECONDS -ge __BUDGET__ ]; then more=1; break; fi
  cnt=$((cnt + 1)); jst=
  emit job $id
  emit recs ${recs#,}
  bad=0; for e in pid proof exit stopped; do [ -f $d/$id.$e ] && [ ! -r $d/$id.$e ] && bad=1; done
  fg=0; [ -f $d/$id.fg ] && fg=1
  p=; x=; xbad=0
  if [ $bad = 0 ]; then
    [ -f $d/$id.pid ] && { p=$(<$d/$id.pid); } 2>/dev/null
    case $p in ''|*[!0-9]*) p= ;; esac
    [ -f $d/$id.exit ] && { x=$(<$d/$id.exit); } 2>/dev/null
    x=${x//[[:space:]]/}
  fi
__XCHECK__
  have=0; for e in log fg pid exit stopped; do [ -f $d/$id.$e ] && have=1; done
  if [ $fg = 1 ]; then emit kind foreground
  elif [ -f $d/$id.log ] || [ -f $d/$id.sh ]; then emit kind background
  else emit kind unknown; fi
  if [ $have = 0 ]; then emit state missing
  elif [ $bad = 1 ]; then emit state unknown
  else
__STATE__
  fi
  if [ $fg = 1 ]; then emit log none
  elif [ -f $d/$id.log ] && [ -r $d/$id.log ]; then emit log present
  elif [ -f $d/$id.log ]; then emit log unreadable
  else emit log absent; fi
  t=; for e in fg sh; do if [ -f $d/$id.$e ]; then t=$(stat -c %Y $d/$id.$e 2>/dev/null); break; fi; done
  case $t in ''|*[!0-9]*) ;; *) emit at $t ;; esac
  f=; case $jst in exited) f=exit ;; stopped) f=stopped ;; esac
  if [ -n "$f" ]; then t=$(stat -c %Y $d/$id.$f 2>/dev/null); case $t in ''|*[!0-9]*) ;; *) emit done $t ;; esac; fi
done
emit count $cnt
emit more $more
emit scan $scan $trunc
emit end ok"#;

/// list_jobs's bash. It only reads: no file is created, changed or removed, and no launch
/// script or log is opened.
pub fn linux_list_script(q: &ListQuery) -> String {
    fill(
        LINUX_LIST,
        &[
            ("__OWN__", &linux_own_by("$id")),
            ("__XCHECK__", LINUX_XCHECK),
            ("__STATE__", &fill(LINUX_STATE, &[("__ID__", "$id")])),
            ("__SCAN__", &q.scan_max.to_string()),
            ("__HI__", &q.high_water.min(MAX_ID).to_string()),
            ("__LT__", &q.before.min(MAX_ID).to_string()),
            ("__LIMIT__", &q.limit.to_string()),
            ("__BUDGET__", &q.budget_secs.to_string()),
        ],
    )
}

/// How the Windows list script finds the names of the files in the job directory: the body of
/// a function that writes them, one per name, and nothing when the directory does not exist.
/// (A seam for tests on systems where a backslash is not a path separator.)
const WINDOWS_LIST_NAMES: &str = r#"  if (-not [IO.Directory]::Exists($d)) { return }
  [IO.Directory]::EnumerateFiles($d) | ForEach-Object { [IO.Path]::GetFileName($_) }"#;

const WINDOWS_LIST: &str = r#"$d = "$env:USERPROFILE\agentpc-bg"
$script:jst = ''
function Emit($k, $v) { if ($k -eq 'state') { $script:jst = $v }; "AGENTPC_LIST $k $v" }
function ListNames {
__NAMES__
}
try { Emit now ([DateTimeOffset]::UtcNow.ToUnixTimeSeconds()) } catch { }
$sw = [Diagnostics.Stopwatch]::StartNew()
$names = @()
try { $names = @(ListNames | Select-Object -First (__SCAN__ + 1)) } catch { Emit error unreadable; Emit end ok; exit 0 }
$scan = $names.Count; $trunc = 0; if ($scan -gt __SCAN__) { $trunc = 1 }
$set = New-Object 'System.Collections.Generic.HashSet[string]'
$rx = '^([1-9][0-9]{0,17})\.(ps1|log|fg|pid|exit|stopped|cancel)$'
$k = 0
foreach ($nm in $names) { $k++; if ($k -gt __SCAN__) { break }; $m = [regex]::Match([string]$nm, $rx); if ($m.Success) { [void]$set.Add($m.Groups[1].Value) } }
$ids = @($set | Sort-Object { [int64]$_ } -Descending)
$hi = [int64]__HI__; $lt = [int64]__LT__; $limit = __LIMIT__
$cnt = 0; $more = 0
__OWN__
foreach ($id in $ids) {
  $n = [int64]$id
  if ($hi -ne 0 -and $n -gt $hi) { continue }
  if ($lt -ne 0 -and $n -ge $lt) { continue }
  $recs = @()
  foreach ($e in 'ps1', 'log', 'fg', 'pid', 'exit', 'stopped', 'cancel') { if ([IO.File]::Exists("$d\$id.$e")) { $recs += $e } }
  if ($recs.Count -eq 0) { continue }
  if ($cnt -ge $limit) { $more = 1; break }
  if ($cnt -gt 0 -and $sw.Elapsed.TotalSeconds -ge __BUDGET__) { $more = 1; break }
  $cnt++; $script:jst = ''
  $log = "$d\$id.log"
  $fg = $recs -contains 'fg'
  $x = ''; $bad = $false
  if ($recs -contains 'exit') { try { $x = [IO.File]::ReadAllText("$d\$id.exit").Trim() } catch { $bad = $true } }
__XCHECK__
  $t = $null
  if ((-not $fg) -and $x -eq '') { $t = Get-ScheduledTask -TaskName "agentpc-bg-$id" -ErrorAction SilentlyContinue }
  if ($t) { $recs += 'task' }
  Emit job $id
  Emit recs ($recs -join ',')
  $have = $fg -or [bool]$t -or ($recs -contains 'log') -or ($recs -contains 'exit') -or ($recs -contains 'stopped') -or ($recs -contains 'pid')
  if ($fg) { Emit kind foreground } elseif (($recs -contains 'log') -or ($recs -contains 'ps1') -or $t) { Emit kind background } else { Emit kind unknown }
  if (-not $have) { Emit state missing }
  elseif ($bad) { Emit state unknown }
  else {
    $ran = $false
    if ($t) { try { $ran = (($t | Get-ScheduledTaskInfo).LastRunTime.Year -gt 2000) } catch { } }
    $tstate = ''
    if ($t) { $tstate = "$($t.State)" }
__STATE__
  }
  if ($fg) { Emit log none } elseif ($recs -contains 'log') { Emit log present } else { Emit log absent }
  foreach ($e in 'fg', 'ps1') { if ($recs -contains $e) { try { Emit at ([DateTimeOffset]::new([IO.File]::GetLastWriteTimeUtc("$d\$id.$e")).ToUnixTimeSeconds()) } catch { }; break } }
  $f = ''
  if ($script:jst -eq 'exited') { $f = 'exit' } elseif ($script:jst -eq 'stopped') { $f = 'stopped' }
  if ($f) { try { Emit done ([DateTimeOffset]::new([IO.File]::GetLastWriteTimeUtc("$d\$id.$f")).ToUnixTimeSeconds()) } catch { } }
}
Emit count $cnt
Emit more $more
Emit scan "$scan $trunc"
Emit end ok"#;

/// Windows's counterpart of `linux_list_script` (PowerShell 5.1 compatible).
pub fn windows_list_script(q: &ListQuery) -> String {
    windows_list_script_with(q, WINDOWS_LIST_NAMES)
}

/// `windows_list_script` with the lister of the job directory's file names replaced (see
/// `WINDOWS_LIST_NAMES`).
pub fn windows_list_script_with(q: &ListQuery, names: &str) -> String {
    fill(
        WINDOWS_LIST,
        &[
            ("__OWN__", &windows_own_by("$id")),
            ("__XCHECK__", WINDOWS_XCHECK),
            ("__STATE__", &fill(WINDOWS_STATE, &[("__ID__", "$id")])),
            ("__NAMES__", names),
            ("__SCAN__", &q.scan_max.to_string()),
            ("__HI__", &q.high_water.min(MAX_ID).to_string()),
            ("__LT__", &q.before.min(MAX_ID).to_string()),
            ("__LIMIT__", &q.limit.to_string()),
            ("__BUDGET__", &q.budget_secs.to_string()),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(cursor: Option<Resume>) -> Request {
        Request::new("vm", 7, 50, Duration::ZERO, cursor, "1:2:3:4".into())
    }

    fn cursor(offset: u64) -> Cursor {
        Cursor {
            vm: "vm".into(),
            generation: "1:2:3:4".into(),
            job: 7,
            boot: "boot-1".into(),
            log: "64768:12".into(),
            head_len: 5,
            head_hash: "0123456789abcdef".into(),
            offset,
        }
    }

    /// What a guest says about a log: its identity, size and the window of bytes it read.
    fn present(state: JobState, start: u64, data: &[u8], size: u64) -> GuestReport {
        GuestReport {
            state,
            kind: Some(JobKind::Background),
            exit_code: (state == JobState::Exited).then_some(0),
            boot: "boot-1".into(),
            log: LogPresence::Present,
            log_id: "64768:12".into(),
            size,
            head_len: 5,
            head_hash: "0123456789abcdef".into(),
            start,
            data: data.to_vec(),
            ..GuestReport::default()
        }
    }

    // ---- arguments --------------------------------------------------------------------

    #[test]
    fn waits_are_bounded_and_the_error_says_what_to_do() {
        assert_eq!(validate_wait(7, 0).unwrap(), Duration::ZERO);
        assert_eq!(validate_wait(7, 30).unwrap(), Duration::from_secs(30));
        let e = validate_wait(7, 31).unwrap_err();
        assert_eq!(e.error_code, Some(JobErrorCode::InvalidWaitSeconds));
        assert_eq!(e.state, JobState::Unknown);
        assert_eq!(e.job_id, 7);
        let msg = e.error.as_deref().unwrap();
        assert!(
            msg.contains("31") && msg.contains("30") && msg.contains("again"),
            "{msg}"
        );
        assert!(validate_wait(7, u64::MAX).is_err());
        // The error is a tool error with structured content and the same text.
        let r = e.into_result();
        assert_eq!(r.is_error, Some(true));
        assert!(r.structured_content.is_some());
    }

    // ---- cursors ----------------------------------------------------------------------

    #[test]
    fn a_cursor_round_trips_and_stays_small() {
        let c = cursor(1234);
        let s = c.encode();
        assert!(s.len() < 400 && !s.contains(['=', '+', '/']), "{s}");
        assert_eq!(parse_cursor(&s, "vm", 7).unwrap(), Resume::At(c));
        assert_eq!(parse_cursor("start", "vm", 7).unwrap(), Resume::Start);
    }

    #[test]
    fn malformed_and_foreign_cursors_are_refused() {
        let good = cursor(10).encode();
        let b64 = |v: serde_json::Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string())
        };
        let mut bad: Vec<(String, &str)> = vec![
            (String::new(), "empty"),
            ("not a cursor!".into(), "not base64"),
            (b64(serde_json::json!("nope")), "not an object"),
            ("a".repeat(CURSOR_MAX_CHARS + 1), "too long"),
            (b64(serde_json::json!({"v": 1})), "missing fields"),
        ];
        let mut with = |f: &dyn Fn(&mut serde_json::Value), why| {
            let mut v: serde_json::Value = serde_json::from_slice(
                &base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(&good)
                    .unwrap(),
            )
            .unwrap();
            f(&mut v);
            bad.push((b64(v), why));
        };
        with(&|v| v["v"] = 2.into(), "unknown version");
        with(&|v| v["extra"] = 1.into(), "unknown field");
        with(&|v| v["vm"] = "v m;x".into(), "bad characters");
        with(&|v| v["l"] = "x".repeat(200).into(), "oversized token");
        with(&|v| v["hl"] = 65.into(), "head longer than the fingerprint");
        with(&|v| v["hh"] = "zz".into(), "bad head hash");
        with(&|v| v["o"] = u64::MAX.into(), "absurd offset");
        with(
            &|v| {
                v["l"] = "".into();
                v["hl"] = 0.into();
                v["hh"] = "".into();
            },
            "an unbound cursor past the start",
        );
        for (s, why) in bad {
            let e = parse_cursor(&s, "vm", 7).unwrap_err();
            assert!(!e.is_empty(), "{why}");
        }
        // Another VM's or another job's cursor is refused, saying whose it is.
        let e = parse_cursor(&good, "other", 7).unwrap_err();
        assert!(e.contains("vm") && e.contains("other"), "{e}");
        let e = parse_cursor(&good, "vm", 8).unwrap_err();
        assert!(e.contains('7') && e.contains('8'), "{e}");
        // `start` is only a literal; its case and padding are not forgiven.
        assert!(parse_cursor("Start", "vm", 7).is_err());
        assert!(parse_cursor(" start", "vm", 7).is_err());
    }

    #[test]
    fn the_vm_generation_follows_the_disk_not_the_name() {
        let dir = std::env::temp_dir().join(format!("agentpc-gen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("disk.qcow2"), b"a").unwrap();
        let g1 = vm_generation(&dir, 3);
        assert_eq!(g1, vm_generation(&dir, 3), "stable while nothing changes");
        // A reset or restore swaps the disk file for a new one.
        std::fs::write(dir.join("new.qcow2"), b"b").unwrap();
        std::fs::rename(dir.join("new.qcow2"), dir.join("disk.qcow2")).unwrap();
        assert_ne!(g1, vm_generation(&dir, 3));
        assert!(
            vm_generation(&dir, 3)
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == ':')
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- guest protocol ---------------------------------------------------------------

    #[test]
    fn a_guest_reply_is_parsed_from_its_machine_lines_only() {
        let data = base64::engine::general_purpose::STANDARD.encode(b"hello\0\xff");
        let out = format!(
            "banner: AGENTPC_JOB state ended\r\n\
             AGENTPC_JOB state exited\r\n\
             AGENTPC_JOB kind background\n\
             AGENTPC_JOB exit 3\n\
             AGENTPC_JOB boot b-1\n\
             AGENTPC_JOB log present\n\
             AGENTPC_JOB logid 5:6\n\
             AGENTPC_JOB size 100\n\
             AGENTPC_JOB head 64 0123456789abcdef\n\
             AGENTPC_JOB reset 1\n\
             AGENTPC_JOB why log_shrunk\n\
             AGENTPC_JOB start 0\n\
             AGENTPC_JOB data {data}\n\
             STATE: running\n\
             AGENTPC_JOB end ok\n"
        );
        let r = parse_report(out.as_bytes()).unwrap();
        assert_eq!(r.state, JobState::Exited);
        assert_eq!(r.kind, Some(JobKind::Background));
        assert_eq!(r.exit_code, Some(3));
        assert_eq!(r.log, LogPresence::Present);
        assert_eq!((r.log_id.as_str(), r.size), ("5:6", 100));
        assert_eq!((r.head_len, r.head_hash.as_str()), (64, "0123456789abcdef"));
        assert_eq!(r.reset, Some(ResetReason::LogShrunk));
        assert_eq!(r.data, b"hello\0\xff");
    }

    #[test]
    fn a_cut_off_or_odd_guest_reply_is_not_a_state() {
        for (out, why) in [
            ("", "nothing"),
            ("AGENTPC_JOB state running\n", "no end line"),
            ("AGENTPC_JOB end ok\n", "no state"),
            (
                "AGENTPC_JOB state sleeping\nAGENTPC_JOB end ok\n",
                "unknown state",
            ),
            (
                "AGENTPC_JOB state exited\nAGENTPC_JOB end ok\n",
                "exited without a code",
            ),
            (
                "AGENTPC_JOB state exited\nAGENTPC_JOB exit abc\nAGENTPC_JOB end ok\n",
                "bad code",
            ),
            (
                "AGENTPC_JOB state running\nAGENTPC_JOB log present\nAGENTPC_JOB data !!!\nAGENTPC_JOB end ok\n",
                "bad base64",
            ),
            (
                "AGENTPC_JOB state running\nAGENTPC_JOB log present\nAGENTPC_JOB logid 1:2\nAGENTPC_JOB size 3\nAGENTPC_JOB start 2\nAGENTPC_JOB data YWJj\nAGENTPC_JOB end ok\n",
                "data past the end of the log",
            ),
        ] {
            assert!(parse_report(out.as_bytes()).is_err(), "{why}");
        }
        // Missing carries no log and needs nothing else.
        let r = parse_report(b"AGENTPC_JOB state missing\nAGENTPC_JOB end ok\n").unwrap();
        assert_eq!(r.state, JobState::Missing);
        assert_eq!(r.log, LogPresence::None);
    }

    // ---- UTF-8 ------------------------------------------------------------------------

    #[test]
    fn decoding_never_splits_a_character_and_says_so_about_bad_bytes() {
        // "h" "é"(2) "€"(3) "𝄞"(4)
        let text = "hé€𝄞".as_bytes();
        // Cut at every byte: what is consumed ends on a character boundary, the rest is
        // held for the next read, and the pieces add up to the whole.
        for cut in 0..=text.len() {
            let d = decode(&text[..cut], false, false);
            assert!(!d.invalid, "{cut}");
            assert!("hé€𝄞".starts_with(&d.text), "{cut}");
            assert_eq!(d.text.len(), d.consumed, "{cut}");
            let rest = decode(&text[d.consumed..], false, false);
            assert_eq!(format!("{}{}", d.text, rest.text), "hé€𝄞");
        }
        // A final chunk (job over, end of log) cannot be completed: shown as U+FFFD.
        let d = decode(&text[..2], false, true);
        assert_eq!(
            (d.text.as_str(), d.consumed, d.invalid),
            ("h\u{fffd}", 2, true)
        );
        // Invalid bytes in the middle are replaced and counted as consumed.
        let d = decode(b"ab\xffcd\xc0\xafe", false, false);
        assert_eq!(d.text, "ab\u{fffd}cd\u{fffd}\u{fffd}e");
        assert!(d.invalid);
        assert_eq!(d.consumed, 8);
        // A NUL byte is data like any other.
        let d = decode(b"a\0b", false, false);
        assert_eq!((d.text.as_str(), d.consumed, d.invalid), ("a\0b", 3, false));
    }

    #[test]
    fn a_byte_order_mark_at_the_start_is_consumed_but_not_shown() {
        let d = decode(b"\xef\xbb\xbfhi", true, false);
        assert_eq!((d.text.as_str(), d.consumed), ("hi", 5));
        // Alone, it still advances the cursor.
        let d = decode(b"\xef\xbb\xbf", true, false);
        assert_eq!((d.text.as_str(), d.consumed), ("", 3));
        // Elsewhere in the log it is an ordinary character.
        let d = decode(b"\xef\xbb\xbfhi", false, false);
        assert_eq!(d.text, "\u{feff}hi");
    }

    // ---- assembling a status ----------------------------------------------------------

    #[test]
    fn the_first_tail_gives_the_text_and_a_cursor_at_the_end_of_what_it_saw() {
        let log = "one\ntwo\nthree\n";
        let rep = present(JobState::Running, 0, log.as_bytes(), log.len() as u64);
        let mut r = req(None);
        r.tail_lines = 2;
        let s = assemble(&r, &rep);
        assert_eq!(s.state, JobState::Running);
        assert_eq!(s.log_tail.as_deref(), Some("two\nthree\n"));
        assert_eq!(s.output, None);
        assert!(!s.output_truncated && !s.cursor_reset && !s.wait_timed_out);
        let next = parse_cursor(s.next_cursor.as_deref().unwrap(), "vm", 7).unwrap();
        let Resume::At(c) = next else { panic!() };
        assert_eq!(c.offset, log.len() as u64);
        assert_eq!((c.log.as_str(), c.boot.as_str()), ("64768:12", "boot-1"));
        assert_eq!((c.head_len, c.head_hash.as_str()), (5, "0123456789abcdef"));
        // Reading from that cursor gets nothing new, and says where it stands.
        let again = present(JobState::Running, log.len() as u64, b"", log.len() as u64);
        let s2 = assemble(&req(Some(Resume::At(c.clone()))), &again);
        assert_eq!(s2.output.as_deref(), Some(""));
        assert_eq!(s2.next_cursor, s.next_cursor);
        assert!(!s2.has_more && !s2.cursor_reset);
    }

    #[test]
    fn a_tail_cut_by_the_byte_cap_is_marked_truncated() {
        // The window starts mid-log and mid-character: its first (partial) line cannot count.
        let window = "\u{20ac}abc\nline2\nline3\n".as_bytes();
        let cut = &window[1..]; // starts in the middle of the euro sign
        let rep = present(JobState::Running, 1000, cut, 1000 + cut.len() as u64);
        let mut r = req(None);
        r.tail_lines = 50;
        let s = assemble(&r, &rep);
        assert!(s.output_truncated, "fewer lines than asked, window cut");
        assert_eq!(s.log_tail.as_deref(), Some("abc\nline2\nline3\n"));
        // Enough whole lines in the window: not truncated, the partial first line is dropped.
        r.tail_lines = 2;
        let s = assemble(&r, &rep);
        assert_eq!(s.log_tail.as_deref(), Some("line2\nline3\n"));
        assert!(!s.output_truncated);
        // tail_lines 0 asks for no log text.
        r.tail_lines = 0;
        assert_eq!(assemble(&r, &rep).log_tail.as_deref(), Some(""));
    }

    #[test]
    fn a_cursor_read_advances_by_exactly_the_bytes_it_returns() {
        // "aé" then a cap that cuts the second byte of "é"... the window is what the guest
        // read; the euro sign is split by the cap and held back.
        let log = "a\u{20ac}b".as_bytes(); // 1 + 3 + 1
        let rep = present(JobState::Running, 0, &log[..3], log.len() as u64);
        let s = assemble(&req(Some(Resume::Start)), &rep);
        assert_eq!(s.output.as_deref(), Some("a"));
        assert_eq!(s.output_bytes, 1);
        assert!(s.has_more, "the log goes on past the window");
        let Resume::At(c) = parse_cursor(s.next_cursor.as_deref().unwrap(), "vm", 7).unwrap()
        else {
            panic!()
        };
        assert_eq!(c.offset, 1, "the split character is re-read, not lost");
        // The next read starts at byte 1 and gets the rest.
        let rep = present(JobState::Running, 1, &log[1..], log.len() as u64);
        let s = assemble(&req(Some(Resume::At(c))), &rep);
        assert_eq!(s.output.as_deref(), Some("\u{20ac}b"));
        assert!(!s.has_more);
        let Resume::At(c) = parse_cursor(s.next_cursor.as_deref().unwrap(), "vm", 7).unwrap()
        else {
            panic!()
        };
        assert_eq!(c.offset, 5);
    }

    #[test]
    fn a_partial_character_at_the_end_of_a_finished_job_is_consumed_honestly() {
        let rep = present(JobState::Exited, 0, &"a\u{20ac}".as_bytes()[..2], 2);
        let s = assemble(&req(Some(Resume::Start)), &rep);
        assert_eq!(s.output.as_deref(), Some("a\u{fffd}"));
        assert!(s.invalid_utf8);
        let Resume::At(c) = parse_cursor(s.next_cursor.as_deref().unwrap(), "vm", 7).unwrap()
        else {
            panic!()
        };
        assert_eq!(c.offset, 2);
        // While the job runs, the same bytes are held for the rest of the character.
        let rep = present(JobState::Running, 0, &"a\u{20ac}".as_bytes()[..2], 2);
        let s = assemble(&req(Some(Resume::Start)), &rep);
        assert_eq!(s.output.as_deref(), Some("a"));
        assert!(!s.invalid_utf8);
    }

    #[test]
    fn a_reset_restarts_at_the_beginning_and_says_why() {
        let mut rep = present(JobState::Running, 0, b"new log\n", 8);
        rep.reset = Some(ResetReason::LogShrunk);
        let s = assemble(&req(Some(Resume::At(cursor(500)))), &rep);
        assert!(s.cursor_reset);
        assert_eq!(s.cursor_reset_reason, Some(ResetReason::LogShrunk));
        assert_eq!(s.output.as_deref(), Some("new log\n"));
        // A cursor from another VM generation is reset by the host before asking the guest.
        let mut c = cursor(500);
        c.generation = "9:9:9:9".into();
        let r = req(Some(Resume::At(c)));
        assert!(matches!(r.query().mode, Mode::Read(ref e) if e.offset == 0 && e.log.is_empty()));
        let rep = present(JobState::Running, 0, b"x", 1);
        let s = assemble(&r, &rep);
        assert_eq!(s.cursor_reset_reason, Some(ResetReason::VmChanged));
        // ... but an unbound cursor (nothing read yet) has nothing to reset.
        let mut c = cursor(0);
        c.log = String::new();
        c.head_len = 0;
        c.head_hash = String::new();
        c.generation = "9:9:9:9".into();
        let s = assemble(&req(Some(Resume::At(c))), &rep);
        assert!(!s.cursor_reset);
        // A log that vanished under a bound cursor is a reset too, with a fresh cursor.
        let gone = GuestReport {
            state: JobState::Running,
            kind: Some(JobKind::Background),
            log: LogPresence::Absent,
            ..GuestReport::default()
        };
        let s = assemble(&req(Some(Resume::At(cursor(500)))), &gone);
        assert_eq!(s.cursor_reset_reason, Some(ResetReason::LogMissing));
        let Resume::At(c) = parse_cursor(s.next_cursor.as_deref().unwrap(), "vm", 7).unwrap()
        else {
            panic!()
        };
        assert_eq!((c.offset, c.log.as_str()), (0, ""));
    }

    #[test]
    fn missing_queued_and_foreground_jobs_are_each_their_own_state() {
        let missing = GuestReport {
            state: JobState::Missing,
            ..GuestReport::default()
        };
        let s = assemble(&req(None), &missing);
        assert_eq!(s.state, JobState::Missing);
        assert_eq!((s.error.as_deref(), s.exit_code), (None, None));
        assert!(!s.log_available);
        // Nothing may suggest running it again: the launch could still arrive.
        let said = render_text(&s);
        assert!(said.contains("STATE: no such job"), "{said}");
        assert!(
            !said.to_lowercase().contains("retry") && !said.contains("run_command again"),
            "{said}"
        );
        assert!(said.contains("may still"), "{said}");
        // Queued: no log yet; the cursor starts at the beginning of whatever log appears.
        let queued = GuestReport {
            state: JobState::Queued,
            kind: Some(JobKind::Background),
            log: LogPresence::Absent,
            ..GuestReport::default()
        };
        let s = assemble(&req(None), &queued);
        assert_eq!(
            (s.state, s.log_tail.as_deref()),
            (JobState::Queued, Some(""))
        );
        assert!(s.next_cursor.is_some() && !s.log_available);
        // A foreground handle has no log and no cursor.
        let fg = GuestReport {
            state: JobState::Running,
            kind: Some(JobKind::Foreground),
            log: LogPresence::None,
            ..GuestReport::default()
        };
        let s = assemble(&req(None), &fg);
        assert_eq!(
            (s.kind, s.log_tail.clone(), s.next_cursor.clone()),
            (Some(JobKind::Foreground), None, None)
        );
        assert!(render_text(&s).contains(FOREGROUND_NOTE));
        let s = assemble(&req(Some(Resume::Start)), &fg);
        assert_eq!((s.output, s.next_cursor), (None, None));
    }

    /// A foreground execution's status reports the exit code run_command reports for it: the
    /// recorded code as an SSH client sees it (its low 8 bits; Windows `exit -1` and `exit 511`
    /// read 255). A background job keeps the code its record holds.
    #[test]
    fn a_foreground_handle_reports_the_same_exit_code_as_run_command() {
        let of = |kind, code| {
            assemble(
                &req(None),
                &GuestReport {
                    state: JobState::Exited,
                    kind: Some(kind),
                    exit_code: Some(code),
                    log: LogPresence::None,
                    ..GuestReport::default()
                },
            )
        };
        for (recorded, shown) in [
            (-1, 255),
            (511, 255),
            (256, 0),
            (3, 3),
            (0, 0),
            (255, 255),
            (-256, 0),
        ] {
            let s = of(JobKind::Foreground, recorded);
            assert_eq!(s.exit_code, Some(shown), "foreground {recorded}");
            assert!(render_text(&s).starts_with(&format!("STATE: exited {shown}\n")));
        }
        assert_eq!(of(JobKind::Background, -1).exit_code, Some(-1));
        assert_eq!(of(JobKind::Background, 511).exit_code, Some(511));
    }

    #[test]
    fn exited_stopped_ended_and_not_started_are_distinct() {
        let base = |state, stop, exit| GuestReport {
            state,
            kind: Some(JobKind::Background),
            exit_code: exit,
            stop,
            log: LogPresence::Absent,
            ..GuestReport::default()
        };
        let label = |g: GuestReport| render_text(&assemble(&req(None), &g));
        assert!(label(base(JobState::Exited, None, Some(3))).starts_with("STATE: exited 3\n"));
        assert!(
            label(base(JobState::Stopped, Some(StopReason::StopJob), None))
                .starts_with("STATE: stopped by stop_job")
        );
        assert!(
            label(base(JobState::Stopped, Some(StopReason::Timeout), None))
                .starts_with("STATE: stopped at its run_command timeout")
        );
        // A foreground command stopped because its run_command request was cancelled: its
        // own reason, in the record and the text, never read as stop_job or a timeout.
        let r = parse_report(
            b"AGENTPC_JOB kind foreground\nAGENTPC_JOB state stopped\nAGENTPC_JOB stop request_cancelled\nAGENTPC_JOB end ok\n",
        )
        .unwrap();
        assert_eq!(r.stop, Some(StopReason::RequestCancelled));
        assert_eq!(
            serde_json::to_value(StopReason::RequestCancelled).unwrap(),
            "request_cancelled"
        );
        let said = label(base(
            JobState::Stopped,
            Some(StopReason::RequestCancelled),
            None,
        ));
        assert!(
            said.starts_with(
                "STATE: stopped when its run_command request was cancelled or its MCP session ended"
            ),
            "{said}"
        );
        assert!(
            label(base(JobState::Ended, None, None))
                .starts_with("STATE: ended without an exit code (killed, or the VM restarted)")
        );
        assert!(label(base(JobState::NotStarted, None, None)).starts_with("STATE: not started"));
        assert!(label(base(JobState::Running, None, None)).starts_with("STATE: running"));
        assert!(label(base(JobState::Queued, None, None)).starts_with("STATE: queued"));
        let s = assemble(&req(None), &base(JobState::Exited, None, Some(3)));
        assert_eq!(s.exit_code, Some(3));
    }

    #[test]
    fn the_text_keeps_the_legacy_state_and_tail_shape() {
        let log = "building\nsmoke-job-done\n";
        let s = assemble(
            &req(None),
            &present(JobState::Exited, 0, log.as_bytes(), log.len() as u64),
        );
        let said = render_text(&s);
        assert!(
            said.starts_with("STATE: exited 0\n--- log tail ---\nbuilding\nsmoke-job-done"),
            "{said}"
        );
        assert!(said.contains("next_cursor: "), "{said}");
        assert!(!said.contains('{'), "no JSON mirror: {said}");
    }

    #[test]
    fn failures_are_structured_errors_that_never_pose_as_a_state_of_the_job() {
        for code in [
            JobErrorCode::InvalidCursor,
            JobErrorCode::NoSuchVm,
            JobErrorCode::VmNotRunning,
            JobErrorCode::GuestUnreachable,
            JobErrorCode::BadGuestReply,
        ] {
            let s = JobStatus::failure(7, code, "why".into());
            assert_eq!(
                (s.state, s.error_code, s.error.as_deref()),
                (JobState::Unknown, Some(code), Some("why"))
            );
            assert!(
                s.exit_code.is_none()
                    && s.next_cursor.is_none()
                    && s.log_tail.is_none()
                    && s.output.is_none()
            );
            let r = s.into_result();
            assert_eq!(r.is_error, Some(true));
            assert_eq!(r.content.len(), 1);
        }
        // A job state is a successful answer, even when the job failed.
        let s = assemble(
            &req(None),
            &GuestReport {
                state: JobState::Exited,
                exit_code: Some(9),
                ..GuestReport::default()
            },
        );
        assert_eq!(s.into_result().is_error, Some(false));
    }

    // ---- the output schema ------------------------------------------------------------

    /// Does `v` fit the (inlined, $ref-free) schema node?
    fn fits(schema: &serde_json::Value, v: &serde_json::Value) -> bool {
        if let Some(any) = schema.get("anyOf").or_else(|| schema.get("oneOf")) {
            return any.as_array().unwrap().iter().any(|s| fits(s, v));
        }
        if let Some(options) = schema.get("enum") {
            return options.as_array().unwrap().contains(v);
        }
        if let Some(c) = schema.get("const") {
            return c == v;
        }
        let ok = |t: &str| match t {
            "string" => v.is_string(),
            "integer" => v.is_u64() || v.is_i64(),
            "boolean" => v.is_boolean(),
            "null" => v.is_null(),
            "number" => v.is_number(),
            "object" => v.is_object(),
            "array" => v.is_array(),
            _ => false,
        };
        match schema.get("type") {
            Some(serde_json::Value::String(t)) => ok(t),
            Some(serde_json::Value::Array(ts)) => ts.iter().any(|t| ok(t.as_str().unwrap())),
            _ => false,
        }
    }

    #[test]
    fn every_result_matches_the_declared_output_schema() {
        let schema = serde_json::Value::Object((*output_schema()).clone());
        assert_eq!(schema["type"], "object");
        assert!(!schema.to_string().contains("$ref"), "enums are inlined");
        let props = schema["properties"].as_object().unwrap();
        for f in [
            "schema_version",
            "state",
            "job_id",
            "kind",
            "exit_code",
            "stop_reason",
            "log_available",
            "log_tail",
            "output",
            "output_bytes",
            "output_truncated",
            "invalid_utf8",
            "has_more",
            "next_cursor",
            "cursor_reset",
            "cursor_reset_reason",
            "wait_timed_out",
            "waited_ms",
            "note",
            "error",
            "error_code",
        ] {
            assert!(props.contains_key(f), "{f}");
        }
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        let log = "x\n";
        let mut samples = vec![
            JobStatus::failure(7, JobErrorCode::InvalidWaitSeconds, "m".into()),
            JobStatus::failure(7, JobErrorCode::GuestUnreachable, "m".into()),
            assemble(
                &req(None),
                &GuestReport {
                    state: JobState::Missing,
                    ..GuestReport::default()
                },
            ),
            assemble(&req(None), &present(JobState::Exited, 0, log.as_bytes(), 2)),
            assemble(
                &req(Some(Resume::Start)),
                &present(JobState::Running, 0, log.as_bytes(), 2),
            ),
        ];
        let mut rep = present(JobState::Running, 0, b"x", 1);
        rep.reset = Some(ResetReason::Reboot);
        samples.push(assemble(&req(Some(Resume::At(cursor(5)))), &rep));
        for s in samples {
            let v = serde_json::to_value(&s).unwrap();
            let obj = v.as_object().unwrap();
            for r in &required {
                assert!(obj.contains_key(*r), "{r} missing from {v}");
            }
            for (k, val) in obj {
                let node = props.get(k).unwrap_or_else(|| panic!("{k} undeclared"));
                assert!(fits(node, val), "{k} = {val} does not fit {node}");
            }
            assert_eq!(obj["schema_version"], SCHEMA_VERSION);
        }
    }

    /// get_job_status is a read: nothing in this module may reach the code that writes a
    /// tombstone (the probe and the cancellation) or stop a job.
    #[test]
    fn this_module_never_reaches_the_mutating_probe_or_cancel() {
        let src = include_str!("jobs.rs");
        let prod = &src[..src.find("#[cfg(test)]\nmod tests").unwrap()];
        for name in [
            "probe_execution",
            "cancel_execution",
            "linux_probe",
            "windows_probe",
            "linux_stop_script",
            "windows_stop_script",
            "stop_job",
            "run_tracked",
        ] {
            // Mentions in prose are fine; calls and imports are not.
            for line in prod.lines().filter(|l| !l.trim_start().starts_with("//")) {
                assert!(!line.contains(&format!("{name}(")), "{name}: {line}");
                assert!(!line.contains(&format!("::{name}")), "{name}: {line}");
            }
        }
        // The only thing it takes from the execution module is the read-only liveness check.
        assert_eq!(prod.matches("crate::execution").count(), 1);
        assert!(prod.contains(
            "use crate::execution::{linux_own, linux_own_by, windows_own, windows_own_by};"
        ));
    }

    // ---- the guest scripts do not write -----------------------------------------------

    #[test]
    fn the_status_scripts_only_read() {
        let q = Query {
            mode: Mode::Tail,
            cap: REPLY_BYTES,
        };
        let read = Query {
            mode: Mode::Read(Expect {
                boot: "b".into(),
                log: "1:2".into(),
                head_len: 3,
                head_hash: "0123456789abcdef".into(),
                offset: 9,
            }),
            cap: REPLY_BYTES,
        };
        for script in [
            linux_script(42, &q),
            linux_script(42, &read),
            windows_script(42, &q),
            windows_script(42, &read),
        ] {
            assert!(
                script.contains("42.log") && script.contains("AGENTPC_JOB"),
                "{script}"
            );
            // No tombstone, no probe, no record is ever written. A tombstone is only ever
            // tested for (it decides not_started), in exactly these read-only forms.
            let tested = script
                .replace("[ -e $d/42.cancel ]", "")
                .replace(r#"[IO.File]::Exists("$d\42.cancel")"#, "");
            assert!(script.contains("42.cancel"), "{script}");
            for forbidden in [
                ".cancel",
                "mkdir",
                "touch",
                "rm ",
                "mv ",
                "tee ",
                "Set-Content",
                "Out-File",
                "Add-Content",
                "New-Item",
                "Remove-Item",
                "WriteAllText",
                "WriteAllBytes",
                "Stop-Process",
                "kill ",
                "Stop-ScheduledTask",
                "Disable-ScheduledTask",
                "Register-",
                "Start-ScheduledTask",
                "AGENTPC_PROBE",
            ] {
                assert!(!tested.contains(forbidden), "{forbidden} in {script}");
            }
            // Redirections only read, or discard.
            for line in script.lines() {
                for (i, _) in line.match_indices('>') {
                    let rest = line[i..].trim_start_matches('>').trim_start();
                    assert!(
                        rest.starts_with("/dev/null") || rest.starts_with('&'),
                        "writes in: {line}"
                    );
                }
            }
        }
    }

    #[test]
    fn script_parameters_are_clean_tokens() {
        let evil = Query {
            mode: Mode::Read(Expect {
                boot: "x'; touch /tmp/pwned #".into(),
                log: "1:2\"; rm -rf ~; \"".into(),
                head_len: 1,
                head_hash: "$(id)".into(),
                offset: 0,
            }),
            cap: REPLY_BYTES,
        };
        for script in [linux_script(1, &evil), windows_script(1, &evil)] {
            for bad in ["touch", "rm -rf", "$(id)", "pwned"] {
                assert!(!script.contains(bad), "{bad} reached the script");
            }
        }
    }

    // ---- waiting ----------------------------------------------------------------------

    use std::sync::Mutex;

    const FAST: Pacing = Pacing {
        poll: Duration::from_millis(5),
        transport: Duration::from_millis(40),
        ceiling: Duration::from_millis(200),
        grace: Duration::from_millis(20),
    };

    fn running() -> GuestReport {
        present(JobState::Running, 0, b"", 0)
    }

    type Played = std::pin::Pin<Box<dyn Future<Output = Result<GuestReport, Failure>> + Send>>;

    /// A fetch that plays back `script` (the last entry repeats) and records the timeouts.
    #[allow(clippy::type_complexity)]
    fn script(
        script: Vec<Result<GuestReport, Failure>>,
    ) -> (
        Arc<Mutex<Vec<Duration>>>,
        impl FnMut(&Query, Duration) -> Played,
    ) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let script = Arc::new(script);
        let f = move |_: &Query, t: Duration| {
            let mut seen = log.lock().unwrap();
            seen.push(t);
            let i = (seen.len() - 1).min(script.len() - 1);
            let r = script[i].clone();
            Box::pin(async move { r }) as Played
        };
        (seen, f)
    }

    #[tokio::test]
    async fn no_wait_asks_the_guest_once_and_never_waits() {
        let (seen, fetch) = script(vec![Ok(running())]);
        let s = observe(&req(None), FAST, fetch).await;
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(s.state, JobState::Running);
        assert!(!s.wait_timed_out);
        // The no-wait check gets the full ceiling, as the old fixed 30 s did.
        assert_eq!(seen.lock().unwrap()[0], FAST.ceiling);
    }

    #[tokio::test]
    async fn an_expired_wait_returns_the_running_job_and_does_not_touch_it() {
        let (seen, fetch) = script(vec![Ok(running())]);
        let mut r = req(None);
        r.wait = Duration::from_millis(60);
        let t = Instant::now();
        let s = observe(&r, FAST, fetch).await;
        let took = t.elapsed();
        assert_eq!(s.state, JobState::Running);
        assert!(s.wait_timed_out, "the wait ran out");
        assert!(s.error.is_none());
        assert!(
            took >= Duration::from_millis(55) && took < Duration::from_millis(400),
            "{took:?}"
        );
        assert!(seen.lock().unwrap().len() >= 3, "polled while waiting");
        assert!(s.waited_ms >= 55);
    }

    #[tokio::test]
    async fn a_wait_ends_as_soon_as_the_job_does() {
        let (seen, fetch) = script(vec![
            Ok(running()),
            Ok(running()),
            Ok(present(JobState::Exited, 0, b"", 0)),
        ]);
        let mut r = req(None);
        r.wait = Duration::from_secs(30);
        let t = Instant::now();
        let s = observe(&r, FAST, fetch).await;
        assert!(t.elapsed() < Duration::from_secs(1));
        assert_eq!(
            (s.state, s.exit_code, s.wait_timed_out),
            (JobState::Exited, Some(0), false)
        );
        assert_eq!(seen.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_cursor_wait_returns_when_output_arrives() {
        let (_, fetch) = script(vec![
            Ok(running()),
            Ok(present(JobState::Running, 0, b"hello\n", 6)),
        ]);
        let mut r = req(Some(Resume::Start));
        r.wait = Duration::from_secs(30);
        let t = Instant::now();
        let s = observe(&r, FAST, fetch).await;
        assert!(t.elapsed() < Duration::from_secs(1));
        assert_eq!(
            (s.output.as_deref(), s.state, s.wait_timed_out),
            (Some("hello\n"), JobState::Running, false)
        );
        // Without a cursor the same wait would keep waiting for the job to end.
        let (seen, fetch) = script(vec![
            Ok(running()),
            Ok(present(JobState::Running, 0, b"hello\n", 6)),
        ]);
        let mut r = req(None);
        r.wait = Duration::from_millis(50);
        let s = observe(&r, FAST, fetch).await;
        assert!(s.wait_timed_out && seen.lock().unwrap().len() > 2);
    }

    #[tokio::test]
    async fn a_wait_survives_a_missing_record_that_may_still_arrive() {
        let missing = GuestReport {
            state: JobState::Missing,
            ..GuestReport::default()
        };
        let (_, fetch) = script(vec![Ok(missing), Ok(running())]);
        let mut r = req(None);
        r.wait = Duration::from_millis(100);
        let s = observe(&r, FAST, fetch).await;
        assert_eq!(
            s.state,
            JobState::Running,
            "the launch arrived while waiting"
        );
        // Without a wait, a missing record is just reported.
        let missing = GuestReport {
            state: JobState::Missing,
            ..GuestReport::default()
        };
        let (_, fetch) = script(vec![Ok(missing)]);
        let s = observe(&req(None), FAST, fetch).await;
        assert_eq!(s.state, JobState::Missing);
    }

    #[tokio::test]
    async fn an_unreachable_guest_is_unknown_never_ended() {
        let down = Failure::unreachable("ssh: connect timed out");
        let (_, fetch) = script(vec![Err(down.clone())]);
        let s = observe(&req(None), FAST, fetch).await;
        assert_eq!(
            (s.state, s.error_code),
            (JobState::Unknown, Some(JobErrorCode::GuestUnreachable))
        );
        assert!(s.error.as_deref().unwrap().contains("connect timed out"));
        assert!(s.exit_code.is_none());
        assert_eq!(s.into_result().is_error, Some(true));
        // A blip during a wait is retried within the wait ...
        let (seen, fetch) = script(vec![Err(down.clone()), Ok(running())]);
        let mut r = req(None);
        r.wait = Duration::from_millis(50);
        let s = observe(&r, FAST, fetch).await;
        assert!(seen.lock().unwrap().len() > 2);
        assert_eq!(s.state, JobState::Running);
        // ... but a guest that stays down is reported, not guessed.
        let (_, fetch) = script(vec![Ok(running()), Err(down)]);
        let s = observe(&r, FAST, fetch).await;
        assert_eq!(s.error_code, Some(JobErrorCode::GuestUnreachable));
        let (_, fetch) = script(vec![Err(Failure::bad_reply("cut off"))]);
        let s = observe(&req(None), FAST, fetch).await;
        assert_eq!(s.error_code, Some(JobErrorCode::BadGuestReply));
    }

    #[tokio::test]
    async fn each_check_is_bounded_by_the_remaining_wait_plus_the_transport_grace() {
        // No fresh floor near the deadline: a check gets what is left of the wait plus the
        // explicit transport grace, never more, and the last one gets about the grace alone.
        let (seen, fetch) = script(vec![Ok(running())]);
        let mut r = req(None);
        r.wait = Duration::from_millis(100);
        let _ = observe(&r, FAST, fetch).await;
        let seen = seen.lock().unwrap().clone();
        assert!(
            seen.iter().all(|t| *t <= r.wait + FAST.transport),
            "{seen:?}"
        );
        assert!(
            *seen.last().unwrap() <= FAST.transport + Duration::from_millis(25),
            "{seen:?}"
        );
        // A fetch that never answers, with nothing seen before, is cut off at its budget (a
        // little over) and reported as an unavailable guest.
        let hang = |_: &Query, _: Duration| async {
            std::future::pending::<()>().await;
            unreachable!()
        };
        let mut r = req(None);
        r.wait = Duration::from_millis(30);
        let t = Instant::now();
        let s = observe(&r, FAST, hang).await;
        assert!(
            t.elapsed() < Duration::from_millis(400),
            "{:?}",
            t.elapsed()
        );
        assert_eq!(s.error_code, Some(JobErrorCode::GuestUnreachable));
        assert_eq!(s.state, JobState::Unknown);
    }

    #[tokio::test]
    async fn a_check_that_hangs_at_the_deadline_keeps_the_last_good_snapshot() {
        // The first check answers (running, with output seen); the later ones hang.
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let n = calls.clone();
        let fetch = move |_: &Query, _: Duration| {
            let first = n.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
            async move {
                if first {
                    Ok(present(JobState::Running, 0, b"working\n", 8))
                } else {
                    std::future::pending::<()>().await;
                    unreachable!()
                }
            }
        };
        let mut r = req(None);
        r.wait = Duration::from_millis(100);
        let t = Instant::now();
        let s = observe(&r, FAST, fetch).await;
        let took = t.elapsed();
        assert!(
            took < r.wait + FAST.transport + FAST.grace + Duration::from_millis(150),
            "{took:?}"
        );
        assert!(calls.load(std::sync::atomic::Ordering::SeqCst) >= 2);
        // Not discarded, not an error: the state seen, flagged as an expired wait.
        assert_eq!(s.error, None);
        assert_eq!((s.state, s.wait_timed_out), (JobState::Running, true));
        assert_eq!(s.log_tail.as_deref(), Some("working\n"));
        assert!(
            s.note.as_deref().unwrap().contains("did not answer"),
            "{:?}",
            s.note
        );
        assert!(s.waited_ms >= 95);
        assert_eq!(s.into_result().is_error, Some(false));
    }

    #[tokio::test]
    async fn a_late_timeout_keeps_the_notes_of_the_snapshot_it_returns() {
        // A snapshot of a missing record carries "may still arrive, do not redo anything"; the
        // timeout explanation is added to it, never replaces it.
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let n = calls.clone();
        let fetch = move |_: &Query, _: Duration| {
            let first = n.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
            async move {
                if first {
                    Ok(GuestReport {
                        state: JobState::Missing,
                        ..GuestReport::default()
                    })
                } else {
                    std::future::pending::<()>().await;
                    unreachable!()
                }
            }
        };
        let mut r = req(None);
        r.wait = Duration::from_millis(80);
        let s = observe(&r, FAST, fetch).await;
        assert_eq!(
            (s.state, s.wait_timed_out, s.error.as_deref()),
            (JobState::Missing, true, None)
        );
        let note = s.note.as_deref().unwrap();
        assert!(note.contains("may still arrive"), "{note}");
        assert!(!note.to_lowercase().contains("retry"), "{note}");
        assert!(note.contains("did not answer in time"), "{note}");
        // Same for an unreadable log's note.
        let unreadable = GuestReport {
            state: JobState::Running,
            kind: Some(JobKind::Background),
            log: LogPresence::Unreadable,
            ..GuestReport::default()
        };
        let (_, fetch) = script(vec![Ok(unreadable), Err(Failure::timeout("slow"))]);
        let s = observe(&r, FAST, fetch).await;
        let note = s.note.as_deref().unwrap();
        assert!(
            note.contains("could not be read") && note.contains("did not answer in time"),
            "{note}"
        );
        // The text shows it too.
        assert!(render_text(&s).contains("could not be read"));
    }

    #[tokio::test]
    async fn a_refused_final_check_is_still_an_error_not_a_stale_state() {
        // A timeout keeps the earlier snapshot; a connection that fails outright does not.
        let (_, fetch) = script(vec![
            Ok(running()),
            Err(Failure::unreachable("connection refused")),
        ]);
        let mut r = req(None);
        r.wait = Duration::from_millis(50);
        let s = observe(&r, FAST, fetch).await;
        assert_eq!(s.error_code, Some(JobErrorCode::GuestUnreachable));
        // A timed-out final check with an earlier snapshot keeps it.
        let (_, fetch) = script(vec![Ok(running()), Err(Failure::timeout("no answer"))]);
        let s = observe(&r, FAST, fetch).await;
        assert_eq!(
            (s.state, s.wait_timed_out, s.error),
            (JobState::Running, true, None)
        );
        // ... and with no snapshot at all, the unavailable guest is reported.
        let (_, fetch) = script(vec![Err(Failure::timeout("no answer"))]);
        let s = observe(&r, FAST, fetch).await;
        assert_eq!(s.error_code, Some(JobErrorCode::GuestUnreachable));
    }

    // ---- script templating ------------------------------------------------------------

    /// Tokens a cursor may carry are valid characters, and some of them look like the
    /// scripts' own placeholders. Nothing a cursor says may be expanded into the script.
    #[test]
    fn cursor_fields_that_look_like_placeholders_stay_data() {
        let evil = |s: &str| Query {
            mode: Mode::Read(Expect {
                boot: s.into(),
                log: s.into(),
                head_len: 1,
                head_hash: "0123456789abcdef".into(),
                offset: 3,
            }),
            cap: REPLY_BYTES,
        };
        let plain = evil("plain");
        let linux_plain = linux_script(42, &plain);
        let windows_plain = windows_script(42, &plain);
        for name in [
            "__OWN__",
            "__BOOT__",
            "__FD__",
            "__SECTION__",
            "__CAP__",
            "__ID__",
            "__EO__",
            "__EB__",
            "__EL__",
            "__EHL__",
            "__EHH__",
        ] {
            let q = evil(name);
            // Same script, with only the two tokens' text different.
            assert_eq!(
                linux_script(42, &q),
                linux_plain.replace("plain", name),
                "linux {name}"
            );
            assert_eq!(
                windows_script(42, &q),
                windows_plain.replace("plain", name),
                "windows {name}"
            );
        }
        // And no placeholder of ours is left unfilled in a normal script.
        for script in [
            linux_plain,
            windows_plain,
            linux_script(
                42,
                &Query {
                    mode: Mode::Tail,
                    cap: 9,
                },
            ),
        ] {
            assert!(
                !script.contains("__"),
                "{}",
                script.lines().find(|l| l.contains("__")).unwrap()
            );
        }
    }
}

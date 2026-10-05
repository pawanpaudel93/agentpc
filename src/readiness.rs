//! wait_for_service: wait until a TCP port or an HTTP path in a VM answers.
//!
//! Only the VM's own IPv4 127.0.0.1 is probed, by a fixed read-only script run over SSH (a raw
//! socket under bash `/dev/tcp` and GNU `timeout` on Linux, a .NET `TcpClient` on Windows). The
//! guest prints only whitelisted words, so no response body, header or guest text reaches the
//! result. `wait` polls on a monotonic deadline, abandons a probe that hangs, ends early only for a
//! job proven to have failed, and stops probing (nothing else) when cancelled. The result type,
//! `Readiness`, is both the success and the error shape and the tool's declared output schema.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{CallToolResult, ContentBlock};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::jobs::{JobState, JobStatus, StopReason};

pub const SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 30;
pub const MAX_TIMEOUT_SECONDS: u64 = 120;
pub const DEFAULT_PATH: &str = "/";
pub const DEFAULT_STATUS: u16 = 200;
/// The longest path accepted, in characters, before escaping.
pub const MAX_PATH_CHARS: usize = 512;
/// The longest request target after escaping, in bytes.
pub const MAX_TARGET_BYTES: usize = 2048;

/// The timing of the polling loop; tests shorten it.
#[derive(Clone, Copy, Debug)]
pub struct Pacing {
    /// Pause between attempts.
    pub poll: Duration,
    /// The longest one probe is asked to run inside the guest.
    pub attempt_cap: Duration,
    /// The shortest guest budget an attempt gets, even with little time left.
    pub min_attempt: Duration,
    /// Allowance for the SSH round trip around the guest budget.
    pub transport: Duration,
    /// Extra time before an attempt that has not answered is abandoned.
    pub grace: Duration,
}

impl Pacing {
    pub const DEFAULT: Pacing = Pacing {
        poll: Duration::from_millis(500),
        attempt_cap: Duration::from_secs(5),
        min_attempt: Duration::from_secs(1),
        transport: Duration::from_secs(3),
        grace: Duration::from_millis(500),
    };
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum Protocol {
    /// Ready when a connection to the port is accepted.
    Tcp,
    /// Ready when one GET of the path answers the expected status.
    Http,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum ReadyState {
    /// The criterion was met. The only state that is not a tool error.
    Ready,
    /// The criterion was not met within `timeout_seconds`; the last observation is kept.
    TimedOut,
    /// Waiting ended early because the given job is proven to have failed.
    JobFailed,
    /// The request was cancelled; only probing stopped.
    Cancelled,
    /// The wait could not run or never got an answer (see `error_code`); the service's state is
    /// unknown, never guessed.
    Error,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum TcpObservation {
    /// A connection to 127.0.0.1:port was accepted.
    Accepted,
    /// The connection was refused: nothing is listening (on IPv4 loopback).
    Refused,
    /// The connection attempt did not complete within the probe's budget.
    TimedOut,
    /// The connection failed some other way.
    Failed,
    /// The guest cannot make the probe (no `/dev/tcp`, or no `timeout` command).
    Unsupported,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum HttpOutcome {
    /// The status line carried the expected status.
    StatusMatched,
    /// A 3xx status that is not the expected one; the redirect was not followed.
    Redirect,
    /// Another status than expected (not a redirect).
    StatusMismatched,
    /// The connection was accepted but no status line arrived in time.
    NoResponse,
    /// A reply that is not an HTTP status line.
    Malformed,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum ErrorCode {
    /// `guest_port` is not 1 to 65535.
    InvalidPort,
    /// `protocol` is not `tcp` or `http`.
    InvalidProtocol,
    /// `timeout_seconds` is not 1 to 120.
    InvalidTimeout,
    /// `path` is not an acceptable request path.
    InvalidPath,
    /// `expected_status` is not 100 to 599.
    InvalidStatus,
    /// An argument that does not apply (`path` or `expected_status` with tcp).
    InvalidArgument,
    /// No VM has that name.
    NoSuchVm,
    /// The VM is not running.
    VmNotRunning,
    /// No probe got an answer from the VM before the deadline.
    GuestUnreachable,
    /// The VM answered, but not with a probe report this server understands.
    BadGuestReply,
    /// The guest cannot make the probe (no `/dev/tcp`, or no `timeout` command).
    ProbeUnsupported,
    /// The criterion was not met within the timeout.
    Timeout,
    /// The given job is proven to have failed.
    JobFailed,
    /// The request was cancelled.
    RequestCancelled,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
/// What was last seen of the `job_id` given to the wait (only present with `job_id`).
pub struct JobObservation {
    /// The job's id, as passed.
    pub job_id: u64,
    /// The last state read with the same rules as get_job_status (`unknown` if it was never read).
    pub state: JobState,
    /// The job's exit code, when it has exited.
    pub exit_code: Option<i64>,
    /// Why the job stopped, when it was stopped.
    pub stop_reason: Option<StopReason>,
    /// Whether the job could be read at all; false means `state` proves nothing.
    pub observed: bool,
    /// What the observation shows or does not show (for example a missing record).
    pub note: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
/// The outcome of a wait_for_service call, for success and failure alike. Anything but `ready` is
/// a tool error. It never carries a response body, header or guest error text.
pub struct Readiness {
    /// Layout version of this object (currently 1).
    pub schema_version: u32,
    /// How the wait ended.
    pub state: ReadyState,
    /// The VM that was asked about.
    pub name: String,
    /// `tcp` or `http`; null when the arguments were refused.
    pub protocol: Option<Protocol>,
    /// The port that was checked, on the VM's own 127.0.0.1.
    pub guest_port: Option<u16>,
    /// http only: the request target actually sent (escaped), echoed back, so keep credentials out
    /// of it.
    pub path: Option<String>,
    /// http only: the status that counts as ready.
    pub expected_status: Option<u16>,
    /// The readiness criterion in words.
    pub criterion: Option<String>,
    /// Whether the last completed check met the criterion.
    pub criterion_met: bool,
    /// The last completed check's TCP result; null if no check completed.
    pub tcp: Option<TcpObservation>,
    /// http only: how the last completed check's reply compared with the expectation.
    pub http_outcome: Option<HttpOutcome>,
    /// http only: the status number of the last reply (the number only, never text).
    pub http_status: Option<u16>,
    /// How many probe attempts were started.
    pub attempts: u32,
    /// How long the call waited, in milliseconds (it can run about 5 s past the timeout).
    pub waited_ms: u64,
    /// The timeout that applied, in seconds.
    pub timeout_seconds: Option<u64>,
    /// Whether the criterion was not met within the timeout.
    pub timed_out: bool,
    /// Whether the request was cancelled (only probing stopped).
    pub cancelled: bool,
    /// The last observation of `job_id`; null without `job_id`.
    pub job: Option<JobObservation>,
    /// What the result does and does not prove (for example that a ready endpoint does not show
    /// which process serves it, or that an observation is from an earlier check).
    pub note: Option<String>,
    /// What went wrong and what to do next; null for `ready`.
    pub error: Option<String>,
    /// A stable code for `error`; null for `ready`.
    pub error_code: Option<ErrorCode>,
}

pub fn output_schema() -> Arc<rmcp::model::JsonObject> {
    rmcp::handler::server::tool::schema_for_output::<Readiness>()
}

impl Readiness {
    fn base(name: &str, state: ReadyState) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            state,
            name: name.to_string(),
            protocol: None,
            guest_port: None,
            path: None,
            expected_status: None,
            criterion: None,
            criterion_met: false,
            tcp: None,
            http_outcome: None,
            http_status: None,
            attempts: 0,
            waited_ms: 0,
            timeout_seconds: None,
            timed_out: false,
            cancelled: false,
            job: None,
            note: None,
            error: None,
            error_code: None,
        }
    }

    /// A call refused before anything was asked of the VM.
    pub fn failure(name: &str, code: ErrorCode, message: String) -> Self {
        let mut r = Self::base(name, ReadyState::Error);
        r.error = Some(message);
        r.error_code = Some(code);
        r
    }

    /// The readable text plus the structured result: the text is the only content block (no
    /// JSON mirror). Anything but `ready` is a tool error.
    pub fn into_result(self) -> CallToolResult {
        let text = render_text(&self);
        let structured = serde_json::to_value(&self).expect("readiness serializes");
        let mut r = if self.state == ReadyState::Ready {
            CallToolResult::success(vec![ContentBlock::text(text)])
        } else {
            CallToolResult::error(vec![ContentBlock::text(text)])
        };
        r.structured_content = Some(structured);
        r
    }
}

fn seconds(ms: u64) -> String {
    format!("{:.1} s", ms as f64 / 1000.0)
}

/// What the last completed check saw, in words.
fn last_check(r: &Readiness) -> String {
    let Some(tcp) = r.tcp else {
        return "no check completed".to_string();
    };
    let port = r.guest_port.map(|p| p.to_string()).unwrap_or_default();
    match (tcp, r.http_outcome, r.http_status) {
        (TcpObservation::Accepted, None, _) => format!("a connection to port {port} was accepted"),
        (TcpObservation::Accepted, Some(HttpOutcome::NoResponse), _) => {
            format!("port {port} accepted a connection but sent no HTTP status in time")
        }
        (TcpObservation::Accepted, Some(HttpOutcome::Malformed), _) => {
            format!("port {port} accepted a connection but did not answer with an HTTP status line")
        }
        (TcpObservation::Accepted, Some(HttpOutcome::Redirect), Some(s)) => {
            format!("HTTP status {s} (a redirect; redirects are not followed)")
        }
        (TcpObservation::Accepted, Some(_), Some(s)) => format!("HTTP status {s}"),
        (TcpObservation::Accepted, Some(_), None) => "an HTTP reply".to_string(),
        (TcpObservation::Refused, ..) => format!("port {port} refused the connection"),
        (TcpObservation::TimedOut, ..) => format!("port {port} did not accept within the attempt"),
        (TcpObservation::Failed, ..) => format!("the connection to port {port} failed"),
        (TcpObservation::Unsupported, ..) => "the guest cannot make the probe".to_string(),
    }
}

/// The text of a result, the only content block.
pub fn render_text(r: &Readiness) -> String {
    let who = match (r.protocol, r.guest_port) {
        (Some(Protocol::Http), Some(p)) => format!(
            "GET {} on 127.0.0.1:{p} in VM {}",
            r.path.as_deref().unwrap_or("/"),
            r.name
        ),
        (Some(Protocol::Tcp), Some(p)) => format!("TCP port 127.0.0.1:{p} in VM {}", r.name),
        _ => format!("VM {}", r.name),
    };
    let tally = format!(
        "{} attempt{}, {}",
        r.attempts,
        if r.attempts == 1 { "" } else { "s" },
        seconds(r.waited_ms)
    );
    let mut out = match r.state {
        ReadyState::Ready => format!("ready: {who} ({tally}). Last check: {}.", last_check(r)),
        ReadyState::TimedOut => format!(
            "not ready: {who} did not meet the criterion within {} s ({tally}). Last check: {}.",
            r.timeout_seconds.unwrap_or(0),
            last_check(r)
        ),
        ReadyState::JobFailed => format!(
            "stopped waiting: {who} ({tally}). Last check: {}.",
            last_check(r)
        ),
        ReadyState::Cancelled => {
            format!("cancelled: {who} ({tally}). Last check: {}.", last_check(r))
        }
        ReadyState::Error => format!("could not wait: {who}."),
    };
    if let Some(e) = &r.error {
        out.push_str(&format!("\n{e}"));
    }
    if let Some(j) = &r.job {
        let state = if j.observed {
            format!("{:?}", j.state).to_lowercase()
        } else {
            "unknown (could not be read)".to_string()
        };
        let code = j
            .exit_code
            .map(|c| format!(", exit code {c}"))
            .unwrap_or_default();
        out.push_str(&format!("\nJob {}: {state}{code}.", j.job_id));
    }
    if let Some(n) = &r.note {
        out.push_str(&format!("\n{n}"));
    }
    out
}

/// The request target for `path`: validated, then escaped in one pass.
///
/// A path is absolute on the VM's own server: it starts with `/` but not `//` (that would name
/// a host), has no control character, backslash or `#` fragment (never sent to a server). A
/// query is kept. One scan over the UTF-8 bytes: unreserved characters and `/ ? = & : @ + , ;`
/// stay, a `%` that already starts a `%XX` stays (so nothing is escaped twice), and every other
/// byte, a stray `%` included, becomes `%XX`. The result is therefore made only of
/// `[A-Za-z0-9-._~/?=&:@+,;%]`.
pub fn encode_path(path: &str) -> Result<String, String> {
    if path.chars().count() > MAX_PATH_CHARS {
        return Err(format!(
            "path is longer than {MAX_PATH_CHARS} characters; give a shorter one"
        ));
    }
    if let Some(c) = path.chars().find(|c| c.is_control()) {
        return Err(format!(
            "path has a control character (U+{:04X}); use a plain path such as /health",
            c as u32
        ));
    }
    if !path.starts_with('/') {
        return Err(
            "path must be a path on the VM's own server and start with / (for \
             example /health): a URL, scheme, host or port is not accepted"
                .to_string(),
        );
    }
    if path.starts_with("//") {
        return Err(
            "path must not start with // (that would name a host); give a path such \
             as /health"
                .to_string(),
        );
    }
    if path.contains('\\') {
        return Err("path must not contain a backslash; use / between segments".to_string());
    }
    if path.contains('#') {
        return Err(
            "path must not contain a # fragment: a fragment is never sent to a \
             server; remove it"
                .to_string(),
        );
    }
    let bytes = path.as_bytes();
    let mut out = String::with_capacity(path.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let escaped = i + 2 < bytes.len()
            && bytes[i + 1].is_ascii_hexdigit()
            && bytes[i + 2].is_ascii_hexdigit();
        if b.is_ascii_alphanumeric() || b"-._~/?=&:@+,;".contains(&b) {
            out.push(b as char);
        } else if b == b'%' && escaped {
            out.push_str(&path[i..i + 3]);
            i += 2;
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
        i += 1;
    }
    if out.len() > MAX_TARGET_BYTES {
        return Err(format!(
            "path is longer than {MAX_TARGET_BYTES} bytes once escaped; give a shorter one"
        ));
    }
    Ok(out)
}

#[derive(Clone, Debug)]
pub struct Request {
    pub name: String,
    pub port: u16,
    pub protocol: Protocol,
    pub timeout: Duration,
    /// http: the escaped request target; only `encode_path` makes one.
    target: Option<String>,
    pub expected: Option<u16>,
    pub job_id: Option<u64>,
}

impl Request {
    /// Check the arguments; a bad one is a result that says which and what to do instead.
    /// Nothing is asked of any VM here.
    pub fn new(
        name: &str,
        guest_port: i64,
        protocol: Option<&str>,
        timeout_seconds: Option<i64>,
        path: Option<&str>,
        expected_status: Option<i64>,
        job_id: Option<u64>,
    ) -> Result<Self, Box<Readiness>> {
        let bad = |code, msg: String| Box::new(Readiness::failure(name, code, msg));
        let port = u16::try_from(guest_port)
            .ok()
            .filter(|p| *p != 0)
            .ok_or_else(|| {
                bad(
                    ErrorCode::InvalidPort,
                    format!("guest_port must be between 1 and 65535 (got {guest_port})."),
                )
            })?;
        let protocol = match protocol.map(str::to_ascii_lowercase).as_deref() {
            None | Some("tcp") => Protocol::Tcp,
            Some("http") => Protocol::Http,
            Some(other) => {
                return Err(bad(
                    ErrorCode::InvalidProtocol,
                    format!(
                        "protocol must be \"tcp\" or \"http\" (got {:?}). https, udp and other \
                         schemes are not supported: this checks plain TCP or HTTP on the VM's \
                         127.0.0.1.",
                        crate::execution::clip(other, 20)
                    ),
                ));
            }
        };
        let timeout = match timeout_seconds {
            None => DEFAULT_TIMEOUT_SECONDS,
            Some(t) => u64::try_from(t)
                .ok()
                .filter(|t| (1..=MAX_TIMEOUT_SECONDS).contains(t))
                .ok_or_else(|| {
                    bad(
                        ErrorCode::InvalidTimeout,
                        format!(
                            "timeout_seconds must be between 1 and {MAX_TIMEOUT_SECONDS} (got \
                             {t}). To wait longer, call wait_for_service again: a wait only \
                             waits and never stops the service."
                        ),
                    )
                })?,
        };
        let status = match expected_status {
            None => None,
            Some(s) => Some(
                u16::try_from(s)
                    .ok()
                    .filter(|s| (100..=599).contains(s))
                    .ok_or_else(|| {
                        bad(
                            ErrorCode::InvalidStatus,
                            format!(
                                "expected_status must be an HTTP status from 100 to 599 (got {s})."
                            ),
                        )
                    })?,
            ),
        };
        let (target, expected) = match protocol {
            Protocol::Tcp if path.is_some() || status.is_some() => {
                return Err(bad(
                    ErrorCode::InvalidArgument,
                    "path and expected_status apply only to protocol \"http\"; for tcp the \
                     criterion is that a connection is accepted."
                        .to_string(),
                ));
            }
            Protocol::Tcp => (None, None),
            Protocol::Http => {
                let target = encode_path(path.unwrap_or(DEFAULT_PATH))
                    .map_err(|m| bad(ErrorCode::InvalidPath, m))?;
                (Some(target), Some(status.unwrap_or(DEFAULT_STATUS)))
            }
        };
        Ok(Self {
            name: name.to_string(),
            port,
            protocol,
            timeout: Duration::from_secs(timeout),
            target,
            expected,
            job_id,
        })
    }

    /// The same request with another wait budget: the tool's own is whole seconds, tests wait
    /// for milliseconds.
    #[allow(dead_code)]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// A result about this request, before anything has been learned.
    fn result(&self, state: ReadyState) -> Readiness {
        let mut r = Readiness::base(&self.name, state);
        r.protocol = Some(self.protocol);
        r.guest_port = Some(self.port);
        r.path = self.target.clone();
        r.expected_status = self.expected;
        r.timeout_seconds = Some(self.timeout.as_secs());
        r.criterion = Some(match (&self.target, self.expected) {
            (Some(t), Some(s)) => format!(
                "GET {t} on the VM's 127.0.0.1:{} answers with HTTP status {s} (redirects are \
                 not followed)",
                self.port
            ),
            _ => format!("the VM's 127.0.0.1:{} accepts a TCP connection", self.port),
        });
        if let Some(id) = self.job_id {
            r.job = Some(JobObservation {
                job_id: id,
                state: JobState::Unknown,
                exit_code: None,
                stop_reason: None,
                observed: false,
                note: None,
            });
        }
        r
    }

    /// A refusal that has a valid request behind it (the VM is missing or stopped).
    pub fn failure(&self, code: ErrorCode, message: String) -> Readiness {
        let mut r = self.result(ReadyState::Error);
        r.error = Some(message);
        r.error_code = Some(code);
        r
    }

    /// What the guest is asked.
    pub fn query(&self) -> Query {
        Query {
            protocol: self.protocol,
            port: self.port,
            target: self.target.clone().unwrap_or_default(),
            job_id: self.job_id,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Query {
    pub protocol: Protocol,
    pub port: u16,
    /// http: the escaped request target; empty for tcp.
    target: String,
    pub job_id: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Connect {
    Open,
    Refused,
    TimedOut,
    Failed,
    Unsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpReply {
    Status(u16),
    NoResponse,
    Malformed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GuestProbe {
    pub connect: Connect,
    pub http: Option<HttpReply>,
}

/// Parse a probe script's output. Only `AGENTPC_READY` lines count and the report is complete
/// only when its `end ok` line arrived, so a connection that died midway is an error, not a
/// state. Each word must be one the script prints; the errors are fixed phrases, so nothing the
/// guest printed is ever repeated.
pub fn parse_probe(out: &[u8], protocol: Protocol) -> Result<GuestProbe, String> {
    let text = String::from_utf8_lossy(out);
    let (mut connect, mut http, mut end) = (None, None, false);
    for line in text.lines() {
        let Some(rest) = line.trim_end().strip_prefix("AGENTPC_READY ") else {
            continue;
        };
        let (key, value) = rest.split_once(' ').unwrap_or((rest, ""));
        match key {
            "connect" => {
                let c = match value {
                    "open" => Connect::Open,
                    "refused" => Connect::Refused,
                    "timeout" => Connect::TimedOut,
                    "failed" => Connect::Failed,
                    "unsupported" => Connect::Unsupported,
                    _ => return Err("the probe reported an unknown connect result".to_string()),
                };
                if connect.replace(c).is_some() {
                    return Err("the probe reported its connection twice".to_string());
                }
            }
            "http" => {
                let h = match value {
                    "none" => HttpReply::NoResponse,
                    "malformed" => HttpReply::Malformed,
                    v if v.len() == 3 && v.bytes().all(|b| b.is_ascii_digit()) => {
                        match v.parse::<u16>() {
                            Ok(s) if (100..=599).contains(&s) => HttpReply::Status(s),
                            _ => {
                                return Err("the probe reported an unknown HTTP status".to_string());
                            }
                        }
                    }
                    _ => return Err("the probe reported an unknown HTTP result".to_string()),
                };
                if http.replace(h).is_some() {
                    return Err("the probe reported its HTTP reply twice".to_string());
                }
            }
            "end" if value == "ok" => end = true,
            _ => {}
        }
    }
    if !end {
        return Err("the probe's report was cut short".to_string());
    }
    let connect = connect.ok_or_else(|| "the probe did not report a connection".to_string())?;
    if protocol == Protocol::Http && connect == Connect::Open && http.is_none() {
        return Err("the probe did not report an HTTP reply".to_string());
    }
    if protocol == Protocol::Tcp || connect != Connect::Open {
        http = None;
    }
    Ok(GuestProbe { connect, http })
}

/// Why a probe produced no report. Its text is made of fixed phrases and numbers only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeFailure {
    pub kind: FailureKind,
    pub detail: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    /// SSH failed, or the VM did not answer in time.
    Unreachable,
    /// The VM answered with something this server does not understand.
    BadReply,
}

impl ProbeFailure {
    pub fn unreachable(detail: &str) -> Self {
        Self {
            kind: FailureKind::Unreachable,
            detail: detail.to_string(),
        }
    }

    pub fn bad_reply(detail: &str) -> Self {
        Self {
            kind: FailureKind::BadReply,
            detail: detail.to_string(),
        }
    }
}

/// One round of a wait: the probe and, with a `job_id`, the job's state read at the same time.
#[derive(Debug)]
pub struct Attempt {
    pub probe: Result<GuestProbe, ProbeFailure>,
    pub job: Option<JobStatus>,
}

/// Run the probe and the optional job read together. Dropping the future drops both.
pub async fn attempt<P, J>(probe: P, job: Option<J>) -> Attempt
where
    P: Future<Output = Result<GuestProbe, ProbeFailure>>,
    J: Future<Output = JobStatus>,
{
    match job {
        None => Attempt {
            probe: probe.await,
            job: None,
        },
        Some(job) => {
            let (probe, job) = tokio::join!(probe, job);
            Attempt {
                probe,
                job: Some(job),
            }
        }
    }
}

/// A probe, judged against the criterion.
#[derive(Clone, Copy, Debug)]
struct Seen {
    tcp: TcpObservation,
    http: Option<(HttpOutcome, Option<u16>)>,
    ready: bool,
}

fn judge(req: &Request, p: &GuestProbe) -> Seen {
    let tcp = match p.connect {
        Connect::Open => TcpObservation::Accepted,
        Connect::Refused => TcpObservation::Refused,
        Connect::TimedOut => TcpObservation::TimedOut,
        Connect::Failed => TcpObservation::Failed,
        Connect::Unsupported => TcpObservation::Unsupported,
    };
    if req.protocol == Protocol::Tcp {
        return Seen {
            tcp,
            http: None,
            ready: p.connect == Connect::Open,
        };
    }
    let http = match (p.connect, p.http) {
        (Connect::Open, Some(HttpReply::Status(got))) => {
            let want = req.expected.unwrap_or(DEFAULT_STATUS);
            let outcome = if got == want {
                HttpOutcome::StatusMatched
            } else if (300..400).contains(&got) {
                HttpOutcome::Redirect
            } else {
                HttpOutcome::StatusMismatched
            };
            Some((outcome, Some(got)))
        }
        (Connect::Open, Some(HttpReply::Malformed)) => Some((HttpOutcome::Malformed, None)),
        (Connect::Open, _) => Some((HttpOutcome::NoResponse, None)),
        _ => None,
    };
    Seen {
        tcp,
        http,
        ready: matches!(http, Some((HttpOutcome::StatusMatched, _))),
    }
}

fn job_observation(id: u64, s: &JobStatus) -> JobObservation {
    if s.error.is_some() {
        return JobObservation {
            job_id: id,
            state: JobState::Unknown,
            exit_code: None,
            stop_reason: None,
            observed: false,
            note: Some("the job's state could not be read this time".to_string()),
        };
    }
    JobObservation {
        job_id: id,
        state: s.state,
        exit_code: s.exit_code,
        stop_reason: s.stop_reason,
        observed: true,
        note: None,
    }
}

/// What a job's state proves about its failure, when it proves one: a nonzero exit, a stop or
/// cancellation, a launch that was cancelled before it started. A zero exit is compatible with
/// a server that outlives its wrapper; queued, running, ended without a code, missing, unknown
/// and unreadable prove nothing.
fn job_failure(j: &JobObservation) -> Option<String> {
    if !j.observed {
        return None;
    }
    let id = j.job_id;
    match j.state {
        JobState::Exited => match j.exit_code {
            Some(c) if c != 0 => Some(format!("job {id} exited with code {c}")),
            _ => None,
        },
        JobState::Stopped => Some(format!("job {id} was stopped")),
        JobState::NotStarted => Some(format!("job {id} never started and now never will")),
        _ => None,
    }
}

#[derive(Debug)]
enum Outcome {
    Ready,
    JobFailed(String),
    TimedOut,
    Fatal(ErrorCode, String),
    Cancelled,
}

/// What a wait has learned so far. It lives outside the polling future, so a cancelled wait
/// still reports it.
#[derive(Default)]
struct Tracker {
    attempts: u32,
    seen: Option<Seen>,
    failure: Option<ProbeFailure>,
    job: Option<JobObservation>,
    /// An attempt has started and not been absorbed: if the wait ends now (a cancellation),
    /// that attempt never completed and `seen` is from an earlier one.
    in_flight: bool,
}

impl Tracker {
    /// Why the newest attempt did not complete, when it did not and an older observation is
    /// all there is: said wherever that observation is shown, whatever way the wait ended.
    fn incomplete_latest(&self) -> Option<String> {
        self.seen.as_ref()?;
        // A later attempt still running is the reason; `failure` may be an earlier attempt's.
        match (&self.failure, self.in_flight) {
            (_, true) => Some("the wait ended while it was running".to_string()),
            (Some(f), false) => Some(f.detail.clone()),
            (None, false) => None,
        }
    }

    /// Fold one attempt in; returns how the wait ends, if it does.
    fn absorb(&mut self, req: &Request, a: Attempt) -> Option<Outcome> {
        if let (Some(id), Some(s)) = (req.job_id, &a.job) {
            let now = job_observation(id, s);
            // A read that failed this time leaves the last good one in place.
            if now.observed || self.job.as_ref().is_none_or(|j| !j.observed) {
                self.job = Some(now);
            }
        }
        match a.probe {
            Err(f) => self.failure = Some(f),
            Ok(p) => {
                self.failure = None;
                let seen = judge(req, &p);
                self.seen = Some(seen);
                if p.connect == Connect::Unsupported {
                    return Some(Outcome::Fatal(
                        ErrorCode::ProbeUnsupported,
                        "The VM cannot make this probe (its bash has no /dev/tcp, or it has no \
                         `timeout` command). Check the port from inside the VM with run_command \
                         instead."
                            .to_string(),
                    ));
                }
                if seen.ready {
                    return Some(Outcome::Ready);
                }
            }
        }
        self.job
            .as_ref()
            .and_then(job_failure)
            .map(Outcome::JobFailed)
    }
}

/// Probe until the criterion holds. `fetch` runs one attempt, given the query and the time its
/// guest-side probe may take. The wait ends when the criterion holds (`ready`); the deadline
/// passes (`timed_out`, keeping the last check; with no check ever completed, `error` with the
/// last failure, never a guess); the optional job is proven to have failed (`job_failed`); or
/// `cancelled` completes. Dropping this future does the same as cancelling. Either way only
/// probing stops: the attempt in flight is dropped with the SSH call it owns, nothing is
/// retried, and the service and the job are untouched.
///
/// A probe gets the time left (at least `min_attempt`, at most `attempt_cap`) plus `transport`,
/// and is abandoned `grace` after that, so a call can overrun its timeout by at most
/// `min_attempt + transport + grace`.
pub async fn wait<F, Fut, C>(req: &Request, pacing: Pacing, mut fetch: F, cancelled: C) -> Readiness
where
    F: FnMut(&Query, Duration) -> Fut,
    Fut: Future<Output = Attempt>,
    C: Future<Output = ()>,
{
    use futures::FutureExt;
    let mut cancelled = Box::pin(cancelled);
    let mut tracker = Tracker::default();
    if cancelled.as_mut().now_or_never().is_some() {
        return finish(req, tracker, Outcome::Cancelled, Duration::ZERO);
    }
    let started = Instant::now();
    let deadline = started + req.timeout;
    let query = req.query();
    let outcome = {
        let run = poll(req, &query, pacing, &mut fetch, &mut tracker, deadline);
        tokio::pin!(run);
        tokio::select! {
            biased;
            _ = &mut cancelled => Outcome::Cancelled,
            o = &mut run => o,
        }
    };
    finish(req, tracker, outcome, started.elapsed())
}

async fn poll<F, Fut>(
    req: &Request,
    query: &Query,
    pacing: Pacing,
    fetch: &mut F,
    tracker: &mut Tracker,
    deadline: Instant,
) -> Outcome
where
    F: FnMut(&Query, Duration) -> Fut,
    Fut: Future<Output = Attempt>,
{
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let budget = left.clamp(
            pacing.min_attempt,
            pacing.attempt_cap.max(pacing.min_attempt),
        );
        tracker.attempts += 1;
        tracker.in_flight = true;
        let window = budget + pacing.transport + pacing.grace;
        let result = tokio::time::timeout(window, fetch(query, budget)).await;
        tracker.in_flight = false;
        match result {
            Err(_) => {
                tracker.failure = Some(ProbeFailure::unreachable(&format!(
                    "no answer within {:.0} s",
                    window.as_secs_f64()
                )));
            }
            Ok(a) => {
                if let Some(done) = tracker.absorb(req, a) {
                    return done;
                }
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Outcome::TimedOut;
        }
        tokio::time::sleep(pacing.poll.min(left)).await;
        if Instant::now() >= deadline {
            return Outcome::TimedOut;
        }
    }
}

const ENDPOINT_NOTE: &str = "Readiness describes the endpoint, not a process: it does not show \
     which process is serving it, so it does not prove that the job (or any particular program) \
     started it.";

fn finish(req: &Request, tr: Tracker, outcome: Outcome, elapsed: Duration) -> Readiness {
    let state = match &outcome {
        Outcome::Ready => ReadyState::Ready,
        Outcome::JobFailed(_) => ReadyState::JobFailed,
        Outcome::TimedOut => ReadyState::TimedOut,
        Outcome::Cancelled => ReadyState::Cancelled,
        Outcome::Fatal(..) => ReadyState::Error,
    };
    // The same words wherever an older observation is shown after a newer attempt failed.
    let stale = tr.incomplete_latest().map(|detail| {
        format!(" The last check did not complete ({detail}), so the observation above is from an earlier one.")
    });
    let mut r = req.result(state);
    r.attempts = tr.attempts;
    r.waited_ms = elapsed.as_millis() as u64;
    if let Some(s) = tr.seen {
        r.tcp = Some(s.tcp);
        if let Some((o, status)) = s.http {
            r.http_outcome = Some(o);
            r.http_status = status;
        }
        r.criterion_met = s.ready;
    }
    if tr.job.is_some() {
        r.job = tr.job;
    }
    let mut note = String::new();
    match outcome {
        Outcome::Ready => {
            let proves = match (req.protocol, req.expected) {
                (Protocol::Http, Some(s)) => format!(
                    "An HTTP {s} for GET {} proves only that status criterion",
                    r.path.as_deref().unwrap_or("/")
                ),
                _ => format!(
                    "A TCP connection proves only that something accepted a connection on \
                     127.0.0.1:{} in the VM",
                    req.port
                ),
            };
            note = format!("{proves}. {ENDPOINT_NOTE}");
            if let Some(j) = r.job.as_ref().and_then(job_failure) {
                note.push_str(&format!(
                    " ({j}, yet the endpoint answers: something else, or a server the job left \
                     running, is serving it.)"
                ));
            }
        }
        Outcome::TimedOut if tr.seen.is_none() => {
            // No probe ever returned a report: the VM never answered, which is not "not ready".
            let (code, what) = match tr.failure.as_ref().map(|f| f.kind) {
                Some(FailureKind::BadReply) => (
                    ErrorCode::BadGuestReply,
                    "The VM answered, but not with a probe report this server understands",
                ),
                _ => (
                    ErrorCode::GuestUnreachable,
                    "The VM did not answer any probe",
                ),
            };
            r.state = ReadyState::Error;
            r.error_code = Some(code);
            r.error = Some(format!(
                "{what} within {} s ({}). The service's state is unknown. Check list_vms, then \
                 call wait_for_service again.",
                req.timeout.as_secs(),
                tr.failure
                    .as_ref()
                    .map_or("no detail", |f| f.detail.as_str())
            ));
        }
        Outcome::TimedOut => {
            r.timed_out = true;
            r.error_code = Some(ErrorCode::Timeout);
            r.error = Some(format!(
                "The criterion was not met within {} s. Nothing was stopped or restarted: the \
                 service and any job are as they were. Call wait_for_service again to keep \
                 waiting, or look at the service's own log.",
                req.timeout.as_secs()
            ));
            if let Some(j) = r
                .job
                .as_ref()
                .filter(|j| j.observed && j.state == JobState::Exited && j.exit_code == Some(0))
            {
                note = format!(
                    "Job {} exited with code 0, which is compatible with a server that outlives \
                     its wrapper, so it did not end the wait; but nothing is answering yet.",
                    j.job_id
                );
            }
            note.push_str(stale.as_deref().unwrap_or(""));
        }
        Outcome::JobFailed(why) => {
            r.error_code = Some(ErrorCode::JobFailed);
            let checked = if stale.is_some() {
                "The endpoint had not met the criterion at an earlier check; the latest check \
                 did not complete, so its current state is unknown."
            } else if tr.seen.is_some() {
                "The endpoint did not meet the criterion at the last check."
            } else {
                "No check of the endpoint had completed yet, so its state is unknown."
            };
            r.error = Some(format!(
                "Stopped waiting early: {why}. {checked} Nothing was stopped or restarted by \
                 this wait. Read the job's log with get_job_status; call wait_for_service \
                 without job_id to keep waiting for the endpoint regardless."
            ));
            note = format!("{ENDPOINT_NOTE}{}", stale.as_deref().unwrap_or(""));
        }
        Outcome::Cancelled => {
            r.cancelled = true;
            r.error_code = Some(ErrorCode::RequestCancelled);
            r.error = Some(
                "The wait_for_service request was cancelled; only probing stopped. The service \
                 and any job were not touched."
                    .to_string(),
            );
            note.push_str(stale.as_deref().unwrap_or(""));
        }
        Outcome::Fatal(code, message) => {
            r.error_code = Some(code);
            r.error = Some(message);
        }
    }
    if !note.is_empty() {
        r.note = Some(note.trim_start().to_string());
    }
    r
}

// ---------------------------------------------------------------------------------------------
// Guest scripts. They write nothing in the VM and print only whitelisted words.

/// `s` as one bash word: single-quoted, so nothing in it is expanded.
fn bash_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// `s` as one PowerShell string: single-quoted, so nothing in it is expanded.
fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Whole seconds to give a guest-side probe: the budget rounded up, at least 1.
fn whole_seconds(budget: Duration) -> u64 {
    budget.as_secs_f64().ceil().max(1.0) as u64
}

/// Replace each placeholder in turn.
fn fill(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (k, v) in values {
        out = out.replace(k, v);
    }
    out
}

/// Ubuntu and Arch. A raw `/dev/tcp` socket to the literal 127.0.0.1, so no proxy variable,
/// name lookup or redirect logic can matter, under GNU `timeout` so a listener that never
/// accepts cannot hold it. The locale is fixed because the one error word it reads must not be
/// translated. The body is fixed text in single quotes: the port, seconds, and path reach it
/// as arguments (the path single-quoted as well). For HTTP it writes one fixed GET and reads
/// the first line only, at most 256 bytes, and prints just a three-digit status from it.
const LINUX_PROBE: &str = r#"LC_ALL=C; export LC_ALL
emit() { printf 'AGENTPC_READY %s\n' "$*"; }
port=__PORT__; secs=__SECS__; proto=__PROTO__; path=__PATH__
body='
if ! exec 3<>"/dev/tcp/127.0.0.1/$1" 2>/dev/null; then
  d=$( (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>&1 )
  case $d in
    *"Connection refused"*) echo failed refused ;;
    *"No such file or directory"*) echo failed unsupported ;;
    *) echo failed other ;;
  esac
  exit 3
fi
echo connected
[ "$4" = http ] || exit 0
printf "GET %s HTTP/1.1\r\nHost: 127.0.0.1:%s\r\nUser-Agent: agentpc-readiness\r\nAccept: */*\r\nConnection: close\r\n\r\n" "$2" "$1" >&3 || { echo http none; exit 0; }
line=; IFS= read -r -t "$3" -n 256 line <&3
cr=$(printf "\r"); line=${line%"$cr"}
case $line in
  "HTTP/"[0-9]"."[0-9]" "[1-5][0-9][0-9]|"HTTP/"[0-9]"."[0-9]" "[1-5][0-9][0-9]" "*) echo "http ${line:9:3}" ;;
  "") echo http none ;;
  *) echo http malformed ;;
esac
'
out=$(timeout -s KILL "$secs" bash -c "$body" _ "$port" "$path" "$secs" "$proto" 2>/dev/null); rc=$?
conn=; failed=; http=
while IFS= read -r l; do
  case $l in
    connected) conn=1 ;;
    "failed refused"|"failed unsupported"|"failed other") failed=${l#failed } ;;
    "http "[1-5][0-9][0-9]|"http none"|"http malformed") http=${l#http } ;;
  esac
done <<EOF
$out
EOF
if [ -n "$conn" ]; then
  emit connect open
  if [ "$proto" = http ]; then emit http "${http:-none}"; fi
elif [ -n "$failed" ]; then
  case $failed in
    refused) emit connect refused ;;
    unsupported) emit connect unsupported ;;
    *) emit connect failed ;;
  esac
elif [ "$rc" = 124 ] || [ "$rc" = 137 ]; then emit connect timeout
elif [ "$rc" = 127 ]; then emit connect unsupported
else emit connect failed; fi
emit end ok"#;

/// wait_for_service's bash.
pub fn linux_probe_script(q: &Query, budget: Duration) -> String {
    let proto = match q.protocol {
        Protocol::Tcp => "tcp",
        Protocol::Http => "http",
    };
    fill(
        LINUX_PROBE,
        &[
            ("__PORT__", &q.port.to_string()),
            ("__SECS__", &whole_seconds(budget).to_string()),
            ("__PROTO__", proto),
            ("__PATH__", &bash_quote(&q.target)),
        ],
    )
}

/// Windows PowerShell 5.1 (and 7): a raw `TcpClient` to the literal 127.0.0.1, which no proxy
/// setting touches. Connect and reads share one stopwatch; an exception is classified and its
/// text thrown away.
const WINDOWS_PROBE: &str = r#"function Emit($k, $v) { "AGENTPC_READY $k $v" }
$port = __PORT__; $ms = __MS__; $http = __HTTP__; $path = __PATH__
$sw = [Diagnostics.Stopwatch]::StartNew()
$c = New-Object System.Net.Sockets.TcpClient
$conn = 'failed'; $reply = 'none'
try {
  $ar = $c.BeginConnect('127.0.0.1', $port, $null, $null)
  if (-not $ar.AsyncWaitHandle.WaitOne($ms, $false)) { $conn = 'timeout' }
  else {
    try { $c.EndConnect($ar); $conn = 'open' } catch {
      $e = $_.Exception
      while ($e.InnerException) { $e = $e.InnerException }
      if (($e -is [System.Net.Sockets.SocketException]) -and $e.SocketErrorCode -eq [System.Net.Sockets.SocketError]::ConnectionRefused) { $conn = 'refused' }
    }
  }
  if ($conn -eq 'open' -and $http) {
    $line = ''
    try {
      $s = $c.GetStream()
      $s.ReadTimeout = [int][math]::Max(100, $ms - $sw.ElapsedMilliseconds); $s.WriteTimeout = $ms
      $crlf = "`r`n"
      $req = [Text.Encoding]::ASCII.GetBytes('GET ' + $path + ' HTTP/1.1' + $crlf + 'Host: 127.0.0.1:' + $port + $crlf + 'User-Agent: agentpc-readiness' + $crlf + 'Accept: */*' + $crlf + 'Connection: close' + $crlf + $crlf)
      $s.Write($req, 0, $req.Length)
      $buf = New-Object byte[] 256; $n = 0
      while ($n -lt 256 -and $sw.ElapsedMilliseconds -lt $ms) {
        $b = $s.ReadByte()
        if ($b -lt 0 -or $b -eq 10) { break }
        $buf[$n] = [byte]$b; $n++
      }
      $line = [Text.Encoding]::ASCII.GetString($buf, 0, $n).TrimEnd("`r")
    } catch { }
    if ($line -cmatch '^HTTP/[0-9]\.[0-9] ([1-5][0-9][0-9])( |$)') { $reply = $Matches[1] }
    elseif ($line -ne '') { $reply = 'malformed' }
  }
} catch { }
try { $c.Close() } catch { }
Emit connect $conn
if ($conn -eq 'open' -and $http) { Emit http $reply }
Emit end ok"#;

/// wait_for_service's PowerShell.
pub fn windows_probe_script(q: &Query, budget: Duration) -> String {
    let http = match q.protocol {
        Protocol::Tcp => "$false",
        Protocol::Http => "$true",
    };
    fill(
        WINDOWS_PROBE,
        &[
            ("__PORT__", &q.port.to_string()),
            ("__MS__", &(whole_seconds(budget) * 1000).to_string()),
            ("__HTTP__", http),
            ("__PATH__", &ps_quote(&q.target)),
        ],
    )
}

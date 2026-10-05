//! Tests of wait_for_service (src/readiness.rs): its argument checks, the polling loop with
//! scripted probes, and the generated guest scripts run for real against local TCP and HTTP
//! servers: the bash one under local bash with GNU `timeout` (any Linux; on a Mac, Homebrew's
//! coreutils), and the PowerShell one under pwsh when this machine has it.
//!
//! A test binary of its own, like tests/execution.rs and tests/jobs.rs: cargo runs binaries one
//! after another, so the processes and sockets spawned here cannot disturb the lock tests in
//! the main binary.

#[allow(dead_code)]
#[path = "../src/execution.rs"]
mod execution;
#[allow(dead_code)]
#[path = "../src/jobs.rs"]
mod jobs;
#[allow(dead_code)]
#[path = "../src/readiness.rs"]
mod readiness;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use readiness::*;

// =============================================================================================
// Arguments

fn args(
    port: i64,
    protocol: Option<&str>,
    timeout: Option<i64>,
    path: Option<&str>,
    status: Option<i64>,
) -> Result<Request, Box<Readiness>> {
    Request::new("vm", port, protocol, timeout, path, status, None)
}

fn code(r: Result<Request, Box<Readiness>>) -> ErrorCode {
    let e = r.expect_err("must be refused");
    assert_eq!(e.state, ReadyState::Error);
    assert!(e.error.is_some());
    assert!(!e.criterion_met && e.attempts == 0 && !e.timed_out && !e.cancelled);
    e.error_code.unwrap()
}

#[test]
fn ports_must_be_1_to_65535() {
    for bad in [0, -1, 65536, 70000, i64::MAX, i64::MIN] {
        assert_eq!(
            code(args(bad, None, None, None, None)),
            ErrorCode::InvalidPort,
            "{bad}"
        );
    }
    for good in [1, 80, 65535] {
        assert_eq!(
            args(good, None, None, None, None).unwrap().port,
            good as u16
        );
    }
}

#[test]
fn protocol_is_tcp_by_default_and_only_tcp_or_http() {
    assert_eq!(
        args(80, None, None, None, None).unwrap().protocol,
        Protocol::Tcp
    );
    assert_eq!(
        args(80, Some("tcp"), None, None, None).unwrap().protocol,
        Protocol::Tcp
    );
    assert_eq!(
        args(80, Some("HTTP"), None, None, None).unwrap().protocol,
        Protocol::Http
    );
    for bad in ["udp", "https", "", "tcp ", "http://", "ftp"] {
        assert_eq!(
            code(args(80, Some(bad), None, None, None)),
            ErrorCode::InvalidProtocol,
            "{bad:?}"
        );
    }
}

#[test]
fn timeout_defaults_to_30_and_is_1_to_120() {
    assert_eq!(
        args(80, None, None, None, None).unwrap().timeout,
        Duration::from_secs(30)
    );
    for good in [1, 120] {
        assert_eq!(
            args(80, None, Some(good), None, None).unwrap().timeout,
            Duration::from_secs(good as u64)
        );
    }
    for bad in [0, -1, 121, 3600, i64::MIN] {
        assert_eq!(
            code(args(80, None, Some(bad), None, None)),
            ErrorCode::InvalidTimeout,
            "{bad}"
        );
    }
}

#[test]
fn http_has_a_default_path_and_status_and_status_is_100_to_599() {
    let r = args(80, Some("http"), None, None, None).unwrap();
    let q = r.query();
    assert_eq!((r.expected, q.protocol), (Some(200), Protocol::Http));
    let res = Readiness::failure("vm", ErrorCode::NoSuchVm, "x".into());
    assert!(res.path.is_none());
    for bad in [0, 99, 600, 1000, -200] {
        assert_eq!(
            code(args(80, Some("http"), None, None, Some(bad))),
            ErrorCode::InvalidStatus,
            "{bad}"
        );
    }
    for good in [100, 204, 301, 404, 599] {
        let r = args(80, Some("http"), None, None, Some(good)).unwrap();
        assert_eq!(r.expected, Some(good as u16));
    }
}

#[test]
fn path_and_status_are_refused_for_tcp() {
    assert_eq!(
        code(args(80, None, None, Some("/x"), None)),
        ErrorCode::InvalidArgument
    );
    assert_eq!(
        code(args(80, Some("tcp"), None, None, Some(200))),
        ErrorCode::InvalidArgument
    );
}

#[test]
fn a_bad_path_is_refused_with_a_structured_error() {
    for bad in [
        "health",
        "http://evil.example/",
        "//evil.example/x",
        "/a\r\nHost: evil",
        "/a\nb",
        "/a\0b",
        "/a\tb",
        "/a\u{7f}b",
        "/a\u{85}b",
        "/a\\b",
        "/a#frag",
        "",
        " /a",
        "evil.example:80/x",
        "@evil/x",
    ] {
        assert_eq!(
            code(args(80, Some("http"), None, Some(bad), None)),
            ErrorCode::InvalidPath,
            "{bad:?}"
        );
    }
    let long = format!("/{}", "a".repeat(MAX_PATH_CHARS));
    assert_eq!(
        code(args(80, Some("http"), None, Some(&long), None)),
        ErrorCode::InvalidPath
    );
    // A path under the character cap that escapes past the byte cap is refused too.
    let wide = format!("/{}", "é".repeat(400));
    assert_eq!(
        code(args(80, Some("http"), None, Some(&wide), None)),
        ErrorCode::InvalidPath
    );
}

#[test]
fn paths_are_escaped_in_a_single_pass() {
    let e = |p: &str| encode_path(p).unwrap();
    assert_eq!(e("/"), "/");
    assert_eq!(e("/health"), "/health");
    assert_eq!(e("/a/b-c_d.e~f"), "/a/b-c_d.e~f");
    assert_eq!(e("/a b"), "/a%20b");
    assert_eq!(e("/café"), "/caf%C3%A9");
    assert_eq!(e("/日本語/ü"), "/%E6%97%A5%E6%9C%AC%E8%AA%9E/%C3%BC");
    assert_eq!(e("/q?a=b&c=d+e,f;g:h@i"), "/q?a=b&c=d+e,f;g:h@i");
    // Already-escaped text is not escaped again; a stray % is.
    assert_eq!(e("/a%20b"), "/a%20b");
    assert_eq!(e("/a%2fb"), "/a%2fb");
    assert_eq!(e("/a%2541"), "/a%2541");
    assert_eq!(e("/100%"), "/100%25");
    assert_eq!(e("/50%zz"), "/50%25zz");
    assert_eq!(e("/%é"), "/%25%C3%A9");
    assert_eq!(e("/a%2"), "/a%252");
    // The characters a shell reads are escaped.
    assert_eq!(e("/a'b"), "/a%27b");
    assert_eq!(e("/a\"b"), "/a%22b");
    assert_eq!(e("/$(id)"), "/%24%28id%29");
    assert_eq!(e("/`id`"), "/%60id%60");
    assert_eq!(
        e("/a<b>|c^d{e}[f]!*"),
        "/a%3Cb%3E%7Cc%5Ed%7Be%7D%5Bf%5D%21%2A"
    );
    // A scheme or host after the first character is only text in a query or a segment.
    assert_eq!(e("/x?u=http://a/b"), "/x?u=http://a/b");
}

#[test]
fn escaping_is_idempotent_and_stays_in_a_shell_inert_alphabet() {
    let samples = [
        "/",
        "/a b",
        "/café ü",
        "/'; touch PWNED; '",
        "/$(touch PWNED)",
        "/`touch PWNED`",
        "/\"$x\"",
        "/a%20b%zz%",
        "/%25%2525",
        "/q?x=1&y=2",
        "/emoji-\u{1F600}",
        "/\u{202e}rtl",
        "/\u{2028}sep",
    ];
    for s in samples {
        let once = encode_path(s).unwrap_or_else(|e| panic!("{s:?}: {e}"));
        assert!(
            once.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._~/?=&:@+,;%".contains(&b)),
            "{s:?} -> {once:?}"
        );
        assert_eq!(encode_path(&once).unwrap(), once, "{s:?} must be stable");
    }
}

// =============================================================================================
// The guest report

fn parse(text: &str, p: Protocol) -> Result<GuestProbe, String> {
    parse_probe(text.as_bytes(), p)
}

#[test]
fn probe_reports_parse_strictly() {
    let ok = |c, h| GuestProbe {
        connect: c,
        http: h,
    };
    assert_eq!(
        parse(
            "AGENTPC_READY connect open\nAGENTPC_READY end ok\n",
            Protocol::Tcp
        ),
        Ok(ok(Connect::Open, None))
    );
    for (word, c) in [
        ("refused", Connect::Refused),
        ("timeout", Connect::TimedOut),
        ("failed", Connect::Failed),
        ("unsupported", Connect::Unsupported),
    ] {
        assert_eq!(
            parse(
                &format!("AGENTPC_READY connect {word}\nAGENTPC_READY end ok\n"),
                Protocol::Http
            ),
            Ok(ok(c, None)),
            "{word}"
        );
    }
    let http = |v: &str| {
        parse(
            &format!(
                "junk first\nAGENTPC_READY connect open\nAGENTPC_READY http {v}\nAGENTPC_READY end ok\n"
            ),
            Protocol::Http,
        )
    };
    assert_eq!(
        http("204"),
        Ok(ok(Connect::Open, Some(HttpReply::Status(204))))
    );
    assert_eq!(
        http("none"),
        Ok(ok(Connect::Open, Some(HttpReply::NoResponse)))
    );
    assert_eq!(
        http("malformed"),
        Ok(ok(Connect::Open, Some(HttpReply::Malformed)))
    );
    for bad in [
        "99", "600", "2000", "20", "abc", "2 0", "+200", "-20", "", "0200", "00200", "0x20",
    ] {
        assert!(http(bad).is_err(), "{bad:?}");
    }
    // Missing or doubled pieces, a cut report, and an http line required for http.
    assert!(parse("AGENTPC_READY connect open\n", Protocol::Tcp).is_err());
    assert!(parse("AGENTPC_READY end ok\n", Protocol::Tcp).is_err());
    assert!(
        parse(
            "AGENTPC_READY connect bogus\nAGENTPC_READY end ok\n",
            Protocol::Tcp
        )
        .is_err()
    );
    assert!(
        parse(
            "AGENTPC_READY connect open\nAGENTPC_READY connect open\nAGENTPC_READY end ok\n",
            Protocol::Tcp
        )
        .is_err()
    );
    assert!(
        parse(
            "AGENTPC_READY connect open\nAGENTPC_READY end ok\n",
            Protocol::Http
        )
        .is_err()
    );
    assert!(parse("", Protocol::Tcp).is_err());
    // The error never repeats guest text.
    let e = parse(
        "AGENTPC_READY connect SECRETWORD\nAGENTPC_READY end ok\n",
        Protocol::Tcp,
    )
    .unwrap_err();
    assert!(!e.contains("SECRETWORD"), "{e}");
}

// =============================================================================================
// The polling loop, with scripted probes

fn pacing() -> Pacing {
    Pacing {
        poll: Duration::from_millis(5),
        attempt_cap: Duration::from_millis(50),
        min_attempt: Duration::from_millis(10),
        transport: Duration::from_millis(20),
        grace: Duration::from_millis(10),
    }
}

fn tcp_req(ms: u64) -> Request {
    args(8080, None, None, None, None)
        .unwrap()
        .with_timeout(Duration::from_millis(ms))
}

fn http_req(ms: u64, expected: Option<i64>) -> Request {
    args(8080, Some("http"), None, Some("/health"), expected)
        .unwrap()
        .with_timeout(Duration::from_millis(ms))
}

fn conn(c: Connect) -> Attempt {
    Attempt {
        probe: Ok(GuestProbe {
            connect: c,
            http: None,
        }),
        job: None,
    }
}

fn reply(h: HttpReply) -> Attempt {
    Attempt {
        probe: Ok(GuestProbe {
            connect: Connect::Open,
            http: Some(h),
        }),
        job: None,
    }
}

fn unreachable() -> Attempt {
    Attempt {
        probe: Err(ProbeFailure::unreachable("ssh could not reach the VM")),
        job: None,
    }
}

/// Wait with a script of attempts: attempt n (1-based) is `script(n)`.
async fn run(req: &Request, mut script: impl FnMut(u32) -> Attempt) -> (Readiness, Vec<Duration>) {
    let budgets = Arc::new(Mutex::new(Vec::new()));
    let b = budgets.clone();
    let mut n = 0;
    let r = wait(
        req,
        pacing(),
        |_q, budget| {
            n += 1;
            b.lock().unwrap().push(budget);
            let a = script(n);
            async move { a }
        },
        std::future::pending(),
    )
    .await;
    let budgets = budgets.lock().unwrap().clone();
    (r, budgets)
}

#[tokio::test]
async fn tcp_becomes_ready_when_the_port_starts_accepting() {
    let (r, _) = run(&tcp_req(2000), |n| {
        conn(if n < 3 {
            Connect::Refused
        } else {
            Connect::Open
        })
    })
    .await;
    assert_eq!(r.state, ReadyState::Ready);
    assert_eq!((r.attempts, r.tcp), (3, Some(TcpObservation::Accepted)));
    assert!(r.criterion_met && !r.timed_out && !r.cancelled);
    assert!(r.error.is_none() && r.error_code.is_none());
    assert_eq!(
        (r.protocol, r.guest_port),
        (Some(Protocol::Tcp), Some(8080))
    );
    assert!(r.path.is_none() && r.expected_status.is_none() && r.http_outcome.is_none());
    let note = r.note.unwrap();
    assert!(
        note.contains("proves only") && note.contains("not a process"),
        "{note}"
    );
    assert!(r.criterion.unwrap().contains("accepts a TCP connection"));
}

#[tokio::test]
async fn tcp_refusal_expires_with_the_last_observation_kept() {
    let started = Instant::now();
    let (r, _) = run(&tcp_req(150), |_| conn(Connect::Refused)).await;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(r.state, ReadyState::TimedOut);
    assert!(r.timed_out && !r.cancelled && !r.criterion_met);
    assert_eq!(r.tcp, Some(TcpObservation::Refused));
    assert_eq!(r.error_code, Some(ErrorCode::Timeout));
    assert!(r.attempts >= 2, "{}", r.attempts);
    assert!(r.waited_ms >= 150, "{}", r.waited_ms);
    let e = r.error.unwrap();
    assert!(e.contains("Nothing was stopped"), "{e}");
}

#[tokio::test]
async fn tcp_accept_timeouts_and_other_failures_are_not_ready() {
    for (c, want) in [
        (Connect::TimedOut, TcpObservation::TimedOut),
        (Connect::Failed, TcpObservation::Failed),
    ] {
        let (r, _) = run(&tcp_req(60), |_| conn(c)).await;
        assert_eq!((r.state, r.tcp), (ReadyState::TimedOut, Some(want)));
    }
}

#[tokio::test]
async fn http_status_is_compared_to_the_expected_one() {
    use HttpOutcome::*;
    let cases = [
        (None, HttpReply::Status(200), Hit(StatusMatched, 200)),
        (None, HttpReply::Status(204), Miss(StatusMismatched, 204)),
        (None, HttpReply::Status(404), Miss(StatusMismatched, 404)),
        (None, HttpReply::Status(503), Miss(StatusMismatched, 503)),
        (None, HttpReply::Status(301), Miss(Redirect, 301)),
        (None, HttpReply::Status(307), Miss(Redirect, 307)),
        (Some(404), HttpReply::Status(404), Hit(StatusMatched, 404)),
        // A redirect is ready only when it is what was asked for; it is never followed.
        (Some(301), HttpReply::Status(301), Hit(StatusMatched, 301)),
        (Some(301), HttpReply::Status(302), Miss(Redirect, 302)),
        (Some(200), HttpReply::Status(301), Miss(Redirect, 301)),
    ];
    enum Want {
        Hit(HttpOutcome, u16),
        Miss(HttpOutcome, u16),
    }
    use Want::*;
    for (expected, got, want) in cases {
        let (r, _) = run(&http_req(60, expected), |_| reply(got)).await;
        match want {
            Hit(o, s) => {
                assert_eq!(r.state, ReadyState::Ready, "{expected:?} {got:?}");
                assert_eq!((r.http_outcome, r.http_status), (Some(o), Some(s)));
                assert!(r.criterion_met && r.attempts == 1);
            }
            Miss(o, s) => {
                assert_eq!(r.state, ReadyState::TimedOut, "{expected:?} {got:?}");
                assert_eq!((r.http_outcome, r.http_status), (Some(o), Some(s)));
                assert!(!r.criterion_met);
            }
        }
        assert_eq!(r.tcp, Some(TcpObservation::Accepted));
        assert_eq!(r.path.as_deref(), Some("/health"));
        assert_eq!(r.expected_status, Some(expected.unwrap_or(200) as u16));
    }
    for (got, want) in [
        (HttpReply::NoResponse, NoResponse),
        (HttpReply::Malformed, Malformed),
    ] {
        let (r, _) = run(&http_req(60, None), |_| reply(got)).await;
        assert_eq!(r.state, ReadyState::TimedOut);
        assert_eq!((r.http_outcome, r.http_status), (Some(want), None));
    }
    // A connection that is not accepted has no HTTP observation.
    let (r, _) = run(&http_req(60, None), |_| conn(Connect::Refused)).await;
    assert_eq!(
        (r.tcp, r.http_outcome),
        (Some(TcpObservation::Refused), None)
    );
    // http becomes ready once the status matches.
    let (r, _) = run(&http_req(2000, None), |n| match n {
        1 => conn(Connect::Refused),
        2 => reply(HttpReply::Status(503)),
        _ => reply(HttpReply::Status(200)),
    })
    .await;
    assert_eq!((r.state, r.attempts), (ReadyState::Ready, 3));
    let note = r.note.unwrap();
    assert!(
        note.contains("HTTP 200") && note.contains("only that status criterion"),
        "{note}"
    );
}

#[tokio::test]
async fn attempts_are_paced_and_their_time_is_capped() {
    let (r, budgets) = run(&tcp_req(300), |_| conn(Connect::Refused)).await;
    let p = pacing();
    assert!(
        budgets
            .iter()
            .all(|b| *b >= p.min_attempt && *b <= p.attempt_cap),
        "{budgets:?}"
    );
    // The first attempt gets the cap (much of the wait is left), the last about what is left.
    assert_eq!(budgets[0], p.attempt_cap);
    // Polling pauses between attempts: 300 ms at a 5 ms poll is far fewer than a busy loop.
    assert!((10..=70).contains(&r.attempts), "{}", r.attempts);
}

#[tokio::test]
async fn no_probe_starts_at_or_after_the_deadline() {
    let req = tcp_req(120);
    let started = Instant::now();
    let starts = Arc::new(Mutex::new(Vec::new()));
    let st = starts.clone();
    let r = wait(
        &req,
        Pacing {
            poll: Duration::from_millis(50),
            ..pacing()
        },
        move |_, _| {
            st.lock().unwrap().push(started.elapsed());
            async { conn(Connect::Refused) }
        },
        std::future::pending(),
    )
    .await;
    assert_eq!(r.state, ReadyState::TimedOut);
    for t in starts.lock().unwrap().iter() {
        assert!(*t < Duration::from_millis(120), "a probe started at {t:?}");
    }
}

#[tokio::test]
async fn an_attempt_that_hangs_is_abandoned_and_the_wait_still_ends() {
    // Nothing was ever observed: the VM is unreachable, which is not "not ready".
    let started = Instant::now();
    let r = wait(
        &tcp_req(150),
        pacing(),
        |_, _| std::future::pending::<Attempt>(),
        std::future::pending(),
    )
    .await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(r.state, ReadyState::Error);
    assert_eq!(r.error_code, Some(ErrorCode::GuestUnreachable));
    assert!(r.tcp.is_none() && r.attempts >= 1 && !r.timed_out);
    // After a real observation, later hangs leave it in place.
    let mut n = 0;
    let r = wait(
        &tcp_req(200),
        pacing(),
        |_, _| {
            n += 1;
            let first = n == 1;
            async move {
                if first {
                    conn(Connect::Refused)
                } else {
                    std::future::pending::<Attempt>().await
                }
            }
        },
        std::future::pending(),
    )
    .await;
    assert_eq!(
        (r.state, r.tcp),
        (ReadyState::TimedOut, Some(TcpObservation::Refused))
    );
    assert!(r.note.unwrap().contains("earlier one"));
}

#[tokio::test]
async fn failures_do_not_stop_the_wait_and_never_become_a_guess() {
    // The VM answers late: unreachable at first, then ready.
    let (r, _) = run(&tcp_req(2000), |n| {
        if n < 3 {
            unreachable()
        } else {
            conn(Connect::Open)
        }
    })
    .await;
    assert_eq!((r.state, r.attempts), (ReadyState::Ready, 3));
    assert!(r.note.unwrap().contains("proves only"));
    // Never reachable: error, with a fixed phrase and no state invented.
    let (r, _) = run(&tcp_req(60), |_| unreachable()).await;
    assert_eq!(
        (r.state, r.error_code),
        (ReadyState::Error, Some(ErrorCode::GuestUnreachable))
    );
    assert!(r.tcp.is_none() && !r.criterion_met);
    assert!(r.error.unwrap().contains("ssh could not reach the VM"));
    // A guest that keeps answering gibberish.
    let (r, _) = run(&tcp_req(60), |_| Attempt {
        probe: Err(ProbeFailure::bad_reply("the probe script exited 7")),
        job: None,
    })
    .await;
    assert_eq!(
        (r.state, r.error_code),
        (ReadyState::Error, Some(ErrorCode::BadGuestReply))
    );
}

#[tokio::test]
async fn a_guest_that_cannot_probe_ends_the_wait_at_once() {
    let started = Instant::now();
    let (r, _) = run(&tcp_req(5000), |_| conn(Connect::Unsupported)).await;
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!((r.state, r.attempts), (ReadyState::Error, 1));
    assert_eq!(r.error_code, Some(ErrorCode::ProbeUnsupported));
    assert_eq!(r.tcp, Some(TcpObservation::Unsupported));
}

// ---- cancellation ----

struct DropFlag(Arc<AtomicBool>);
impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn a_request_cancelled_before_it_starts_probes_nothing() {
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let r = wait(
        &tcp_req(5000),
        pacing(),
        move |_, _| {
            c.fetch_add(1, Ordering::SeqCst);
            async { conn(Connect::Open) }
        },
        std::future::ready(()),
    )
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!((r.state, r.attempts), (ReadyState::Cancelled, 0));
    assert!(r.cancelled && !r.timed_out && !r.criterion_met);
    assert_eq!(r.error_code, Some(ErrorCode::RequestCancelled));
    assert!(r.error.unwrap().contains("not touched"));
}

#[tokio::test]
async fn cancelling_mid_attempt_drops_the_probe_and_keeps_the_last_observation() {
    let dropped = Arc::new(AtomicBool::new(false));
    let d = dropped.clone();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let mut n = 0;
    let req = tcp_req(5000);
    let waiting = wait(
        &req,
        pacing(),
        move |_, _| {
            n += 1;
            let first = n == 1;
            let flag = (!first).then(|| DropFlag(d.clone()));
            async move {
                if first {
                    conn(Connect::Refused)
                } else {
                    let _flag = flag;
                    std::future::pending::<Attempt>().await
                }
            }
        },
        async {
            let _ = rx.await;
        },
    );
    let (r, _) = tokio::join!(waiting, async {
        tokio::time::sleep(Duration::from_millis(80)).await;
        let _ = tx.send(());
    });
    assert_eq!((r.state, r.attempts), (ReadyState::Cancelled, 2));
    assert!(r.cancelled);
    assert_eq!(
        r.tcp,
        Some(TcpObservation::Refused),
        "the last observation survives"
    );
    assert!(
        dropped.load(Ordering::SeqCst),
        "the probe in flight was dropped"
    );
}

#[tokio::test]
async fn cancellation_is_not_starved_by_probes_that_always_answer_at_once() {
    let started = Instant::now();
    let r = wait(
        &tcp_req(5000),
        Pacing {
            poll: Duration::ZERO,
            ..pacing()
        },
        |_, _| async { conn(Connect::Refused) },
        tokio::time::sleep(Duration::from_millis(30)),
    )
    .await;
    assert_eq!(r.state, ReadyState::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn dropping_the_wait_drops_the_probe_and_nothing_probes_again() {
    let dropped = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let (d, c) = (dropped.clone(), calls.clone());
    let req = tcp_req(5000);
    let fut = wait(
        &req,
        pacing(),
        move |_, _| {
            c.fetch_add(1, Ordering::SeqCst);
            let flag = DropFlag(d.clone());
            async move {
                let _flag = flag;
                std::future::pending::<Attempt>().await
            }
        },
        std::future::pending(),
    );
    // The caller gives up after 40 ms (the handler's future is dropped).
    assert!(
        tokio::time::timeout(Duration::from_millis(40), fut)
            .await
            .is_err()
    );
    assert!(dropped.load(Ordering::SeqCst));
    let seen = calls.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        seen,
        "no probe after the drop"
    );
}

#[tokio::test]
async fn attempt_runs_probe_and_job_together_and_drops_both() {
    let (d1, d2) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let (f1, f2) = (DropFlag(d1.clone()), DropFlag(d2.clone()));
    let fut = attempt(
        async move {
            let _f = f1;
            std::future::pending::<Result<GuestProbe, ProbeFailure>>().await
        },
        Some(async move {
            let _f = f2;
            std::future::pending::<jobs::JobStatus>().await
        }),
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(30), fut)
            .await
            .is_err()
    );
    assert!(d1.load(Ordering::SeqCst) && d2.load(Ordering::SeqCst));
    let a = attempt(
        async {
            Ok(GuestProbe {
                connect: Connect::Open,
                http: None,
            })
        },
        None::<std::future::Ready<jobs::JobStatus>>,
    )
    .await;
    assert!(a.probe.is_ok() && a.job.is_none());
}

// ---- the optional job ----

fn job(state: &str, exit: Option<i64>, stop: Option<&str>) -> jobs::JobStatus {
    serde_json::from_value(serde_json::json!({
        "schema_version": 1,
        "state": state,
        "job_id": 7,
        "kind": "background",
        "exit_code": exit,
        "stop_reason": stop,
        "log_available": false,
        "output_bytes": 0,
        "output_truncated": false,
        "invalid_utf8": false,
        "has_more": false,
        "cursor_reset": false,
        "wait_timed_out": false,
        "waited_ms": 0,
    }))
    .unwrap()
}

fn with_job(id: u64, ms: u64) -> Request {
    Request::new("vm", 8080, None, None, None, None, Some(id))
        .unwrap()
        .with_timeout(Duration::from_millis(ms))
}

fn refused_with(j: jobs::JobStatus) -> Attempt {
    Attempt {
        probe: Ok(GuestProbe {
            connect: Connect::Refused,
            http: None,
        }),
        job: Some(j),
    }
}

#[tokio::test]
async fn a_job_that_failed_ends_the_wait_early() {
    for (j, what) in [
        (job("exited", Some(3), None), "exited with code 3"),
        (job("exited", Some(-1), None), "exited with code -1"),
        (job("stopped", None, Some("stop_job")), "was stopped"),
        (
            job("stopped", None, Some("request_cancelled")),
            "was stopped",
        ),
        (job("not_started", None, None), "never started"),
    ] {
        let started = Instant::now();
        let (r, _) = run(&with_job(7, 5000), |_| refused_with(j.clone())).await;
        assert!(started.elapsed() < Duration::from_secs(1), "{what}");
        assert_eq!((r.state, r.attempts), (ReadyState::JobFailed, 1), "{what}");
        assert_eq!(r.error_code, Some(ErrorCode::JobFailed));
        assert!(!r.timed_out && !r.criterion_met);
        assert_eq!(r.tcp, Some(TcpObservation::Refused));
        let e = r.error.unwrap();
        assert!(
            e.contains(what) && e.contains("Nothing was stopped or restarted"),
            "{e}"
        );
        assert!(r.note.unwrap().contains("not a process"));
        let jo = r.job.unwrap();
        assert!(jo.observed && jo.job_id == 7);
    }
}

#[tokio::test]
async fn a_job_failure_before_any_endpoint_check_does_not_claim_a_check() {
    // The job failed while every probe errored: nothing is known about the endpoint.
    let (r, _) = run(&with_job(7, 5000), |_| Attempt {
        probe: Err(ProbeFailure::unreachable("ssh could not reach the VM")),
        job: Some(job("exited", Some(3), None)),
    })
    .await;
    assert_eq!(r.state, ReadyState::JobFailed);
    assert!(r.tcp.is_none());
    let e = r.error.unwrap();
    assert!(!e.contains("did not meet the criterion"), "{e}");
    assert!(e.contains("No check of the endpoint"), "{e}");
    assert!(e.contains("exited with code 3"), "{e}");
    // With an observation, the words say what it was.
    let (r, _) = run(&with_job(7, 5000), |_| {
        refused_with(job("exited", Some(3), None))
    })
    .await;
    let e = r.error.unwrap();
    assert!(
        e.contains("did not meet the criterion at the last check"),
        "{e}"
    );
    assert!(!e.contains("No check of the endpoint"), "{e}");
}

/// An older observation is never described as the latest check of the endpoint: when the
/// newest attempt did not complete, job_failed and cancelled say so too (not just timed_out).
#[tokio::test]
async fn job_failed_after_an_incomplete_latest_attempt_keeps_the_older_observation_warning() {
    // Attempt 1 saw the port refused; attempt 2's probe failed while the job was seen failing.
    let (r, _) = run(&with_job(7, 5000), |n| {
        if n == 1 {
            refused_with(job("running", None, None))
        } else {
            Attempt {
                probe: Err(ProbeFailure::unreachable("ssh could not reach the VM")),
                job: Some(job("exited", Some(3), None)),
            }
        }
    })
    .await;
    assert_eq!((r.state, r.attempts), (ReadyState::JobFailed, 2));
    assert_eq!(r.tcp, Some(TcpObservation::Refused));
    let e = r.error.unwrap();
    assert!(
        !e.contains("did not meet the criterion at the last check"),
        "an older observation called the last check: {e}"
    );
    assert!(
        e.contains("earlier check") && e.contains("latest check did not complete"),
        "{e}"
    );
    let note = r.note.unwrap();
    assert!(note.contains("not a process"), "{note}");
    assert!(
        note.contains("did not complete") && note.contains("earlier one"),
        "{note}"
    );
    // A job failure after a completed check keeps the plain wording and no warning.
    let (r, _) = run(&with_job(7, 5000), |_| {
        refused_with(job("exited", Some(3), None))
    })
    .await;
    assert!(!r.note.unwrap().contains("earlier one"));
}

#[tokio::test]
async fn cancelled_after_an_incomplete_latest_attempt_keeps_the_older_observation_warning() {
    // (a) cancelled while the second attempt was still running (it never completes).
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let mut n = 0;
    let req = tcp_req(5000);
    let waiting = wait(
        &req,
        pacing(),
        move |_, _| {
            n += 1;
            let first = n == 1;
            async move {
                if first {
                    conn(Connect::Refused)
                } else {
                    std::future::pending::<Attempt>().await
                }
            }
        },
        async {
            let _ = rx.await;
        },
    );
    let (r, _) = tokio::join!(waiting, async {
        tokio::time::sleep(Duration::from_millis(80)).await;
        let _ = tx.send(());
    });
    assert_eq!((r.state, r.attempts), (ReadyState::Cancelled, 2));
    assert_eq!(r.tcp, Some(TcpObservation::Refused));
    let note = r
        .note
        .expect("a cancelled wait with a stale observation says so");
    assert!(
        note.contains("did not complete") && note.contains("earlier one"),
        "{note}"
    );
    // (b) cancelled after the latest attempt failed outright.
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let mut n = 0;
    let waiting = wait(
        &req,
        pacing(),
        move |_, _| {
            n += 1;
            let a = if n == 1 {
                conn(Connect::Refused)
            } else {
                unreachable()
            };
            async move { a }
        },
        async {
            let _ = rx.await;
        },
    );
    let (r, _) = tokio::join!(waiting, async {
        tokio::time::sleep(Duration::from_millis(120)).await;
        let _ = tx.send(());
    });
    assert_eq!(r.state, ReadyState::Cancelled);
    assert_eq!(r.tcp, Some(TcpObservation::Refused));
    let note = r.note.expect("stale observation after a failed attempt");
    assert!(note.contains("earlier one"), "{note}");
    // (c) cancelled between polls, after a completed check: the observation is the latest one,
    // so there is nothing to warn about.
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let waiting = wait(
        &req,
        Pacing {
            poll: Duration::from_secs(5),
            ..pacing()
        },
        |_, _| async { conn(Connect::Refused) },
        async {
            let _ = rx.await;
        },
    );
    let (r, _) = tokio::join!(waiting, async {
        tokio::time::sleep(Duration::from_millis(60)).await;
        let _ = tx.send(());
    });
    assert_eq!((r.state, r.attempts), (ReadyState::Cancelled, 1));
    assert!(
        r.note.as_deref().is_none_or(|n| !n.contains("earlier one")),
        "{:?}",
        r.note
    );
}

/// A failed attempt followed by one still running at cancellation: the reason given is the
/// running one's situation, never the earlier attempt's failure.
#[tokio::test]
async fn a_cancel_during_a_later_attempt_does_not_blame_the_earlier_failure() {
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let mut n = 0;
    let req = tcp_req(5000);
    let waiting = wait(
        &req,
        pacing(),
        move |_, _| {
            n += 1;
            let k = n;
            async move {
                match k {
                    1 => conn(Connect::Refused),
                    2 => Attempt {
                        probe: Err(ProbeFailure::unreachable("EARLIER-ATTEMPT-REASON")),
                        job: None,
                    },
                    _ => std::future::pending::<Attempt>().await,
                }
            }
        },
        async {
            let _ = rx.await;
        },
    );
    let (r, _) = tokio::join!(waiting, async {
        // Past attempts 1 and 2, inside attempt 3 (abandoned only after its window).
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = tx.send(());
    });
    assert_eq!(r.state, ReadyState::Cancelled);
    assert!(r.attempts >= 3, "attempts: {}", r.attempts);
    assert_eq!(r.tcp, Some(TcpObservation::Refused));
    let note = r.note.expect("a stale observation is flagged");
    assert!(note.contains("earlier one"), "{note}");
    assert!(
        !note.contains("EARLIER-ATTEMPT-REASON"),
        "an earlier attempt's failure was given as the cause: {note}"
    );
    assert!(note.contains("was running"), "{note}");
}

/// probe_unsupported covers a guest bash with no /dev/tcp and one with no GNU `timeout`.
#[tokio::test]
async fn probe_unsupported_names_both_missing_tcp_support_and_missing_timeout() {
    let (r, _) = run(&tcp_req(5000), |_| conn(Connect::Unsupported)).await;
    assert_eq!(r.error_code, Some(ErrorCode::ProbeUnsupported));
    let e = r.error.unwrap();
    assert!(e.contains("/dev/tcp"), "{e}");
    assert!(e.contains("timeout"), "{e}");
    assert!(e.contains("run_command"), "{e}");
}

#[tokio::test]
async fn states_that_prove_nothing_do_not_end_the_wait() {
    for j in [
        job("exited", Some(0), None),
        job("queued", None, None),
        job("running", None, None),
        job("ended", None, None),
        job("missing", None, None),
        job("unknown", None, None),
        jobs::JobStatus::failure(7, jobs::JobErrorCode::GuestUnreachable, "no".into()),
    ] {
        let state = j.state;
        let (r, _) = run(&with_job(7, 120), |_| refused_with(j.clone())).await;
        assert_eq!(r.state, ReadyState::TimedOut, "{state:?}");
        assert!(r.attempts >= 2, "{state:?}");
    }
}

#[tokio::test]
async fn a_zero_exit_with_a_surviving_server_is_ready() {
    let (r, _) = run(&with_job(7, 2000), |n| Attempt {
        probe: Ok(GuestProbe {
            connect: if n < 2 {
                Connect::Refused
            } else {
                Connect::Open
            },
            http: None,
        }),
        job: Some(job("exited", Some(0), None)),
    })
    .await;
    assert_eq!((r.state, r.attempts), (ReadyState::Ready, 2));
    let jo = r.job.unwrap();
    assert_eq!(
        (jo.state, jo.exit_code, jo.observed),
        (jobs::JobState::Exited, Some(0), true)
    );
}

#[tokio::test]
async fn a_zero_exit_with_nothing_listening_says_why_it_kept_waiting() {
    let (r, _) = run(&with_job(7, 80), |_| {
        refused_with(job("exited", Some(0), None))
    })
    .await;
    assert_eq!(r.state, ReadyState::TimedOut);
    assert!(r.note.unwrap().contains("exited with code 0"));
}

#[tokio::test]
async fn an_answering_endpoint_is_ready_even_if_its_job_failed() {
    // Readiness is about the endpoint; it says so rather than tying it to the job.
    let (r, _) = run(&with_job(7, 2000), |_| Attempt {
        probe: Ok(GuestProbe {
            connect: Connect::Open,
            http: None,
        }),
        job: Some(job("exited", Some(2), None)),
    })
    .await;
    assert_eq!(r.state, ReadyState::Ready);
    let note = r.note.unwrap();
    assert!(
        note.contains("exited with code 2") && note.contains("something else"),
        "{note}"
    );
}

#[tokio::test]
async fn a_job_read_that_fails_keeps_the_last_good_reading() {
    let (r, _) = run(&with_job(7, 80), |n| {
        refused_with(if n == 1 {
            job("running", None, None)
        } else {
            jobs::JobStatus::failure(7, jobs::JobErrorCode::GuestUnreachable, "no".into())
        })
    })
    .await;
    let jo = r.job.unwrap();
    assert!(jo.observed);
    assert_eq!(jo.state, jobs::JobState::Running);
    // No job_id: no job in the result.
    let (r, _) = run(&tcp_req(40), |_| conn(Connect::Refused)).await;
    assert!(r.job.is_none());
}

// ---- the result ----

#[tokio::test]
async fn results_are_one_text_block_with_a_typed_body_and_errors_are_tool_errors() {
    let (ready, _) = run(&tcp_req(500), |_| conn(Connect::Open)).await;
    let (late, _) = run(&tcp_req(40), |_| conn(Connect::Refused)).await;
    let refused = args(0, None, None, None, None).unwrap_err();
    for (r, is_err) in [(ready, false), (late, true), (*refused, true)] {
        let state = r.state;
        let res = r.clone().into_result();
        assert_eq!(res.is_error, Some(is_err), "{state:?}");
        assert_eq!(res.content.len(), 1);
        let text = res.content[0].as_text().unwrap().text.clone();
        assert!(!text.trim_start().starts_with('{'), "{text}");
        assert!(text.contains("VM vm"), "{text}");
        let back: Readiness = serde_json::from_value(res.structured_content.unwrap()).unwrap();
        assert_eq!(back, r);
        assert_eq!(back.schema_version, 1);
        assert_eq!(is_err, back.error.is_some() && back.error_code.is_some());
    }
}

#[test]
fn the_schema_has_the_documented_fields_and_nothing_that_could_carry_guest_text() {
    let schema = serde_json::Value::Object((*output_schema()).clone());
    let props = schema["properties"].as_object().unwrap();
    for f in [
        "schema_version",
        "state",
        "name",
        "protocol",
        "guest_port",
        "path",
        "expected_status",
        "criterion",
        "criterion_met",
        "tcp",
        "http_outcome",
        "http_status",
        "attempts",
        "waited_ms",
        "timeout_seconds",
        "timed_out",
        "cancelled",
        "job",
        "note",
        "error",
        "error_code",
    ] {
        assert!(props.contains_key(f), "{f}");
    }
    for banned in [
        "body", "headers", "stderr", "stdout", "log_tail", "output", "response",
    ] {
        assert!(!props.contains_key(banned), "{banned} in the schema");
    }
    let states = schema["properties"]["state"].to_string();
    for s in ["ready", "timed_out", "job_failed", "cancelled", "error"] {
        assert!(states.contains(s), "{s}: {states}");
    }
}

/// The declared output schema documents itself: the module doc is no leftover stub note, every
/// field has a description, every variant of the state-like enums has its own, and so does every
/// field of the job observation. (schemars takes these from the types' doc comments.)
#[test]
fn the_output_schema_describes_every_field_and_variant() {
    let source = include_str!("../src/readiness.rs");
    assert!(
        !source.contains("STUB") && !source.contains("test-first rebuild"),
        "a leftover stub note in the module doc"
    );
    let schema = serde_json::Value::Object((*output_schema()).clone());
    let described = |v: &serde_json::Value| {
        v["description"]
            .as_str()
            .is_some_and(|d| d.trim().len() >= 12)
    };
    let props = schema["properties"].as_object().unwrap();
    for (name, p) in props {
        assert!(described(p), "field {name} has no description: {p}");
    }
    for field in ["state", "tcp", "http_outcome", "error_code", "protocol"] {
        let node = &props[field];
        let variants = node
            .get("oneOf")
            .or_else(|| node.get("anyOf").and_then(|a| a[0].get("oneOf")))
            .and_then(|v| v.as_array())
            .unwrap_or_else(|| panic!("{field}: variants are not individually described: {node}"));
        assert!(variants.len() >= 2, "{field}");
        for v in variants {
            assert!(described(v), "{field} variant without a description: {v}");
        }
    }
    let job = &schema["$defs"]["JobObservation"];
    for (name, p) in job["properties"].as_object().unwrap() {
        assert!(described(p), "job.{name} has no description: {p}");
    }
}

// =============================================================================================
// Local servers

#[derive(Clone)]
struct Seen {
    heads: Arc<Mutex<Vec<String>>>,
    connections: Arc<AtomicUsize>,
}

struct Server {
    port: u16,
    seen: Seen,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    /// Serve on `listener`; `handler` gets each connection's request head and the stream.
    fn on(
        listener: TcpListener,
        handler: impl Fn(usize, &str, &mut TcpStream) + Send + Sync + 'static,
    ) -> Server {
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let seen = Seen {
            heads: Arc::default(),
            connections: Arc::default(),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let handler = Arc::new(handler);
        let (s2, stop2) = (seen.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !stop2.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let n = s2.connections.fetch_add(1, Ordering::SeqCst) + 1;
                        let (h, s3) = (handler.clone(), s2.clone());
                        std::thread::spawn(move || {
                            stream.set_nonblocking(false).unwrap();
                            stream
                                .set_read_timeout(Some(Duration::from_millis(1500)))
                                .unwrap();
                            let mut head = Vec::new();
                            let mut buf = [0u8; 1024];
                            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                                match stream.read(&mut buf) {
                                    Ok(0) | Err(_) => break,
                                    Ok(k) => head.extend_from_slice(&buf[..k]),
                                }
                            }
                            let head = String::from_utf8_lossy(&head).into_owned();
                            if !head.is_empty() {
                                s3.heads.lock().unwrap().push(head.clone());
                            }
                            h(n, &head, &mut stream);
                        });
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        Server {
            port,
            seen,
            stop,
            thread: Some(thread),
        }
    }

    fn start(handler: impl Fn(usize, &str, &mut TcpStream) + Send + Sync + 'static) -> Server {
        Self::on(TcpListener::bind("127.0.0.1:0").unwrap(), handler)
    }

    /// A TCP listener that only accepts.
    fn tcp() -> Server {
        Self::start(|_, _, _| {})
    }

    fn heads(&self) -> Vec<String> {
        self.seen.heads.lock().unwrap().clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn send(stream: &mut TcpStream, raw: &str) {
    let _ = stream.write_all(raw.as_bytes());
    let _ = stream.flush();
}

/// A port nothing listens on.
fn closed_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

// =============================================================================================
// The Linux probe, run for real under local bash

fn have_gnu_timeout() -> bool {
    Command::new("timeout")
        .arg("--version")
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains("GNU"))
        .unwrap_or(false)
}

fn query(req: &Request) -> Query {
    req.query()
}

struct Ran {
    stdout: String,
    stderr: String,
    elapsed: Duration,
}

fn run_linux(q: &Query, budget_secs: u64, env: &[(&str, &str)]) -> Option<Ran> {
    if !have_gnu_timeout() {
        eprintln!("no GNU timeout here; the bash fixtures are skipped");
        return None;
    }
    let script = linux_probe_script(q, Duration::from_secs(budget_secs));
    let started = Instant::now();
    let mut cmd = Command::new("bash");
    cmd.args(["-c", &script]);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(Ran {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        elapsed: started.elapsed(),
    })
}

fn linux(req: &Request, budget_secs: u64) -> Option<(GuestProbe, Ran)> {
    let ran = run_linux(&query(req), budget_secs, &[])?;
    let p = parse_probe(ran.stdout.as_bytes(), req.protocol)
        .unwrap_or_else(|e| panic!("{e}: {:?}", ran.stdout));
    Some((p, ran))
}

fn tcp_to(port: u16) -> Request {
    args(port as i64, None, None, None, None).unwrap()
}

fn http_to(port: u16, path: &str) -> Request {
    args(port as i64, Some("http"), None, Some(path), None).unwrap()
}

#[test]
fn the_generated_bash_probe_parses() {
    for req in [tcp_to(80), http_to(80, "/a b?x=1&y='\"$(id)")] {
        let script = linux_probe_script(&query(&req), Duration::from_secs(3));
        let out = Command::new("bash")
            .args(["-n", "-c", &script])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}\n{script}",
            String::from_utf8_lossy(&out.stderr)
        );
        // Only the loopback is named; nothing that could talk to a proxy or another host.
        assert!(
            script.contains("127.0.0.1") && !script.contains("curl") && !script.contains("wget")
        );
        assert!(!script.contains("proxy"));
    }
}

#[test]
fn bash_tcp_accepts_and_refuses() {
    let server = Server::tcp();
    let Some((p, ran)) = linux(&tcp_to(server.port), 2) else {
        return;
    };
    assert_eq!(
        p,
        GuestProbe {
            connect: Connect::Open,
            http: None
        }
    );
    assert_eq!(ran.stderr, "");
    let Some((p, ran)) = linux(&tcp_to(closed_port()), 2) else {
        return;
    };
    assert_eq!(
        p,
        GuestProbe {
            connect: Connect::Refused,
            http: None
        }
    );
    assert_eq!(ran.stderr, "");
    assert!(ran.elapsed < Duration::from_secs(2));
}

#[test]
fn bash_http_reports_the_status_and_nothing_else() {
    for (reply_line, want) in [
        ("HTTP/1.1 200 OK", 200),
        ("HTTP/1.0 404 Not Found", 404),
        ("HTTP/1.1 301 Moved Permanently", 301),
        ("HTTP/1.1 503", 503),
        ("HTTP/1.1 204 No Content", 204),
    ] {
        let server = Server::start(move |_, _, s| {
            send(
                s,
                &format!(
                    "{reply_line}\r\nLocation: http://127.0.0.1:1/SECRETLOCATION\r\n\
                     X-Secret: SECRETHEADER\r\nContent-Length: 12\r\n\r\nSECRETBODY!!"
                ),
            );
        });
        let Some((p, ran)) = linux(&http_to(server.port, "/health"), 3) else {
            return;
        };
        assert_eq!(
            p,
            GuestProbe {
                connect: Connect::Open,
                http: Some(HttpReply::Status(want))
            },
            "{reply_line}"
        );
        assert_eq!(server.heads().len(), 1, "one request, no redirect followed");
        assert!(!ran.stdout.contains("SECRET") && !ran.stderr.contains("SECRET"));
        assert_eq!(ran.stderr, "");
    }
}

#[test]
fn bash_http_does_not_follow_redirects() {
    let target = Server::tcp();
    let tp = target.port;
    let server = Server::start(move |_, _, s| {
        send(
            s,
            &format!("HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{tp}/elsewhere\r\n\r\n"),
        );
    });
    let Some((p, _)) = linux(&http_to(server.port, "/go"), 3) else {
        return;
    };
    assert_eq!(p.http, Some(HttpReply::Status(302)));
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(target.seen.connections.load(Ordering::SeqCst), 0);
    assert_eq!(server.heads().len(), 1);
}

#[test]
fn bash_http_sends_one_plain_get_with_no_credentials() {
    let server = Server::start(|_, _, s| send(s, "HTTP/1.1 200 OK\r\n\r\n"));
    let Some(_) = linux(&http_to(server.port, "/health?x=1"), 3) else {
        return;
    };
    let head = server.heads().remove(0);
    let lines: Vec<&str> = head.split("\r\n").collect();
    assert_eq!(lines[0], "GET /health?x=1 HTTP/1.1");
    assert_eq!(lines[1], format!("Host: 127.0.0.1:{}", server.port));
    let lower = head.to_ascii_lowercase();
    for banned in ["authorization", "cookie", "proxy-", "x-api", "token"] {
        assert!(!lower.contains(banned), "{banned}: {head}");
    }
    assert!(head.ends_with("\r\n\r\n"));
}

#[test]
fn bash_http_failures_are_classified_without_repeating_the_server() {
    // Not HTTP at all.
    let server = Server::start(|_, _, s| send(s, "SECRETGARBAGE not http\r\n\r\n"));
    let Some((p, ran)) = linux(&http_to(server.port, "/"), 3) else {
        return;
    };
    assert_eq!(p.http, Some(HttpReply::Malformed));
    assert!(!ran.stdout.contains("SECRET"));
    // A status outside 100-599, a bad version, a long first line.
    for bad in [
        "HTTP/1.1 999 X",
        "HTTP/1.1 20 X",
        "HTTP/x.y 200 OK",
        "http/1.1 200 OK",
    ] {
        let server = Server::start(move |_, _, s| send(s, &format!("{bad}\r\n\r\n")));
        let Some((p, _)) = linux(&http_to(server.port, "/"), 3) else {
            return;
        };
        assert_eq!(p.http, Some(HttpReply::Malformed), "{bad}");
    }
    let server =
        Server::start(|_, _, s| send(s, &format!("HTTP/1.1 200 {}\r\n\r\n", "x".repeat(5000))));
    let Some((p, _)) = linux(&http_to(server.port, "/"), 3) else {
        return;
    };
    assert_eq!(
        p.http,
        Some(HttpReply::Status(200)),
        "the first 256 bytes hold the status"
    );
    let server =
        Server::start(|_, _, s| send(s, &format!("{} HTTP/1.1 200 OK\r\n\r\n", "x".repeat(5000))));
    let Some((p, _)) = linux(&http_to(server.port, "/"), 3) else {
        return;
    };
    assert_eq!(p.http, Some(HttpReply::Malformed));
    // Closed without a reply.
    let server = Server::start(|_, _, _| {});
    let Some((p, _)) = linux(&http_to(server.port, "/"), 3) else {
        return;
    };
    assert_eq!(
        p,
        GuestProbe {
            connect: Connect::Open,
            http: Some(HttpReply::NoResponse)
        }
    );
}

#[test]
fn bash_http_waits_for_a_slow_reply_within_its_budget_and_no_longer() {
    let slow = Server::start(|_, _, s| {
        std::thread::sleep(Duration::from_millis(600));
        send(s, "HTTP/1.1 200 OK\r\n\r\n");
    });
    let Some((p, _)) = linux(&http_to(slow.port, "/"), 3) else {
        return;
    };
    assert_eq!(p.http, Some(HttpReply::Status(200)));
    let hung = Server::start(|_, _, s| {
        std::thread::sleep(Duration::from_secs(4));
        send(s, "HTTP/1.1 200 OK\r\n\r\n");
    });
    let Some((p, ran)) = linux(&http_to(hung.port, "/"), 1) else {
        return;
    };
    assert_eq!(p.http, Some(HttpReply::NoResponse));
    assert!(
        ran.elapsed < Duration::from_millis(2500),
        "{:?}",
        ran.elapsed
    );
}

#[test]
fn bash_http_reads_only_the_status_line_of_a_huge_reply() {
    let server = Server::start(|_, _, s| {
        send(s, "HTTP/1.1 200 OK\r\n");
        for _ in 0..4096 {
            send(s, &format!("X-Pad: {}\r\n", "p".repeat(512)));
        }
        send(s, "\r\n");
        let body = "SECRETBODY".repeat(200_000);
        send(s, &body);
    });
    let Some((p, ran)) = linux(&http_to(server.port, "/big"), 3) else {
        return;
    };
    assert_eq!(p.http, Some(HttpReply::Status(200)));
    assert!(ran.stdout.len() < 200, "{}", ran.stdout.len());
    assert!(!ran.stdout.contains("SECRET") && !ran.stdout.contains("X-Pad"));
}

#[test]
fn bash_http_escapes_unicode_and_cannot_be_injected_through_the_path() {
    let server = Server::start(|_, _, s| send(s, "HTTP/1.1 200 OK\r\n\r\n"));
    let dir = std::env::temp_dir().join(format!("agentpc-readiness-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let evil = [
        "/caf\u{e9}/\u{65e5}\u{672c}?q=\u{1F600}",
        "/a'; touch PWNED; echo '",
        "/$(touch PWNED)",
        "/`touch PWNED`",
        "/\"; touch PWNED; \"",
        "/a b%20c%zz",
    ];
    for path in evil {
        let req = http_to(server.port, path);
        let q = query(&req);
        if !have_gnu_timeout() {
            eprintln!("skipped");
            return;
        }
        let script = linux_probe_script(&q, Duration::from_secs(3));
        let out = Command::new("bash")
            .args(["-c", &script])
            .current_dir(&dir)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let p = parse_probe(text.as_bytes(), Protocol::Http).unwrap();
        assert_eq!(p.http, Some(HttpReply::Status(200)), "{path:?}: {text}");
        let sent = server.heads().pop().unwrap();
        let want = format!("GET {} HTTP/1.1", encode_path(path).unwrap());
        assert_eq!(sent.lines().next().unwrap(), want, "{path:?}");
    }
    assert!(!dir.join("PWNED").exists(), "a path reached a shell");
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        0,
        "the probe wrote a file"
    );
    let _ = std::fs::remove_dir_all(&dir);
    // Spot-check the escaped forms the server saw.
    let heads = server.heads();
    assert!(heads[0].starts_with("GET /caf%C3%A9/%E6%97%A5%E6%9C%AC?q=%F0%9F%98%80 HTTP/1.1"));
    assert!(
        heads[5].starts_with("GET /a%20b%20c%25zz HTTP/1.1"),
        "{}",
        heads[5]
    );
}

#[test]
fn bash_probe_ignores_proxy_settings_and_stays_on_the_loopback() {
    let decoy = Server::tcp();
    let proxy = format!("http://127.0.0.1:{}", decoy.port);
    let server = Server::start(|_, _, s| send(s, "HTTP/1.1 200 OK\r\n\r\n"));
    let env = [
        ("http_proxy", proxy.as_str()),
        ("HTTP_PROXY", proxy.as_str()),
        ("https_proxy", proxy.as_str()),
        ("ALL_PROXY", proxy.as_str()),
        ("no_proxy", ""),
    ];
    let q = query(&http_to(server.port, "/p"));
    let Some(ran) = run_linux(&q, 3, &env) else {
        return;
    };
    let p = parse_probe(ran.stdout.as_bytes(), Protocol::Http).unwrap();
    assert_eq!(p.http, Some(HttpReply::Status(200)));
    assert_eq!(server.heads().len(), 1);
    assert_eq!(decoy.seen.connections.load(Ordering::SeqCst), 0);
}

#[test]
fn bash_probe_maps_a_timeout_stub_to_connect_timeout() {
    if !have_gnu_timeout() {
        return;
    }
    // A `timeout` that always reports its command timed out, ahead of the real one.
    let dir = std::env::temp_dir().join(format!("agentpc-readiness-stub-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("timeout"), "#!/bin/sh\nexit 124\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir.join("timeout"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", dir.display(), std::env::var("PATH").unwrap());
    let q = query(&tcp_to(closed_port()));
    let ran = run_linux(&q, 1, &[("PATH", &path)]).unwrap();
    assert_eq!(
        parse_probe(ran.stdout.as_bytes(), Protocol::Tcp)
            .unwrap()
            .connect,
        Connect::TimedOut
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A directory of stand-ins for guest tools, put first on PATH.
fn stub_dir(tag: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("agentpc-readiness-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (name, body) in files {
        std::fs::write(dir.join(name), body).unwrap();
        std::fs::set_permissions(dir.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

#[test]
fn bash_probe_reports_a_missing_timeout_command_as_unsupported_at_once() {
    if !have_gnu_timeout() {
        return;
    }
    // A shell that cannot find `timeout` exits 127: the guest cannot make this probe, which is
    // a fixed fact, not a refusal to retry until the deadline.
    let stub = stub_dir("notimeout", &[("timeout", "#!/bin/sh\nexit 127\n")]);
    let path = format!("{}:{}", stub.display(), std::env::var("PATH").unwrap());
    for req in [tcp_to(closed_port()), http_to(closed_port(), "/")] {
        let started = Instant::now();
        let ran = run_linux(&query(&req), 3, &[("PATH", &path)]).unwrap();
        assert_eq!(
            parse_probe(ran.stdout.as_bytes(), req.protocol)
                .unwrap()
                .connect,
            Connect::Unsupported,
            "{}",
            ran.stdout
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }
    let _ = std::fs::remove_dir_all(&stub);
    // And with no `timeout` on PATH at all (only a bash to run).
    let bash = String::from_utf8(
        Command::new("sh")
            .args(["-c", "command -v bash"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let only = stub_dir("onlybash", &[]);
    std::os::unix::fs::symlink(bash.trim(), only.join("bash")).unwrap();
    let ran = run_linux(
        &query(&tcp_to(closed_port())),
        3,
        &[("PATH", only.to_str().unwrap())],
    )
    .unwrap();
    assert_eq!(
        parse_probe(ran.stdout.as_bytes(), Protocol::Tcp)
            .unwrap()
            .connect,
        Connect::Unsupported,
        "{}",
        ran.stdout
    );
    let _ = std::fs::remove_dir_all(&only);
}

// =============================================================================================
// The Windows probe, run under pwsh when this machine has it

fn have_pwsh() -> bool {
    Command::new("pwsh")
        .args(["-NoProfile", "-Command", "exit 0"])
        .output()
        .is_ok()
}

fn run_windows(req: &Request, budget_secs: u64, env: &[(&str, &str)]) -> Option<Ran> {
    if !have_pwsh() {
        eprintln!("no pwsh here; the PowerShell fixtures are skipped");
        return None;
    }
    let script = windows_probe_script(&query(req), Duration::from_secs(budget_secs));
    let started = Instant::now();
    let mut cmd = Command::new("pwsh");
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    Some(Ran {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        elapsed: started.elapsed(),
    })
}

fn windows(req: &Request, budget_secs: u64) -> Option<(GuestProbe, Ran)> {
    let ran = run_windows(req, budget_secs, &[])?;
    let p = parse_probe(ran.stdout.as_bytes(), req.protocol)
        .unwrap_or_else(|e| panic!("{e}: {:?} {:?}", ran.stdout, ran.stderr));
    Some((p, ran))
}

#[test]
fn the_generated_powershell_probe_parses() {
    if !have_pwsh() {
        eprintln!("no pwsh here; skipped");
        return;
    }
    let parse = "$e = $null; $t = $null; \
        [void][System.Management.Automation.Language.Parser]::ParseInput($env:AGENTPC_PS, [ref]$t, [ref]$e); \
        if ($e.Count) { $e | ForEach-Object { $_.ToString() }; exit 1 }";
    for req in [tcp_to(80), http_to(80, "/a b?x=1&y='\"$(id)")] {
        let script = windows_probe_script(&query(&req), Duration::from_secs(3));
        let out = Command::new("pwsh")
            .args(["-NoProfile", "-NonInteractive", "-Command", parse])
            .env("AGENTPC_PS", &script)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}\n{script}",
            String::from_utf8_lossy(&out.stdout)
        );
        assert!(!script.contains("Invoke-WebRequest") && !script.contains("Proxy"));
    }
}

#[test]
fn powershell_tcp_accepts_and_refuses() {
    let server = Server::tcp();
    let Some((p, ran)) = windows(&tcp_to(server.port), 3) else {
        return;
    };
    assert_eq!(
        p,
        GuestProbe {
            connect: Connect::Open,
            http: None
        }
    );
    assert_eq!(ran.stderr, "");
    let Some((p, ran)) = windows(&tcp_to(closed_port()), 3) else {
        return;
    };
    assert_eq!(
        p,
        GuestProbe {
            connect: Connect::Refused,
            http: None
        }
    );
    assert_eq!(ran.stderr, "");
}

#[test]
fn powershell_http_reports_status_only_and_sends_a_plain_get() {
    let target = Server::tcp();
    let tp = target.port;
    for (line, want) in [
        ("HTTP/1.1 200 OK", 200),
        ("HTTP/1.1 404 Nope", 404),
        ("HTTP/1.1 302 Found", 302),
    ] {
        let server = Server::start(move |_, _, s| {
            send(
                s,
                &format!(
                    "{line}\r\nLocation: http://127.0.0.1:{tp}/x\r\nX-Secret: SECRETHEADER\r\n\
                     Content-Length: 10\r\n\r\nSECRETBODY"
                ),
            );
        });
        let Some((p, ran)) = windows(&http_to(server.port, "/h?x=\u{e9}"), 3) else {
            return;
        };
        assert_eq!(p.http, Some(HttpReply::Status(want)), "{line}");
        assert!(!ran.stdout.contains("SECRET") && !ran.stderr.contains("SECRET"));
        let head = server.heads().remove(0);
        let lines: Vec<&str> = head.split("\r\n").collect();
        assert_eq!(lines[0], "GET /h?x=%C3%A9 HTTP/1.1");
        assert_eq!(lines[1], format!("Host: 127.0.0.1:{}", server.port));
        assert!(!head.to_ascii_lowercase().contains("authorization"));
    }
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        target.seen.connections.load(Ordering::SeqCst),
        0,
        "no redirect followed"
    );
}

#[test]
fn powershell_http_failures_and_proxy_settings() {
    let server = Server::start(|_, _, s| send(s, "SECRETGARBAGE\r\n\r\n"));
    let Some((p, ran)) = windows(&http_to(server.port, "/"), 3) else {
        return;
    };
    assert_eq!(p.http, Some(HttpReply::Malformed));
    assert!(!ran.stdout.contains("SECRET"));
    let silent = Server::start(|_, _, _| {});
    let Some((p, _)) = windows(&http_to(silent.port, "/"), 3) else {
        return;
    };
    assert_eq!(p.http, Some(HttpReply::NoResponse));
    let hung = Server::start(|_, _, _| std::thread::sleep(Duration::from_secs(4)));
    let Some((p, ran)) = windows(&http_to(hung.port, "/"), 1) else {
        return;
    };
    assert_eq!(p.http, Some(HttpReply::NoResponse));
    assert!(
        ran.elapsed < Duration::from_millis(4000),
        "{:?}",
        ran.elapsed
    );
    // Proxy settings do not reroute it.
    let decoy = Server::tcp();
    let proxy = format!("http://127.0.0.1:{}", decoy.port);
    let ok = Server::start(|_, _, s| send(s, "HTTP/1.1 200 OK\r\n\r\n"));
    let Some(ran) = run_windows(
        &http_to(ok.port, "/p"),
        3,
        &[
            ("http_proxy", &proxy),
            ("HTTP_PROXY", &proxy),
            ("ALL_PROXY", &proxy),
        ],
    ) else {
        return;
    };
    let p = parse_probe(ran.stdout.as_bytes(), Protocol::Http).unwrap();
    assert_eq!(p.http, Some(HttpReply::Status(200)));
    assert_eq!(decoy.seen.connections.load(Ordering::SeqCst), 0);
}

#[test]
fn powershell_unicode_path_is_sent_escaped() {
    let server = Server::start(|_, _, s| send(s, "HTTP/1.1 200 OK\r\n\r\n"));
    let Some(_) = windows(&http_to(server.port, "/caf\u{e9}/'$(x)`y\""), 3) else {
        return;
    };
    let head = server.heads().remove(0);
    assert_eq!(
        head.lines().next().unwrap(),
        "GET /caf%C3%A9/%27%24%28x%29%60y%22 HTTP/1.1"
    );
}

// =============================================================================================
// The whole wait, against real servers through the real scripts

/// One attempt through the generated bash probe, as the tool runs it over SSH.
async fn local_attempt(q: Query, budget: Duration) -> Attempt {
    let script = linux_probe_script(&q, budget);
    let out = tokio::process::Command::new("bash")
        .args(["-c", &script])
        .kill_on_drop(true)
        .output()
        .await;
    let probe = match out {
        Ok(o) if o.status.success() => {
            parse_probe(&o.stdout, q.protocol).map_err(|e| ProbeFailure::bad_reply(&e))
        }
        _ => Err(ProbeFailure::unreachable("could not run the probe")),
    };
    Attempt { probe, job: None }
}

fn real_pacing() -> Pacing {
    Pacing {
        poll: Duration::from_millis(100),
        attempt_cap: Duration::from_secs(2),
        min_attempt: Duration::from_secs(1),
        transport: Duration::from_secs(2),
        grace: Duration::from_millis(500),
    }
}

#[tokio::test]
async fn wait_sees_a_listener_that_starts_late() {
    if !have_gnu_timeout() {
        return;
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let starter = tokio::task::spawn_blocking(move || {
        std::thread::sleep(Duration::from_millis(900));
        Server::on(
            TcpListener::bind(("127.0.0.1", port)).unwrap(),
            |_, _, _| {},
        )
    });
    let req = tcp_to(port).with_timeout(Duration::from_secs(8));
    let r = wait(
        &req,
        real_pacing(),
        |q, b| local_attempt(q.clone(), b),
        std::future::pending(),
    )
    .await;
    assert_eq!(r.state, ReadyState::Ready, "{r:?}");
    assert!(
        r.attempts >= 2 && r.waited_ms >= 800,
        "{} {}",
        r.attempts,
        r.waited_ms
    );
    drop(starter.await.unwrap());
}

#[tokio::test]
async fn wait_times_out_on_a_closed_port_and_keeps_the_refusal() {
    if !have_gnu_timeout() {
        return;
    }
    let req = tcp_to(closed_port()).with_timeout(Duration::from_millis(1200));
    let started = Instant::now();
    let r = wait(
        &req,
        real_pacing(),
        |q, b| local_attempt(q.clone(), b),
        std::future::pending(),
    )
    .await;
    assert_eq!(
        (r.state, r.tcp),
        (ReadyState::TimedOut, Some(TcpObservation::Refused)),
        "{r:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(6));
}

#[tokio::test]
async fn wait_polls_http_until_the_status_matches_one_request_per_attempt() {
    if !have_gnu_timeout() {
        return;
    }
    let server = Server::start(|n, _, s| {
        send(
            s,
            if n < 3 {
                "HTTP/1.1 503 Busy\r\n\r\n"
            } else {
                "HTTP/1.1 200 OK\r\n\r\n"
            },
        );
    });
    let req = http_to(server.port, "/ready").with_timeout(Duration::from_secs(10));
    let r = wait(
        &req,
        real_pacing(),
        |q, b| local_attempt(q.clone(), b),
        std::future::pending(),
    )
    .await;
    assert_eq!((r.state, r.attempts), (ReadyState::Ready, 3), "{r:?}");
    assert_eq!(
        (r.http_outcome, r.http_status),
        (Some(HttpOutcome::StatusMatched), Some(200))
    );
    assert_eq!(server.heads().len(), 3);
}

#[tokio::test]
async fn wait_on_a_redirecting_endpoint_reports_the_redirect_and_never_follows_it() {
    if !have_gnu_timeout() {
        return;
    }
    let other = Server::tcp();
    let op = other.port;
    let server = Server::start(move |_, _, s| {
        send(
            s,
            &format!("HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{op}/\r\n\r\n"),
        );
    });
    let req = http_to(server.port, "/").with_timeout(Duration::from_millis(1200));
    let r = wait(
        &req,
        real_pacing(),
        |q, b| local_attempt(q.clone(), b),
        std::future::pending(),
    )
    .await;
    assert_eq!(r.state, ReadyState::TimedOut);
    assert_eq!(
        (r.http_outcome, r.http_status),
        (Some(HttpOutcome::Redirect), Some(302))
    );
    assert_eq!(server.heads().len() as u32, r.attempts);
    assert_eq!(other.seen.connections.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn wait_cancelled_mid_probe_returns_quickly_and_leaves_the_server_alone() {
    if !have_gnu_timeout() {
        return;
    }
    let hung = Server::start(|_, _, _| std::thread::sleep(Duration::from_secs(4)));
    let req = http_to(hung.port, "/").with_timeout(Duration::from_secs(30));
    let started = Instant::now();
    let r = wait(
        &req,
        real_pacing(),
        |q, b| local_attempt(q.clone(), b),
        tokio::time::sleep(Duration::from_millis(400)),
    )
    .await;
    assert_eq!((r.state, r.attempts), (ReadyState::Cancelled, 1), "{r:?}");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(r.cancelled);
    // The server was probed once and is still serving: waiting changed nothing.
    assert_eq!(hung.heads().len(), 1);
}

// =============================================================================================
// The job read that a wait makes asks for no log bytes

/// A PATH with GNU coreutils first (on a Mac, Homebrew's), which the job status script needs.
fn gnu_path() -> Option<String> {
    let base = std::env::var("PATH").unwrap_or_default();
    for extra in [
        "",
        "/opt/homebrew/opt/coreutils/libexec/gnubin:",
        "/usr/local/opt/coreutils/libexec/gnubin:",
    ] {
        let path = format!("{extra}{base}");
        let ok = |tool: &str| {
            Command::new(tool)
                .arg("--version")
                .env("PATH", &path)
                .output()
                .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains("GNU"))
                .unwrap_or(false)
        };
        if ["stat", "sha256sum", "base64", "head", "tail", "tr"]
            .iter()
            .all(|t| ok(t))
        {
            return Some(path);
        }
    }
    None
}

#[test]
fn a_job_read_with_no_log_cap_returns_no_log_text() {
    let Some(gnu) = gnu_path() else {
        eprintln!("no GNU coreutils here; the job fixture is skipped");
        return;
    };
    let home = std::env::temp_dir().join(format!("agentpc-readiness-job-{}", std::process::id()));
    let bg = home.join("agentpc-bg");
    std::fs::create_dir_all(&bg).unwrap();
    std::fs::write(bg.join("7.log"), "SECRETLOGLINE one\nSECRETLOGLINE two\n").unwrap();
    std::fs::write(bg.join("7.exit"), "3\n").unwrap();
    std::fs::write(home.join("boot_id"), "boot-aaaa\n").unwrap();
    let mut req = jobs::Request::new("vm", 7, 0, Duration::ZERO, None, "gen".into());
    req.cap = 0;
    let paths = jobs::LinuxPaths {
        fd: bg.join("7.log").to_string_lossy().into_owned(),
        boot_id: home.join("boot_id").to_string_lossy().into_owned(),
    };
    let script = jobs::linux_script_with(7, &req.query(), &paths);
    let out = Command::new("bash")
        .args(["-c", &script])
        .env("HOME", &home)
        .env("PATH", &gnu)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(!text.contains("SECRET"), "{text}");
    let rep = jobs::parse_report(&out.stdout).unwrap_or_else(|e| panic!("{e}: {text}"));
    assert_eq!(
        (rep.state, rep.exit_code),
        (jobs::JobState::Exited, Some(3))
    );
    assert!(rep.data.is_empty());
    // Control: with the ordinary cap the same script does return the log's bytes (base64).
    let full = jobs::Request::new("vm", 7, 50, Duration::ZERO, None, "gen".into());
    let script = jobs::linux_script_with(7, &full.query(), &paths);
    let out = Command::new("bash")
        .args(["-c", &script])
        .env("HOME", &home)
        .env("PATH", &gnu)
        .output()
        .unwrap();
    let rep = jobs::parse_report(&out.stdout).unwrap();
    assert!(String::from_utf8_lossy(&rep.data).contains("SECRETLOGLINE"));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_windows_job_read_with_no_log_cap_returns_no_log_text() {
    if !have_pwsh() {
        eprintln!("no pwsh here; skipped");
        return;
    }
    // Off Windows a backslash is part of a file name: the profile's records are files named
    // `<profile>\agentpc-bg\<id>.<ext>` next to the profile directory.
    let base = std::env::temp_dir().join(format!("agentpc-readiness-win-{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    let name = base.file_name().unwrap().to_str().unwrap().to_string();
    let rec = |ext: &str| {
        base.parent()
            .unwrap()
            .join(format!("{name}\\agentpc-bg\\7.{ext}"))
    };
    std::fs::write(rec("log"), "SECRETLOGLINE one\nSECRETLOGLINE two\n").unwrap();
    std::fs::write(rec("exit"), "3\n").unwrap();
    let mut req = jobs::Request::new("vm", 7, 0, Duration::ZERO, None, "gen".into());
    req.cap = 0;
    let script = format!(
        "function Get-ScheduledTask {{ [CmdletBinding()] param($TaskName) $null }}\n{}",
        jobs::windows_script(7, &req.query())
    );
    let out = Command::new("pwsh")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .env("USERPROFILE", &base)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    // Control: with the ordinary cap the same script does return the log's bytes (base64).
    let full = jobs::Request::new("vm", 7, 50, Duration::ZERO, None, "gen".into());
    let script = format!(
        "function Get-ScheduledTask {{ [CmdletBinding()] param($TaskName) $null }}\n{}",
        jobs::windows_script(7, &full.query())
    );
    let ctl = Command::new("pwsh")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .env("USERPROFILE", &base)
        .output()
        .unwrap();
    let ctl = jobs::parse_report(&ctl.stdout);
    let _ = std::fs::remove_file(rec("log"));
    let _ = std::fs::remove_file(rec("exit"));
    let _ = std::fs::remove_dir_all(&base);
    assert!(!text.contains("SECRET"), "{text}");
    let rep = jobs::parse_report(&out.stdout).unwrap_or_else(|e| panic!("{e}: {text}"));
    assert_eq!(
        (rep.state, rep.exit_code),
        (jobs::JobState::Exited, Some(3))
    );
    assert!(rep.data.is_empty());
    let ctl = ctl.unwrap();
    assert!(String::from_utf8_lossy(&ctl.data).contains("SECRETLOGLINE"));
}

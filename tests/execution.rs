//! Tests of tracked execution (src/execution.rs) that start real processes: sh and bash
//! standing in for ssh and the guest, and pwsh running the Windows wrapper when this machine
//! has it. They are a test binary of their own because cargo runs test binaries one after
//! another: a process spawned while a test in the main binary holds an flock-based lock can
//! keep that lock alive for a moment, which those lock tests would see as a failure.

#[allow(dead_code)]
#[path = "../src/execution.rs"]
mod execution;

use std::time::Duration;

use execution::*;
use rmcp::model::CallToolResult;

fn text(r: &CallToolResult) -> String {
    r.content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The generated job scripts parse: bash -n for Linux, and PowerShell's parser for Windows
/// when this machine has pwsh.
#[test]
fn job_scripts_parse() {
    for script in [linux_stop_script(42, false), linux_stop_script(42, true)] {
        let out = std::process::Command::new("bash")
            .args(["-n", "-c", &script])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let parse = "$e = $null; $t = $null; \
        [void][System.Management.Automation.Language.Parser]::ParseInput($env:AGENTPC_PS, [ref]$t, [ref]$e); \
        if ($e.Count) { $e | ForEach-Object { $_.ToString() }; exit 1 }";
    for script in [
        windows_stop_script(42, false),
        windows_stop_script(42, true),
        windows_probe(42),
        windows_foreground(42, "Write-Output hi"),
    ] {
        let Ok(out) = std::process::Command::new("pwsh")
            .args(["-NoProfile", "-NonInteractive", "-Command", parse])
            .env("AGENTPC_PS", &script)
            .output()
        else {
            eprintln!("no pwsh here; PowerShell parse skipped");
            return;
        };
        assert!(
            out.status.success(),
            "{}\n{script}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}

/// Runs the Windows wrapper for real under PowerShell, when this machine has `pwsh`
/// (PowerShell 7; the guests run 5.1, whose `-Command` exit rules these match). Its
/// USERPROFILE is a temp dir: off Windows, `\` is part of a file name, so the records
/// land next to it as `agentpc-bg\<id>.*`-named files.
#[test]
fn windows_wrapper_runs_under_powershell_when_available() {
    if std::process::Command::new("pwsh")
        .args(["-NoProfile", "-Command", "exit 0"])
        .output()
        .is_err()
    {
        eprintln!("no pwsh here; skipped");
        return;
    }
    let home = tempdir();
    let rec = |id: u64, ext: &str| {
        std::fs::read_to_string(home.parent().unwrap().join(format!(
            "{}\\agentpc-bg\\{id}.{ext}",
            home.file_name().unwrap().to_str().unwrap()
        )))
        .ok()
    };
    let run = |id: u64, script: &str| {
        let out = std::process::Command::new("pwsh")
            .args(["-NoProfile", "-NonInteractive", "-Command"])
            .arg(windows_foreground(id, script))
            .env("USERPROFILE", &*home)
            .output()
            .unwrap();
        let mut err = Stream::with_marker(start_marker(id));
        err.feed(&out.stderr);
        let (stderr, start) = err.finish(true);
        (
            out.status.code(),
            start,
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            stderr.text(),
            out,
        )
    };
    // Success: output clean of the marker, 0 recorded, identity recorded first.
    let (code, start, stdout, stderr, _) = run(101, "Write-Output 'hi'");
    assert_eq!(
        (code, start, stdout.as_str(), stderr.as_str()),
        (Some(0), Start::Seen, "hi", "")
    );
    assert_eq!(rec(101, "exit").as_deref(), Some("0"));
    let pid = rec(101, "pid").unwrap();
    let fields: Vec<&str> = pid.split(' ').collect();
    assert_eq!(fields.len(), 2, "{pid}");
    assert!(fields.iter().all(|f| f.parse::<u64>().is_ok()), "{pid}");
    assert!(rec(101, "fg").is_some());
    // An explicit exit keeps its code.
    let (code, start, ..) = run(102, "Write-Output x; exit 3");
    assert_eq!((code, start), (Some(3), Start::Seen));
    // A failed cmdlet as the last statement: 1, as with -Command.
    let (code, start, ..) = run(103, "Get-Item -LiteralPath /no/such/agentpc/path");
    assert_eq!((code, start), (Some(1), Start::Seen));
    assert_eq!(rec(103, "exit").as_deref(), Some("1"));
    // Cancelled before it started: no marker, no payload, the wrapper's code.
    std::fs::write(
        home.parent().unwrap().join(format!(
            "{}\\agentpc-bg\\104.cancel",
            home.file_name().unwrap().to_str().unwrap()
        )),
        "",
    )
    .unwrap();
    let (code, start, stdout, ..) = run(104, "Write-Output ran");
    assert_eq!(
        (code, start, stdout.as_str()),
        (Some(WRAPPER_CANCELLED), Start::Absent, "")
    );
    assert!(
        rec(104, "pid").is_some(),
        "the record comes before the check"
    );
    // The probe and the record-reading function parse and run too.
    let probe = std::process::Command::new("pwsh")
        .args(["-NoProfile", "-NonInteractive", "-Command"])
        .arg(windows_probe(101))
        .env("USERPROFILE", &*home)
        .output()
        .unwrap();
    assert_eq!(
        Probe::parse(&String::from_utf8_lossy(&probe.stdout)),
        Probe::Exited(0)
    );
    let probe = std::process::Command::new("pwsh")
        .args(["-NoProfile", "-NonInteractive", "-Command"])
        .arg(windows_probe(102))
        .env("USERPROFILE", &*home)
        .output()
        .unwrap();
    // 102 ended with an explicit `exit 3`; its Exiting event recorded the code.
    assert_eq!(
        Probe::parse(&String::from_utf8_lossy(&probe.stdout)),
        Probe::Exited(3)
    );
    let probe = std::process::Command::new("pwsh")
        .args(["-NoProfile", "-NonInteractive", "-Command"])
        .arg(windows_probe(105))
        .env("USERPROFILE", &*home)
        .output()
        .unwrap();
    assert_eq!(
        Probe::parse(&String::from_utf8_lossy(&probe.stdout)),
        Probe::NotStarted
    );
    // ... and that check made sure 105 never starts.
    let (code, start, stdout, ..) = run(105, "Write-Output ran");
    assert_eq!(
        (code, start, stdout.as_str()),
        (Some(WRAPPER_CANCELLED), Start::Absent, "")
    );

    let probe = |id: u64| {
        let out = std::process::Command::new("pwsh")
            .args(["-NoProfile", "-NonInteractive", "-Command"])
            .arg(windows_probe(id))
            .env("USERPROFILE", &*home)
            .output()
            .unwrap();
        Probe::parse(&String::from_utf8_lossy(&out.stdout))
    };
    // A running execution is recognised by its recorded pid and creation time...
    let mut live = std::process::Command::new("pwsh")
        .args(["-NoProfile", "-NonInteractive", "-Command"])
        .arg(windows_foreground(106, "Start-Sleep 60"))
        .env("USERPROFILE", &*home)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while rec(106, "pid").is_none_or(|r| r.split(' ').count() != 2) {
        assert!(std::time::Instant::now() < deadline, "no record");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(probe(106), Probe::Running);
    // ... and a record naming that same live pid with another creation time (a reused
    // pid) is not it.
    let pid = rec(106, "pid").unwrap();
    let (pid, born) = pid.split_once(' ').unwrap();
    let path = |id: u64, ext: &str| {
        home.parent().unwrap().join(format!(
            "{}\\agentpc-bg\\{id}.{ext}",
            home.file_name().unwrap().to_str().unwrap()
        ))
    };
    let forged: u64 = born.parse::<u64>().unwrap() - 10_000_000;
    std::fs::write(path(107, "fg"), "").unwrap();
    std::fs::write(path(107, "pid"), format!("{pid} {forged}")).unwrap();
    assert_eq!(probe(107), Probe::Gone);
    let _ = live.kill();
    let _ = live.wait();
    assert_eq!(probe(106), Probe::Gone);
}

/// The generated bash parses (bash -n), including the inner script it quotes.
#[test]
fn linux_scripts_parse() {
    let check = |script: &str| {
        let out = std::process::Command::new("bash")
            .args(["-n", "-c", script])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}\n{script}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    let l = linux_foreground(77, "echo \"$HOME\" 'q' && exit 3", "A=1");
    check(&l);
    let start = l.find("i='").unwrap() + 3;
    let inner = &l[start..start + l[start..].find('\'').unwrap()];
    check(inner);
    check(LINUX_GC);
    check(&linux_probe(77));
}

// -----------------------------------------------------------------------------------------
// The host-side run, against real local processes standing in for ssh.

fn sh(script: &str) -> tokio::process::Command {
    let mut c = tokio::process::Command::new("sh");
    c.args(["-c", script]);
    // Its own process group, so a test can clean up children that outlive it.
    c.process_group(0);
    c
}

fn marker_printf(id: u64) -> String {
    format!("printf '\\036agentpc-started:{id}\\n' >&2")
}

async fn never_cancel() -> Cancel {
    panic!("no cancellation expected")
}

async fn never_probe() -> Probe {
    panic!("no probe expected")
}

#[tokio::test]
async fn a_finished_command_reports_its_code_and_clean_streams() {
    let script = format!("{}; echo out; echo err >&2; exit 4", marker_printf(5));
    let f = run_foreground(
        sh(&script),
        5,
        Duration::from_secs(20),
        FAST,
        never_cancel,
        never_probe,
    )
    .await
    .unwrap();
    assert_eq!(
        (f.code, f.start, f.timed_out),
        (Some(4), Start::Seen, false)
    );
    assert_eq!(f.stdout.text(), "out");
    assert_eq!(f.stderr.text(), "err");
    let r = conclude(f, 5, "vm", 20);
    assert_eq!(r.outcome.exit_code, Some(4));
    assert_eq!(
        r.text,
        "exit code: 4\n--- stdout ---\nout\n--- stderr ---\nerr"
    );
}

#[tokio::test]
async fn no_marker_and_a_clean_exit_is_not_started() {
    let f = run_foreground(
        sh("echo setup failed >&2; exit 97"),
        5,
        Duration::from_secs(20),
        FAST,
        never_cancel,
        never_probe,
    )
    .await
    .unwrap();
    assert_eq!((f.code, f.start), (Some(97), Start::Absent));
    let o = conclude(f, 5, "vm", 20).outcome;
    assert_eq!(o.state, ExecutionState::NotStarted);
    assert_eq!(o.stderr, "setup failed");
}

/// A guest whose SSH server merged stderr into stdout: the marker is still found and
/// removed, and stdout carries only the payload's output.
#[tokio::test]
async fn the_marker_is_found_and_removed_on_stdout_too() {
    let script = format!("{} 2>&1; echo out", marker_printf(5));
    let f = run_foreground(
        sh(&script),
        5,
        Duration::from_secs(20),
        FAST,
        never_cancel,
        never_probe,
    )
    .await
    .unwrap();
    assert_eq!((f.code, f.start), (Some(0), Start::Seen));
    assert_eq!(f.stdout.text(), "out");
}

/// No marker and an exit code that isn't the wrapper's own: not trusted as "never
/// started" without the VM's record.
#[tokio::test]
async fn no_marker_and_another_code_asks_the_vm() {
    let f = run_foreground(
        sh("echo 'bash: base64: command not found' >&2; exit 127"),
        5,
        Duration::from_secs(20),
        FAST,
        never_cancel,
        || async { Probe::NotStarted },
    )
    .await
    .unwrap();
    assert_eq!(
        (f.start, f.probe.clone()),
        (Start::Absent, Some(Probe::NotStarted))
    );
    let o = conclude(f, 5, "vm", 20).outcome;
    assert_eq!(o.state, ExecutionState::NotStarted);
    assert!(!o.effect_unknown);
    let f = run_foreground(
        sh("exit 0"),
        5,
        Duration::from_secs(20),
        FAST,
        never_cancel,
        || async { Probe::Exited(0) },
    )
    .await
    .unwrap();
    let o = conclude(f, 5, "vm", 20).outcome;
    assert_eq!((o.state, o.exit_code), (ExecutionState::Exited, Some(0)));
}

#[tokio::test]
async fn ssh_failure_code_asks_the_vm() {
    let script = format!("{}; exit 255", marker_printf(5));
    let f = run_foreground(
        sh(&script),
        5,
        Duration::from_secs(20),
        FAST,
        never_cancel,
        || async { Probe::Running },
    )
    .await
    .unwrap();
    assert_eq!(f.probe, Some(Probe::Running));
    assert_eq!(conclude(f, 5, "vm", 20).outcome.job_id, Some(5));
}

/// A timeout cancels in the "guest" (here: kills the recorded pid) while output keeps
/// flowing, returns the partial output, and the session then ends by itself.
#[tokio::test]
async fn timeout_cancels_and_keeps_partial_output() {
    let dir = tempdir();
    let pidfile = dir.join("pid");
    let script = format!(
        "echo $$ > {}; {}; echo before; sleep 30; echo after",
        pidfile.display(),
        marker_printf(6)
    );
    let started = std::time::Instant::now();
    let pf = pidfile.clone();
    let f = run_foreground(
        sh(&script),
        6,
        Duration::from_millis(700),
        FAST,
        move || async move {
            let pid: i32 = std::fs::read_to_string(&pf)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            // The recorded shell and its child, as the guest's stop script would.
            let _ = std::process::Command::new("pkill")
                .args(["-TERM", "-g", &pid.to_string()])
                .status();
            Cancel::Stopped { exited: None }
        },
        never_probe,
    )
    .await
    .unwrap();
    assert!(f.timed_out);
    assert_eq!(f.start, Start::Seen);
    assert_eq!(f.cancel, Some(Cancel::Stopped { exited: None }));
    assert_eq!(f.stdout.text(), "before");
    assert!(started.elapsed() < Duration::from_secs(10));
    let r = conclude(f, 6, "vm", 1).into_result();
    assert_eq!(r.structured_content.as_ref().unwrap()["timed_out"], true);
    assert_eq!(
        r.structured_content.as_ref().unwrap()["termination_confirmed"],
        true
    );
    assert_eq!(r.is_error, Some(true));
    assert!(text(&r).contains("--- stdout ---\nbefore"));
}

/// When the guest can't confirm, the local session is still ended and the reads stay
/// bounded even though a leftover child keeps the pipes open.
#[tokio::test]
async fn unconfirmed_cancel_is_bounded_even_with_pipes_held_open() {
    let dir = tempdir();
    let pidfile = dir.join("pid");
    let script = format!(
        "echo $$ > {}; {}; (sleep 60) & echo partial; sleep 60",
        pidfile.display(),
        marker_printf(8)
    );
    let started = std::time::Instant::now();
    let f = run_foreground(
        sh(&script),
        8,
        Duration::from_millis(500),
        FAST,
        || async { Cancel::Unconfirmed("the VM did not answer".into()) },
        never_probe,
    )
    .await
    .unwrap();
    let elapsed = started.elapsed();
    // The orphaned sleeps still hold the pipes; clean them up (their process group).
    let pgid = std::fs::read_to_string(&pidfile).unwrap();
    let _ = std::process::Command::new("kill")
        .args(["-9", &format!("-{}", pgid.trim())])
        .status();
    assert!(
        elapsed < FAST.session_grace + FAST.drain_grace + Duration::from_secs(3),
        "{elapsed:?}"
    );
    assert!(f.timed_out);
    assert_eq!(f.stdout.text(), "partial");
    let r = conclude(f, 8, "vm", 1).into_result();
    assert_eq!(
        r.structured_content.as_ref().unwrap()["termination_confirmed"],
        false
    );
    assert_eq!(r.structured_content.as_ref().unwrap()["job_id"], 8);
    assert_eq!(r.is_error, Some(true));
}

#[tokio::test]
async fn a_cancel_that_hangs_is_cut_off_at_its_deadline() {
    let dir = tempdir();
    let pidfile = dir.join("pid");
    let script = format!(
        "echo $$ > {}; {}; sleep 60",
        pidfile.display(),
        marker_printf(9)
    );
    let started = std::time::Instant::now();
    let f = run_foreground(
        sh(&script),
        9,
        Duration::from_millis(300),
        FAST,
        std::future::pending::<Cancel>,
        never_probe,
    )
    .await
    .unwrap();
    let pgid = std::fs::read_to_string(&pidfile).unwrap();
    let _ = std::process::Command::new("kill")
        .args(["-9", &format!("-{}", pgid.trim())])
        .status();
    assert!(
        matches!(f.cancel, Some(Cancel::Unconfirmed(_))),
        "{:?}",
        f.cancel
    );
    assert!(started.elapsed() < Duration::from_secs(10));
}

/// Short bounds, so the tests of the bounded steps run fast.
const FAST: Limits = Limits {
    cancel: Duration::from_secs(1),
    probe: Duration::from_secs(1),
    session_grace: Duration::from_millis(500),
    drain_grace: Duration::from_millis(500),
};

/// A fresh temp dir, removed (with files named after it, see the PowerShell test) when
/// dropped.
struct TempDir(std::path::PathBuf);

impl std::ops::Deref for TempDir {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        &self.0
    }
}

impl AsRef<std::ffi::OsStr> for TempDir {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.0.as_os_str()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
        let name = self.0.file_name().unwrap().to_string_lossy().to_string();
        if let Ok(entries) = std::fs::read_dir(self.0.parent().unwrap()) {
            for e in entries.flatten() {
                if e.file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{name}\\"))
                {
                    let p = e.path();
                    let _ = std::fs::remove_file(&p).or_else(|_| std::fs::remove_dir_all(&p));
                }
            }
        }
    }
}

fn tempdir() -> TempDir {
    let d = std::env::temp_dir().join(format!(
        "agentpc-exec-test-{}-{}",
        std::process::id(),
        unique()
    ));
    std::fs::create_dir_all(&d).unwrap();
    TempDir(d)
}

fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::SeqCst)
}

fn have_pwsh() -> bool {
    let ok = std::process::Command::new("pwsh")
        .args(["-NoProfile", "-Command", "exit 0"])
        .output()
        .is_ok();
    if !ok {
        eprintln!("no pwsh here; skipped");
    }
    ok
}

/// Off Windows, `[IO.File]` takes `\` as part of a file name while PowerShell's provider
/// cmdlets (Test-Path, Get-Content) turn it into `/`. A record the wrapper wrote is read
/// back by the first kind; copying it to the real directory serves the second.
fn record_path(home: &TempDir, id: u64, ext: &str) -> std::path::PathBuf {
    home.parent().unwrap().join(format!(
        "{}\\agentpc-bg\\{id}.{ext}",
        home.file_name().unwrap().to_str().unwrap()
    ))
}

fn pwsh(home: &TempDir, script: &str) -> std::process::Output {
    std::process::Command::new("pwsh")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .env("USERPROFILE", &**home)
        .output()
        .unwrap()
}

/// Review finding 1: an explicit `exit N` ends PowerShell before the wrapper's own record,
/// yet the full code is recorded, so ssh's 255 is settled as the command's own code.
#[test]
fn windows_explicit_exit_codes_are_recorded() {
    if !have_pwsh() {
        return;
    }
    let home = tempdir();
    for (id, body, ssh) in [
        (201, "Write-Output x; exit 255", 255),
        (202, "exit -1", 255),
        (203, "exit 511", 255),
        (204, "function f { exit 7 }; f", 7),
    ] {
        let out = pwsh(&home, &windows_foreground(id, body));
        assert_eq!(out.status.code(), Some(ssh), "{body}");
        let rec = std::fs::read_to_string(record_path(&home, id, "exit"))
            .unwrap_or_else(|_| panic!("{body}: no exit record"));
        let rec: i64 = rec.trim().parse().unwrap();
        assert_eq!(rec & 0xff, i64::from(ssh), "{body}: {rec}");
        let probe = Probe::parse(&String::from_utf8_lossy(
            &pwsh(&home, &windows_probe(id)).stdout,
        ));
        let mut stderr = Captured::default();
        stderr.push(b"");
        let r = conclude(
            Facts {
                code: Some(ssh),
                timed_out: false,
                request_cancelled: None,
                start: Start::Seen,
                cancel: None,
                probe: Some(probe),
                stdout: Captured::default(),
                stderr,
            },
            id,
            "vm",
            120,
        );
        assert_eq!(
            (r.outcome.state, r.outcome.exit_code),
            (ExecutionState::Exited, Some(ssh)),
            "{body}: {}",
            r.text
        );
    }
}

/// Review finding 2: when the process list can't be read (here: no Get-CimInstance off
/// Windows, as with a broken WMI), the stop must not claim the execution is gone while its
/// recorded PowerShell still runs.
#[test]
fn a_windows_stop_that_cannot_list_processes_never_confirms() {
    if !have_pwsh() {
        return;
    }
    let home = tempdir();
    let id = 211;
    let mut live = std::process::Command::new("pwsh")
        .args(["-NoProfile", "-NonInteractive", "-Command"])
        .arg(windows_foreground(id, "Start-Sleep 60"))
        .env("USERPROFILE", &*home)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::fs::read_to_string(record_path(&home, id, "pid"))
        .map_or(true, |r| r.split(' ').count() != 2)
    {
        assert!(std::time::Instant::now() < deadline, "no record");
        std::thread::sleep(Duration::from_millis(100));
    }
    let real = home.join("agentpc-bg");
    std::fs::create_dir_all(&real).unwrap();
    for ext in ["fg", "pid"] {
        std::fs::copy(
            record_path(&home, id, ext),
            real.join(format!("{id}.{ext}")),
        )
        .unwrap();
    }
    let out = pwsh(&home, &windows_stop_script(id, true));
    let _ = live.kill();
    let _ = live.wait();
    let cancel = Cancel::from_stop(
        out.status.code(),
        false,
        &String::from_utf8_lossy(&out.stdout),
        45,
    );
    assert!(
        matches!(cancel, Cancel::Unconfirmed(_)),
        "{cancel:?}: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// Runs a Linux job script with HOME pointing at `home`, as far as it gets without /proc:
/// the record checks before any process work.
fn bash_in(home: &TempDir, script: &str) -> std::process::Output {
    std::process::Command::new("bash")
        .args(["-c", script])
        .env("HOME", &**home)
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap()
}

/// Review findings 3 and 5 on Linux: a cancel finding the payload's exit record says so in
/// a machine line; stop_job on an id with no record tombstones it, so a run whose record
/// never appeared can't start later.
#[test]
fn linux_stop_reports_the_recorded_exit_and_tombstones_unknown_ids() {
    let home = tempdir();
    let d = home.join("agentpc-bg");
    std::fs::create_dir_all(&d).unwrap();
    for (ext, body) in [("fg", ""), ("pid", "999999999"), ("exit", "3")] {
        std::fs::write(d.join(format!("31.{ext}")), body).unwrap();
    }
    let out = bash_in(&home, &linux_stop_script(31, true));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        Cancel::from_stop(out.status.code(), false, &stdout, 45),
        Cancel::Stopped { exited: Some(3) },
        "{stdout}"
    );
    let out = bash_in(&home, &linux_stop_script(32, false));
    assert_eq!(out.status.code(), Some(3));
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(said.contains("no such job 32"), "{said}");
    // Fix2 C: the tombstone only binds a foreground run's wrapper; a background launch
    // doesn't check it, so the reply must not promise that nothing can start.
    assert!(!said.contains("nothing can start"), "{said}");
    assert!(said.contains("foreground"), "{said}");
    for script in [windows_stop_script(32, false), linux_stop_script(32, false)] {
        assert!(!script.contains("nothing can start"));
    }
    assert!(d.join("32.cancel").exists(), "unknown id not tombstoned");
    // The probe reads a recorded code, and tombstones what has no record.
    let out = bash_in(&home, &linux_probe(31));
    assert_eq!(
        Probe::parse(&String::from_utf8_lossy(&out.stdout)),
        Probe::Exited(3)
    );
    let out = bash_in(&home, &linux_probe(33));
    assert_eq!(
        Probe::parse(&String::from_utf8_lossy(&out.stdout)),
        Probe::NotStarted
    );
    assert!(d.join("33.cancel").exists());
}

/// Review finding 6: the Linux foreground wrapper, run for real by local bash with HOME at a
/// temp dir (no systemd or /proc here, so it takes the plain path and its proof stays
/// empty): the payload never lands on disk or in the payload's environment, keeps `$0`
/// `bash` as with a plain SSH command, and its code is recorded; a tombstone stops it.
#[test]
fn linux_wrapper_runs_under_local_bash_without_writing_the_payload() {
    let home = tempdir();
    let run = |id: u64, payload: &str| {
        let out = bash_in(&home, &linux_foreground(id, payload, "AGENTPC_TEST_ENV=1"));
        let mut err = Stream::with_marker(start_marker(id));
        err.feed(&out.stderr);
        let (stderr, start) = err.finish(true);
        (
            out.status.code(),
            start,
            String::from_utf8_lossy(&out.stdout).to_string(),
            stderr.text(),
        )
    };
    let (code, start, stdout, stderr) = run(
        401,
        "echo \"secret-token-xyz $0 ${AGENTPC_CMD-unset} $AGENTPC_TEST_ENV\"; echo err >&2; exit 3",
    );
    assert_eq!((code, start), (Some(3), Start::Seen), "{stderr}");
    assert_eq!(stdout, "secret-token-xyz bash unset 1\n");
    assert!(stderr.ends_with("err"), "{stderr}");
    let d = home.join("agentpc-bg");
    assert_eq!(
        std::fs::read_to_string(d.join("401.exit")).unwrap().trim(),
        "3"
    );
    assert!(d.join("401.fg").exists() && d.join("401.pid").exists());
    for e in std::fs::read_dir(&d).unwrap().flatten() {
        let body = std::fs::read(e.path()).unwrap_or_default();
        assert!(
            !String::from_utf8_lossy(&body).contains("secret-token-xyz"),
            "payload written to {}",
            e.path().display()
        );
    }
    // A tombstone written before the wrapper records itself: it never runs the payload.
    std::fs::write(d.join("402.cancel"), "").unwrap();
    let (code, start, stdout, _) = run(402, "echo ran");
    assert_eq!(
        (code, start, stdout.as_str()),
        (Some(WRAPPER_CANCELLED), Start::Absent, "")
    );
    assert!(!d.join("402.exit").exists());
}

/// A stand-in for systemd-run (and the systemctl and setsid the background launch calls),
/// in a directory put first on PATH. `MOCK_SD_VERSION` picks the release it behaves as, taken
/// from systemd's NEWS and src/run/run.c: before 254 no `--expand-environment=` (an unknown
/// option fails) and a scope's command line is never expanded; 254 to 257 accept the switch
/// and leave a scope's line alone by default; from 258 systemd-run itself expands it by
/// default (`$$` becomes `$`, `${...}` the variable or, for a name that isn't one, nothing
/// plus a warning on stderr) before it execs the command. Each call's options go to
/// `MOCK_SD_LOG`, one line per call.
const MOCK_SYSTEMD_RUN: &str = r#"#!/bin/bash
v=${MOCK_SD_VERSION:?}; o=; expand=
while [ $# -gt 0 ]; do
  case $1 in
    --version) echo "CALL $o --version" >> "$MOCK_SD_LOG"; echo "systemd $v (mock)"; exit 0 ;;
    --expand-environment=*)
      [ "$v" -ge 254 ] || { echo "systemd-run: unrecognized option '$1'" >&2; exit 1; }
      expand=${1#*=} ;;
    --) shift; break ;;
    -*) ;;
    *) break ;;
  esac
  o="$o $1"; shift
done
echo "CALL $o" >> "$MOCK_SD_LOG"
if [ -z "$expand" ]; then if [ "$v" -ge 258 ]; then expand=yes; else expand=no; fi; fi
[ "$expand" = yes ] || exec "$@"
exec perl -e '
my $bad = "";
for (@ARGV) {
  s/\$(\$|\{([^}]*)\})/ $1 eq "\$" ? "\$" : (($2 =~ m{^[A-Za-z_]\w*$}) ? ($ENV{$2} \/\/ "") : do { $bad .= ($bad eq "" ? "" : ", ") . $2; "" }) /ge;
}
print STDERR "Invalid environment variable name evaluates to an empty string: $bad\n" if $bad ne "";
exec { $ARGV[0] } @ARGV or die "exec: $!";
' "$@"
"#;

struct MockSystemd {
    dir: TempDir,
    /// A real socket standing for the user manager's bus.
    bus: std::path::PathBuf,
    _listener: std::os::unix::net::UnixListener,
}

impl MockSystemd {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir();
        for (name, body) in [
            ("systemd-run", MOCK_SYSTEMD_RUN),
            ("systemctl", "#!/bin/sh\nexit 0\n"),
            ("setsid", "#!/bin/sh\nexec \"$@\"\n"),
        ] {
            let p = dir.join(name);
            std::fs::write(&p, body).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let bus = dir.join("bus");
        let _listener = std::os::unix::net::UnixListener::bind(&bus).unwrap();
        Self {
            dir,
            bus,
            _listener,
        }
    }

    /// Runs `script` under bash as the guest would, with HOME at `home` and this systemd.
    fn run(&self, home: &TempDir, version: u32, script: &str) -> std::process::Output {
        std::process::Command::new("bash")
            .args(["-c", script])
            .env("HOME", &**home)
            .env("PATH", format!("{}:/usr/bin:/bin", self.dir.display()))
            .env("MOCK_SD_VERSION", version.to_string())
            .env("MOCK_SD_LOG", self.dir.join(format!("calls-{version}")))
            .output()
            .unwrap()
    }

    /// The options of each systemd-run call made as `version`.
    fn calls(&self, version: u32) -> Vec<String> {
        std::fs::read_to_string(self.dir.join(format!("calls-{version}")))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

/// The scope's command line keeps every `$`. Final review (Arch): systemd 258 began expanding
/// a scope's command line in systemd-run itself, which turned the wrapper's `/proc/$$/stat`
/// into `/proc/$/stat`, its `echo $$ > <id>.pid` into a pid record of `$`, and `${20}` and
/// `${s##*) }` into nothing plus a warning on the command's stderr. On every release the
/// wrapper's recorded pid is the shell that ran the payload, the payload runs exactly once
/// with its own `$` text intact, stderr carries only the payload's, and the switch that turns
/// expansion off is passed exactly where systemd-run accepts it (never on 249, whose scope
/// keeps working without it).
#[test]
fn linux_foreground_scope_keeps_dollars_on_every_systemd() {
    let sd = MockSystemd::new();
    let home = tempdir();
    let proc_here = std::path::Path::new("/proc/self/stat").exists();
    let payload = "printf '%s|%s\\n' \"$PPID\" '$$ ${20} ${s##*) }'; \
                   echo \"${AGENTPC_TEST_ENV}x\"; echo ran >> ~/ran-$AGENTPC_RUN_ID; \
                   echo payload-err >&2; exit 4";
    for (n, version) in [258u32, 255, 249].into_iter().enumerate() {
        let id = 600 + n as u64;
        let script = linux_foreground(
            id,
            &payload.replace("$AGENTPC_RUN_ID", &id.to_string()),
            "AGENTPC_TEST_ENV=1",
        );
        assert!(script.contains("[ -S /run/user/1000/bus ]"));
        let script = script.replace("/run/user/1000/bus", &sd.bus.to_string_lossy());
        let out = sd.run(&home, version, &script);
        let mut err = Stream::with_marker(start_marker(id));
        err.feed(&out.stderr);
        let (stderr, start) = err.finish(true);
        let stderr = stderr.text();
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let d = home.join("agentpc-bg");
        let pid = std::fs::read_to_string(d.join(format!("{id}.pid"))).unwrap();
        let pid = pid.trim();
        assert!(
            !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()),
            "systemd {version}: pid record {pid:?}; stderr {stderr:?}"
        );
        assert_eq!(
            (out.status.code(), start),
            (Some(4), Start::Seen),
            "systemd {version}: {stderr}"
        );
        assert_eq!(
            stdout,
            format!("{pid}|$$ ${{20}} ${{s##*) }}\n1x\n"),
            "systemd {version}: the payload ran in the recorded shell, its text intact"
        );
        assert!(
            !stderr.contains("environment variable") && !stderr.contains("/proc/$/"),
            "systemd {version}: wrapper diagnostics on stderr: {stderr:?}"
        );
        if proc_here {
            // A guest's /proc: nothing but the payload's own stderr, and a full proof.
            assert_eq!(stderr, "payload-err", "systemd {version}");
            let proof = std::fs::read_to_string(d.join(format!("{id}.proof"))).unwrap();
            let words: Vec<&str> = proof.split_whitespace().collect();
            assert!(
                words.len() == 2 && words[1].bytes().all(|b| b.is_ascii_digit()),
                "systemd {version}: proof {proof:?}"
            );
        } else {
            assert!(
                stderr.ends_with("payload-err"),
                "systemd {version}: {stderr:?}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(home.join(format!("ran-{id}"))).unwrap(),
            "ran\n",
            "systemd {version}: the payload ran exactly once"
        );
        let scopes: Vec<String> = sd
            .calls(version)
            .into_iter()
            .filter(|c| c.contains("--scope"))
            .collect();
        assert_eq!(
            scopes.len(),
            1,
            "systemd {version}: one scope, no fallback: {scopes:?}"
        );
        assert_eq!(
            scopes[0].contains("--expand-environment=no"),
            version >= 254,
            "systemd {version}: {scopes:?}"
        );
        assert!(scopes[0].contains(&format!("--unit=agentpc-job-{id}")));
    }
}

/// The background launch's scope gets the same switch where systemd-run takes it, and its job
/// still runs (in a scope, even on 249) with its script's `$` text intact.
#[test]
fn linux_background_scope_keeps_dollars_on_every_systemd() {
    use base64::Engine;
    let sd = MockSystemd::new();
    let home = tempdir();
    let script = "printf '%s\\n' '$$ ${20} ${s##*) }'; echo \"${AGENTPC_TEST_ENV}\"; exit 6";
    let b64 = base64::engine::general_purpose::STANDARD.encode(script);
    for (n, version) in [258u32, 255, 249].into_iter().enumerate() {
        let id = 700 + n as u64;
        let out = sd.run(
            &home,
            version,
            &linux_background(id, &b64, "AGENTPC_TEST_ENV=1"),
        );
        assert_eq!(
            out.status.code(),
            Some(0),
            "systemd {version}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let d = home.join("agentpc-bg");
        let exit = d.join(format!("{id}.exit"));
        let started = std::time::Instant::now();
        while !exit.exists() && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            std::fs::read_to_string(&exit).unwrap().trim(),
            "6",
            "systemd {version}"
        );
        assert_eq!(
            std::fs::read_to_string(d.join(format!("{id}.log"))).unwrap(),
            "$$ ${20} ${s##*) }\n1\n",
            "systemd {version}"
        );
        let scopes: Vec<String> = sd
            .calls(version)
            .into_iter()
            .filter(|c| c.contains("--scope"))
            .collect();
        assert_eq!(scopes.len(), 1, "systemd {version}: {scopes:?}");
        assert_eq!(
            scopes[0].contains("--expand-environment=no"),
            version >= 254,
            "systemd {version}: {scopes:?}"
        );
    }
}

// -----------------------------------------------------------------------------------------
// Supervision and request cancellation: the run belongs to its own task, a cancelled
// request or a dropped handler stops it in the "guest" once, and a run that ended by itself
// is never stopped. Real local processes stand in for ssh and the guest, as above.

use std::sync::{Arc, Mutex};

/// Every guest-side stop the run asked for, in order.
#[derive(Clone, Default)]
struct Cleanups(Arc<Mutex<Vec<StopCause>>>);

impl Cleanups {
    fn calls(&self) -> Vec<StopCause> {
        self.0.lock().unwrap().clone()
    }
}

/// The process group a test script recorded (`echo $$ > pidfile`, its own group).
fn group(pidfile: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(pidfile)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn kill_group(pidfile: &std::path::Path, sig: &str) {
    if let Some(g) = group(pidfile) {
        let _ = std::process::Command::new("kill")
            .args([sig, &format!("-{g}")])
            .status();
    }
}

fn group_alive(pidfile: &std::path::Path) -> bool {
    group(pidfile).is_some_and(|g| {
        std::process::Command::new("kill")
            .args(["-0", &format!("-{g}")])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

/// The guest-side stop, played locally: records why it was asked, kills the recorded group
/// (after `delay`), and answers `answer`.
fn stopper(
    cleanups: &Cleanups,
    pidfile: &std::path::Path,
    delay: Duration,
    answer: Cancel,
) -> impl FnOnce(StopCause) -> std::pin::Pin<Box<dyn std::future::Future<Output = Cancel> + Send>>
+ Send
+ 'static {
    let cleanups = cleanups.clone();
    let pidfile = pidfile.to_path_buf();
    move |cause| {
        Box::pin(async move {
            cleanups.0.lock().unwrap().push(cause);
            tokio::time::sleep(delay).await;
            kill_group(&pidfile, "-TERM");
            answer
        })
    }
}

async fn wait_for_file(path: &std::path::Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A script that records its group, prints `before` after the marker, then sleeps.
fn sleeper(pidfile: &std::path::Path, id: u64) -> String {
    format!(
        "echo $$ > {}; {}; echo before; sleep 30; echo after",
        pidfile.display(),
        marker_printf(id)
    )
}

/// One supervised foreground run of `script`, the way run_command's handler runs it.
async fn supervised_run(
    sup: &Supervisors,
    requested: impl std::future::Future<Output = ()>,
    script: String,
    id: u64,
    stop: impl FnOnce(StopCause) -> std::pin::Pin<Box<dyn std::future::Future<Output = Cancel> + Send>>
    + Send
    + 'static,
) -> Supervision<(Reply, Option<CancelSource>)> {
    sup.supervise(Mode::Foreground, requested, move |signal| async move {
        let f = run_supervised(
            sh(&script),
            id,
            Duration::from_secs(30),
            FAST,
            &signal,
            stop,
            never_probe,
        )
        .await
        .unwrap();
        let who = f.request_cancelled;
        (conclude(f, id, "vm", 30), who)
    })
    .await
}

async fn drained(sup: &Supervisors) {
    let left = sup
        .drain(tokio::time::Instant::now() + Duration::from_secs(10))
        .await;
    assert_eq!(left, 0, "supervised runs left");
}

#[tokio::test]
async fn a_cancelled_request_stops_its_started_run_once_and_keeps_partial_output() {
    let dir = tempdir();
    let pidfile = dir.join("pid");
    let cleanups = Cleanups::default();
    let sup = Supervisors::new();
    let pf = pidfile.clone();
    let requested = async move {
        wait_for_file(&pf).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
    };
    let started = std::time::Instant::now();
    let s = supervised_run(
        &sup,
        requested,
        sleeper(&pidfile, 21),
        21,
        stopper(
            &cleanups,
            &pidfile,
            Duration::ZERO,
            Cancel::Stopped { exited: None },
        ),
    )
    .await;
    let Supervision::Done((r, who)) = s else {
        panic!("{s:?}")
    };
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(cleanups.calls(), [StopCause::RequestCancelled]);
    assert_eq!(who, Some(CancelSource::Request));
    let o = &r.outcome;
    assert_eq!(o.state, ExecutionState::Cancelled);
    assert!(o.request_cancelled && !o.timed_out && !o.effect_unknown);
    assert_eq!(o.termination_confirmed, Some(true));
    assert_eq!(o.exit_code, None);
    assert_eq!(o.stdout, "before");
    assert!(r.text.starts_with("cancelled: "), "{}", r.text);
    assert!(r.text.contains("--- stdout ---\nbefore"), "{}", r.text);
    let res = r.into_result();
    assert_eq!(res.is_error, Some(true));
    assert_eq!(
        res.structured_content.as_ref().unwrap()["state"],
        "cancelled"
    );
    assert_eq!(
        res.structured_content.as_ref().unwrap()["request_cancelled"],
        true
    );
    assert!(!group_alive(&pidfile));
    drained(&sup).await;
}

/// The future waiting on the run goes away (rmcp drops a handler only when its runtime goes,
/// but nothing here may depend on that): the run's own task still stops it in the guest and
/// ends; nothing is left behind.
#[tokio::test]
async fn dropping_the_waiting_handler_still_stops_the_run() {
    let dir = tempdir();
    let pidfile = dir.join("pid");
    let cleanups = Cleanups::default();
    let sup = Supervisors::new();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let script = sleeper(&pidfile, 22);
    let stop = stopper(
        &cleanups,
        &pidfile,
        Duration::ZERO,
        Cancel::Stopped { exited: None },
    );
    let waiting = sup.supervise(
        Mode::Foreground,
        std::future::pending(),
        move |signal| async move {
            let f = run_supervised(
                sh(&script),
                22,
                Duration::from_secs(30),
                FAST,
                &signal,
                stop,
                never_probe,
            )
            .await
            .unwrap();
            let _ = tx.send((f.request_cancelled, conclude(f, 22, "vm", 30).outcome));
        },
    );
    let pf = pidfile.clone();
    tokio::select! {
        _ = waiting => panic!("the run ended by itself"),
        () = async { wait_for_file(&pf).await; tokio::time::sleep(Duration::from_millis(300)).await } => {}
    }
    // `waiting` is dropped here, mid-run.
    let (who, o) = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("the run ended")
        .unwrap();
    assert_eq!(who, Some(CancelSource::HandlerDropped));
    assert_eq!(cleanups.calls(), [StopCause::RequestCancelled]);
    assert_eq!(o.state, ExecutionState::Cancelled);
    assert_eq!(o.termination_confirmed, Some(true));
    assert!(!group_alive(&pidfile));
    drained(&sup).await;
}

/// A run that ended by itself is never stopped: not when its request's token fires during the
/// steps after it (here a leftover holds its pipes, so it is still reading), and not when the
/// finished handler goes away. rmcp also cancels a request's token when it sends the reply.
#[tokio::test]
async fn a_run_that_ended_is_never_cancelled_even_when_its_token_fires_at_the_end() {
    for round in 0..3 {
        let dir = tempdir();
        let done = dir.join("done");
        let leftover = dir.join("pid");
        let cleanups = Cleanups::default();
        let sup = Supervisors::new();
        // The command exits 3 at once; a leftover keeps its pipes for 2 s, longer than the
        // run reads them (FAST.drain_grace), so the token fires while the run still works.
        let script = format!(
            "{}; (echo $$ > {}; exec sleep 2) & touch {}; exit 3",
            marker_printf(23),
            leftover.display(),
            done.display()
        );
        let d = done.clone();
        let requested = async move {
            wait_for_file(&d).await;
            tokio::time::sleep(Duration::from_millis(250)).await;
        };
        let s = supervised_run(
            &sup,
            requested,
            script,
            23,
            stopper(
                &cleanups,
                &leftover,
                Duration::ZERO,
                Cancel::Stopped { exited: None },
            ),
        )
        .await;
        let Supervision::Done((r, who)) = s else {
            panic!("{s:?}")
        };
        assert_eq!(cleanups.calls(), [], "round {round}");
        assert_eq!(who, None, "round {round}");
        assert_eq!(
            (r.outcome.state, r.outcome.exit_code),
            (ExecutionState::Exited, Some(3)),
            "round {round}"
        );
        assert!(!r.outcome.request_cancelled);
        drained(&sup).await;
        kill_group(&leftover, "-KILL");
    }
    // The ordinary case: done, then whatever fires is ignored.
    let sup = Supervisors::new();
    let s = sup
        .supervise(
            Mode::Foreground,
            std::future::pending(),
            |signal| async move {
                let f = run_supervised(
                    sh(&format!("{}; exit 0", marker_printf(24))),
                    24,
                    Duration::from_secs(30),
                    FAST,
                    &signal,
                    |_| async { panic!("a finished run was cancelled") },
                    never_probe,
                )
                .await
                .unwrap();
                (f.request_cancelled, signal)
            },
        )
        .await;
    let Supervision::Done((who, signal)) = s else {
        panic!("{s:?}")
    };
    assert_eq!(who, None);
    assert_eq!(
        signal.fired(),
        None,
        "a finished handler disarms its signal"
    );
    drained(&sup).await;
}

/// A request already cancelled starts nothing; one cancelled after its run was dispatched
/// but before the wrapper started the payload gets the guest's "never started" answer.
#[tokio::test]
async fn a_cancellation_before_the_start_never_starts_the_command() {
    let dir = tempdir();
    let ran = dir.join("ran");
    let sup = Supervisors::new();
    // Already cancelled when the call arrives: no task, no process.
    let mut invoked = false;
    let s = sup
        .supervise(Mode::Foreground, std::future::ready(()), |_| {
            invoked = true;
            async {}
        })
        .await;
    assert!(matches!(s, Supervision::CancelledBeforeStart), "{s:?}");
    assert!(!invoked, "the work was not even built");
    let s = sup
        .supervise(Mode::Background, std::future::ready(()), |_| async {})
        .await;
    assert!(matches!(s, Supervision::CancelledBeforeStart), "{s:?}");
    // Fired between the call's check and the spawn: nothing is spawned or stopped.
    let signal = CancelSignal::new();
    assert!(signal.fire(CancelSource::Request));
    let f = run_supervised(
        sh(&format!("touch {}", ran.display())),
        25,
        Duration::from_secs(30),
        FAST,
        &signal,
        |_| async { panic!("nothing was started, so nothing is stopped") },
        never_probe,
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!ran.exists(), "the command was spawned");
    assert_eq!(f.request_cancelled, Some(CancelSource::Request));
    let r = conclude(f, 25, "vm", 30);
    let o = &r.outcome;
    assert_eq!(o.state, ExecutionState::NotStarted);
    assert!(o.request_cancelled && !o.effect_unknown);
    assert_eq!((o.execution_id, o.job_id), (Some(25), None));
    assert_eq!(o.termination_confirmed, None);
    assert_eq!(
        o.recovery_action, None,
        "a cancelled request is not retried"
    );
    assert!(r.text.contains("never"), "{}", r.text);
    // Dispatched, cancelled before the wrapper's marker: the guest says it never started.
    let pidfile = dir.join("pid");
    let cleanups = Cleanups::default();
    let script = format!(
        "echo $$ > {}; sleep 1; touch {}; {}; echo payload",
        pidfile.display(),
        ran.display(),
        marker_printf(26)
    );
    let pf = pidfile.clone();
    let s = supervised_run(
        &sup,
        async move { wait_for_file(&pf).await },
        script,
        26,
        stopper(&cleanups, &pidfile, Duration::ZERO, Cancel::NotStarted),
    )
    .await;
    let Supervision::Done((r, _)) = s else {
        panic!("{s:?}")
    };
    assert_eq!(cleanups.calls(), [StopCause::RequestCancelled]);
    assert_eq!(r.outcome.state, ExecutionState::NotStarted);
    assert!(r.outcome.request_cancelled);
    assert_eq!(r.outcome.stdout, "");
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(!ran.exists(), "the payload started after the cancellation");
    drained(&sup).await;
}

/// The guest can't confirm the stop, or doesn't answer at all: the reply (and the bounded
/// time it takes) says so and hands out the id; it never claims termination.
#[tokio::test]
async fn an_unconfirmed_or_hanging_cleanup_is_bounded_and_honest() {
    for (n, hang) in [(27u64, false), (28, true)] {
        let dir = tempdir();
        let pidfile = dir.join("pid");
        let sup = Supervisors::new();
        let pf = pidfile.clone();
        let started = std::time::Instant::now();
        let script = sleeper(&pidfile, n);
        let s = sup
            .supervise(
                Mode::Foreground,
                async move { wait_for_file(&pf).await },
                move |signal| async move {
                    let f = run_supervised(
                        sh(&script),
                        n,
                        Duration::from_secs(30),
                        FAST,
                        &signal,
                        move |_| async move {
                            if hang {
                                std::future::pending::<Cancel>().await
                            } else {
                                Cancel::Unconfirmed("the VM did not answer".into())
                            }
                        },
                        never_probe,
                    )
                    .await
                    .unwrap();
                    conclude(f, n, "vm", 30)
                },
            )
            .await;
        let elapsed = started.elapsed();
        kill_group(&pidfile, "-KILL");
        let Supervision::Done(r) = s else {
            panic!("{s:?}")
        };
        assert!(
            elapsed < FAST.cancel + FAST.session_grace + FAST.drain_grace + Duration::from_secs(3),
            "{elapsed:?}"
        );
        let o = &r.outcome;
        assert_eq!(o.state, ExecutionState::Cancelled, "{n}");
        assert!(o.request_cancelled && o.effect_unknown, "{n}");
        assert_eq!(o.termination_confirmed, Some(false), "{n}");
        assert_eq!(o.job_id, Some(n));
        assert_eq!(o.recovery_action, Some(RecoveryAction::StopJob));
        assert!(
            r.text.contains(&format!("get_job_status name=vm id={n}")),
            "{}",
            r.text
        );
        assert!(!r.text.contains("agentpc stopped"), "{}", r.text);
        drained(&sup).await;
    }
}

/// Every source may ask (the token, the handler going away, shutdown, and the timeout racing
/// them), yet the guest-side stop runs once.
#[tokio::test]
async fn many_cancellations_stop_the_run_once() {
    let dir = tempdir();
    let pidfile = dir.join("pid");
    let cleanups = Cleanups::default();
    let sup = Supervisors::new();
    let pf = pidfile.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let script = sleeper(&pidfile, 29);
    // A slow stop, so the other sources fire while it runs.
    let stop = stopper(
        &cleanups,
        &pidfile,
        Duration::from_millis(400),
        Cancel::Stopped { exited: None },
    );
    let waiting = sup.supervise(
        Mode::Foreground,
        async move { wait_for_file(&pf).await },
        move |signal| async move {
            let f = run_supervised(
                sh(&script),
                29,
                // The timeout falls inside the slow stop.
                Duration::from_millis(600),
                FAST,
                &signal,
                stop,
                never_probe,
            )
            .await
            .unwrap();
            let _ = tx.send(f.request_cancelled);
        },
    );
    let _ = tokio::time::timeout(Duration::from_millis(500), waiting).await;
    sup.close();
    let who = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(who, Some(CancelSource::Request));
    assert_eq!(cleanups.calls(), [StopCause::RequestCancelled]);
    drained(&sup).await;
    assert_eq!(cleanups.calls().len(), 1);
}

/// Shutdown asks every run to stop, waits for them within its deadline, and takes no new
/// ones; a run that won't end doesn't hold it past the deadline.
#[tokio::test]
async fn shutdown_signals_and_drains_runs_within_its_deadline() {
    let dir = tempdir();
    let sup = Supervisors::new();
    let cleanups = Cleanups::default();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut pidfiles = vec![];
    for n in [31u64, 32] {
        let pidfile = dir.join(format!("pid{n}"));
        pidfiles.push(pidfile.clone());
        let script = sleeper(&pidfile, n);
        let stop = stopper(
            &cleanups,
            &pidfile,
            Duration::ZERO,
            Cancel::Stopped { exited: None },
        );
        let tx = tx.clone();
        let sup2 = sup.clone();
        tokio::spawn(async move {
            let s = sup2
                .supervise(
                    Mode::Foreground,
                    std::future::pending(),
                    move |signal| async move {
                        let f = run_supervised(
                            sh(&script),
                            n,
                            Duration::from_secs(30),
                            FAST,
                            &signal,
                            stop,
                            never_probe,
                        )
                        .await
                        .unwrap();
                        (f.request_cancelled, conclude(f, n, "vm", 30).outcome.state)
                    },
                )
                .await;
            let _ = tx.send(s);
        });
    }
    for p in &pidfiles {
        wait_for_file(p).await;
    }
    assert_eq!(sup.active(), 2);
    let started = std::time::Instant::now();
    assert_eq!(
        sup.close(),
        Closing {
            foreground: 2,
            background: 0
        }
    );
    drained(&sup).await;
    assert!(started.elapsed() < Duration::from_secs(5));
    for _ in 0..2 {
        let s = rx.recv().await.unwrap();
        let Supervision::Done((who, state)) = s else {
            panic!("{s:?}")
        };
        assert_eq!(who, Some(CancelSource::Shutdown));
        assert_eq!(state, ExecutionState::Cancelled);
    }
    assert_eq!(
        cleanups.calls(),
        [StopCause::RequestCancelled, StopCause::RequestCancelled]
    );
    for p in &pidfiles {
        assert!(!group_alive(p));
    }
    // Closed: nothing new starts.
    let mut invoked = false;
    let s = sup
        .supervise(Mode::Foreground, std::future::pending(), |_| {
            invoked = true;
            async {}
        })
        .await;
    assert!(matches!(s, Supervision::ShuttingDown), "{s:?}");
    assert!(!invoked);
    // A run that ignores its signal is waited for only until the deadline.
    let sup = Supervisors::new();
    let stuck = sup.clone();
    tokio::spawn(async move {
        stuck
            .supervise(Mode::Foreground, std::future::pending(), |_| {
                std::future::pending::<()>()
            })
            .await
    });
    while sup.active() == 0 {
        tokio::task::yield_now().await;
    }
    sup.close();
    let started = std::time::Instant::now();
    let left = sup
        .drain(tokio::time::Instant::now() + Duration::from_millis(300))
        .await;
    assert_eq!(left, 1);
    assert!(started.elapsed() < Duration::from_secs(2));
}

/// The registry is bounded: one run past MAX_SUPERVISED is refused before it starts.
#[tokio::test]
async fn supervision_is_bounded() {
    let sup = Supervisors::new();
    for _ in 0..MAX_SUPERVISED {
        let s = sup.clone();
        tokio::spawn(async move {
            s.supervise(
                Mode::Foreground,
                std::future::pending(),
                |signal| async move {
                    signal.wait().await;
                },
            )
            .await
        });
    }
    while sup.active() < MAX_SUPERVISED {
        tokio::task::yield_now().await;
    }
    let mut invoked = false;
    let s = sup
        .supervise(Mode::Foreground, std::future::pending(), |_| {
            invoked = true;
            async {}
        })
        .await;
    assert!(matches!(s, Supervision::Busy), "{s:?}");
    assert!(!invoked);
    assert_eq!(
        sup.close(),
        Closing {
            foreground: MAX_SUPERVISED,
            background: 0
        }
    );
    drained(&sup).await;
}

/// A background launch outlives its request's cancellation: once started it completes, and
/// neither the token nor the handler going away fires its signal.
#[tokio::test]
async fn a_background_launch_is_not_cancelled_with_its_request() {
    let sup = Supervisors::new();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let waiting = sup.supervise(
        Mode::Background,
        tokio::time::sleep(Duration::from_millis(50)),
        move |signal| async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            let _ = tx.send(signal.fired());
        },
    );
    // The token fires at 50 ms; the handler is dropped at 150 ms.
    let _ = tokio::time::timeout(Duration::from_millis(150), waiting).await;
    let fired = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fired, None, "the launch was asked to stop");
    drained(&sup).await;
}

/// Review I1: ssh ends (here with 255, the code of a dropped connection) while the command
/// lives on in the "guest", and the request is cancelled on the same tick or while the
/// follow-up check runs. Before the fix only the check ran and its `running` answer went into
/// a reply nobody receives; now the run's one stop still runs, and a check that hangs doesn't
/// hold it back. A check that proves the command finished keeps that, and stops nothing.
#[tokio::test]
async fn a_cancellation_after_a_lost_connection_still_stops_the_command() {
    let lost = |pidfile: &std::path::Path, id: u64| {
        format!(
            "echo $$ > {}; {}; (exec sleep 30) >/dev/null 2>&1 & exit 255",
            pidfile.display(),
            marker_printf(id)
        )
    };
    // The cancellation lands as the check answers: still running, or no answer.
    for (n, answer) in [
        (51u64, Probe::Running),
        (52, Probe::Failed("no answer".into())),
    ] {
        let dir = tempdir();
        let pidfile = dir.join("pid");
        let cleanups = Cleanups::default();
        let signal = CancelSignal::new();
        let sig = signal.clone();
        let f = run_supervised(
            sh(&lost(&pidfile, n)),
            n,
            Duration::from_secs(30),
            FAST,
            &signal,
            stopper(
                &cleanups,
                &pidfile,
                Duration::ZERO,
                Cancel::Stopped { exited: None },
            ),
            move || async move {
                sig.fire(CancelSource::Request);
                answer
            },
        )
        .await
        .unwrap();
        let alive = group_alive(&pidfile);
        kill_group(&pidfile, "-KILL");
        assert_eq!(cleanups.calls(), [StopCause::RequestCancelled], "{n}");
        assert!(!alive, "{n}: the command was left running");
        assert_eq!(f.request_cancelled, Some(CancelSource::Request), "{n}");
        let o = conclude(f, n, "vm", 30).outcome;
        assert_eq!(o.state, ExecutionState::Cancelled, "{n}");
        assert!(o.request_cancelled, "{n}");
        assert_eq!(o.termination_confirmed, Some(true), "{n}");
    }
    // Re-review 1: the recorded root died with the connection (no exit record), so the
    // check says `gone`, but a nohup'd member of the run lives on. `gone` proves nothing
    // about the rest of the run: the one stop still runs and the member is ended.
    let dir = tempdir();
    let pidfile = dir.join("pid");
    let member = dir.join("member");
    let cleanups = Cleanups::default();
    let signal = CancelSignal::new();
    let sig = signal.clone();
    let script = format!(
        "echo $$ > {}; {}; nohup sh -c 'echo $$ > {}; exec sleep 30' >/dev/null 2>&1 & exit 255",
        pidfile.display(),
        marker_printf(55),
        member.display()
    );
    let m = member.clone();
    let f = run_supervised(
        sh(&script),
        55,
        Duration::from_secs(30),
        FAST,
        &signal,
        stopper(
            &cleanups,
            &pidfile,
            Duration::ZERO,
            Cancel::Stopped { exited: None },
        ),
        move || async move {
            // The member is up before the check answers.
            wait_for_file(&m).await;
            sig.fire(CancelSource::Request);
            Probe::Gone
        },
    )
    .await
    .unwrap();
    let member_pid = std::fs::read_to_string(&member).unwrap();
    let member_alive = std::process::Command::new("kill")
        .args(["-0", member_pid.trim()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    kill_group(&pidfile, "-KILL");
    assert_eq!(cleanups.calls(), [StopCause::RequestCancelled]);
    assert!(!member_alive, "the nohup'd member was left running");
    assert_eq!(f.request_cancelled, Some(CancelSource::Request));
    let o = conclude(f, 55, "vm", 30).outcome;
    assert_eq!(o.state, ExecutionState::Cancelled);
    assert!(o.request_cancelled);
    assert_eq!(o.termination_confirmed, Some(true));
    // The check hangs; the cancellation comes while it waits: the stop doesn't wait out the
    // check's (here 20 s) deadline.
    let dir = tempdir();
    let pidfile = dir.join("pid");
    let cleanups = Cleanups::default();
    let signal = CancelSignal::new();
    let checking = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (sig, flag) = (signal.clone(), checking.clone());
    tokio::spawn(async move {
        while !flag.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        sig.fire(CancelSource::Request);
    });
    let slow = Limits {
        probe: Duration::from_secs(20),
        ..FAST
    };
    let started = std::time::Instant::now();
    let f = run_supervised(
        sh(&lost(&pidfile, 53)),
        53,
        Duration::from_secs(30),
        slow,
        &signal,
        stopper(
            &cleanups,
            &pidfile,
            Duration::ZERO,
            Cancel::Stopped { exited: None },
        ),
        move || async move {
            checking.store(true, std::sync::atomic::Ordering::SeqCst);
            std::future::pending::<Probe>().await
        },
    )
    .await
    .unwrap();
    let took = started.elapsed();
    let alive = group_alive(&pidfile);
    kill_group(&pidfile, "-KILL");
    assert!(took < Duration::from_secs(5), "{took:?}");
    assert_eq!(cleanups.calls(), [StopCause::RequestCancelled]);
    assert!(!alive);
    assert_eq!(
        conclude(f, 53, "vm", 30).outcome.state,
        ExecutionState::Cancelled
    );
    // The check proves it finished (its recorded code): nothing is stopped, the code is kept.
    let dir = tempdir();
    let pidfile = dir.join("pid");
    let signal = CancelSignal::new();
    let sig = signal.clone();
    let f = run_supervised(
        sh(&lost(&pidfile, 54)),
        54,
        Duration::from_secs(30),
        FAST,
        &signal,
        |_| async { panic!("a finished command was stopped") },
        move || async move {
            sig.fire(CancelSource::Request);
            Probe::Exited(0)
        },
    )
    .await
    .unwrap();
    kill_group(&pidfile, "-KILL");
    assert_eq!(f.request_cancelled, None);
    let o = conclude(f, 54, "vm", 30).outcome;
    assert_eq!(o.exit_code, Some(0));
    assert!(!o.request_cancelled);
}

/// Review I2: two runs end at once on a multi-threaded runtime (as in production). Run A has
/// counted what is left (1) but not yet published it when run B ends and publishes 0; A's
/// late 1 must not leave `drain` waiting for a run that is gone. The seam holds A at exactly
/// that point and lets B go (or try to) meanwhile.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drain_sees_the_last_end_however_two_ends_interleave() {
    use std::sync::mpsc;
    let sup = Supervisors::new();
    let (a_end, a_rx) = tokio::sync::oneshot::channel::<()>();
    let (b_end, b_rx) = tokio::sync::oneshot::channel::<()>();
    let (b_done_tx, b_done_rx) = mpsc::channel::<()>();
    let s = sup.clone();
    tokio::spawn(async move {
        s.supervise(Mode::Foreground, std::future::pending(), |_| async move {
            let _ = a_rx.await;
        })
        .await
    });
    let s = sup.clone();
    tokio::spawn(async move {
        s.supervise(Mode::Foreground, std::future::pending(), |_| async move {
            let _ = b_rx.await;
        })
        .await;
        let _ = b_done_tx.send(());
    });
    while sup.active() < 2 {
        tokio::task::yield_now().await;
    }
    let (in_window_tx, in_window_rx) = mpsc::channel::<()>();
    let held = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let b_done = Mutex::new(b_done_rx);
    let in_window = Mutex::new(in_window_tx);
    let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let a_released = released.clone();
    sup.set_before_publish(move |n| {
        if n == 1 && !held.swap(true, std::sync::atomic::Ordering::SeqCst) {
            // A, between counting and publishing: let B end now, if it can.
            let _ = in_window.lock().unwrap().send(());
            let _ = b_done.lock().unwrap().recv_timeout(Duration::from_secs(2));
            a_released.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    });
    a_end.send(()).unwrap();
    tokio::task::spawn_blocking(move || in_window_rx.recv_timeout(Duration::from_secs(10)))
        .await
        .unwrap()
        .expect("A reached the window");
    b_end.send(()).unwrap();
    sup.close();
    // Both ends are over (A's publish is the statement after its seam returns) before
    // shutdown starts waiting.
    while !released.load(std::sync::atomic::Ordering::SeqCst) || sup.active() > 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    let started = std::time::Instant::now();
    let left = sup
        .drain(tokio::time::Instant::now() + Duration::from_secs(8))
        .await;
    let took = started.elapsed();
    assert_eq!(left, 0);
    assert!(
        took < Duration::from_secs(5),
        "drain waited {took:?} for runs that had ended"
    );
}

/// Shutdown tells foreground commands it stops from background launches it only waits for.
#[tokio::test]
async fn close_counts_stopped_commands_apart_from_awaited_launches() {
    let sup = Supervisors::new();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let (fired_tx, mut fired_rx) = tokio::sync::mpsc::unbounded_channel();
    let s = sup.clone();
    tokio::spawn(async move {
        s.supervise(
            Mode::Foreground,
            std::future::pending(),
            |signal| async move {
                signal.wait().await;
            },
        )
        .await
    });
    let s = sup.clone();
    tokio::spawn(async move {
        s.supervise(
            Mode::Background,
            std::future::pending(),
            |signal| async move {
                let _ = rx.await;
                let _ = fired_tx.send(signal.fired());
            },
        )
        .await
    });
    while sup.active() < 2 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        sup.close(),
        Closing {
            foreground: 1,
            background: 1
        }
    );
    // The foreground run stops; the launch is waited for, not stopped: it is still there
    // until it finishes by itself, and was never asked to stop.
    let left = sup
        .drain(tokio::time::Instant::now() + Duration::from_millis(500))
        .await;
    assert_eq!(left, 1);
    tx.send(()).unwrap();
    assert_eq!(fired_rx.recv().await, Some(None));
    drained(&sup).await;
}

// ---- The same through a real rmcp server and client, over an in-memory pipe ---------------

mod rpc {
    use super::*;
    use rmcp::model::{
        CallToolRequest, CallToolRequestParams, CallToolResponse, CallToolResult, ClientRequest,
        ContentBlock, ServerCapabilities, ServerConfig,
    };
    use rmcp::service::{PeerRequestOptions, RequestContext, RunningService};
    use rmcp::{RoleClient, RoleServer, ServerHandler, ServiceExt};

    /// A server with run_command's two paths: `fg` supervises a foreground run of the
    /// `script` argument with the request's own cancellation token, `bg` a launch that
    /// starts a detached process after a delay.
    #[derive(Clone)]
    struct Server {
        sup: Supervisors,
        cleanups: Cleanups,
        pidfile: std::path::PathBuf,
        /// What each handler concluded, once it got past its supervision.
        returned: Arc<Mutex<Vec<String>>>,
    }

    impl ServerHandler for Server {
        fn get_info(&self) -> ServerConfig {
            ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
        }

        async fn call_tool(
            &self,
            req: CallToolRequestParams,
            ctx: RequestContext<RoleServer>,
        ) -> Result<CallToolResponse, rmcp::ErrorData> {
            let arg = |k: &str| {
                req.arguments
                    .as_ref()
                    .and_then(|a| a.get(k))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string()
            };
            let text = match req.name.as_ref() {
                "fg" => {
                    let script = arg("script");
                    let stop = stopper(
                        &self.cleanups,
                        &self.pidfile,
                        Duration::ZERO,
                        Cancel::Stopped { exited: None },
                    );
                    let s = self
                        .sup
                        .supervise(
                            Mode::Foreground,
                            ctx.ct.cancelled(),
                            move |signal| async move {
                                let f = run_supervised(
                                    sh(&script),
                                    41,
                                    Duration::from_secs(30),
                                    FAST,
                                    &signal,
                                    stop,
                                    never_probe,
                                )
                                .await
                                .unwrap();
                                conclude(f, 41, "vm", 30)
                            },
                        )
                        .await;
                    match s {
                        Supervision::Done(r) => format!("{:?}", r.outcome.state),
                        other => format!("{other:?}"),
                    }
                }
                "bg" => {
                    let pidfile = self.pidfile.clone();
                    let s = self
                        .sup
                        .supervise(Mode::Background, ctx.ct.cancelled(), move |_| async move {
                            tokio::time::sleep(Duration::from_millis(400)).await;
                            // Detached, as a background job is: not killed with anything here.
                            use std::os::unix::process::CommandExt;
                            let mut job = std::process::Command::new("sh")
                                .args([
                                    "-c",
                                    &format!("echo $$ > {}; exec sleep 30", pidfile.display()),
                                ])
                                .process_group(0)
                                .stdout(std::process::Stdio::null())
                                .stderr(std::process::Stdio::null())
                                .spawn()
                                .unwrap();
                            // Reaped by a thread of its own, never waited on here.
                            std::thread::spawn(move || job.wait());
                        })
                        .await;
                    format!("{s:?}")
                }
                other => format!("no tool {other}"),
            };
            self.returned.lock().unwrap().push(text.clone());
            Ok(CallToolResult::success(vec![ContentBlock::text(text)]).into())
        }
    }

    async fn connect(
        server: Server,
    ) -> (RunningService<RoleClient, ()>, tokio::task::JoinHandle<()>) {
        let (server_io, client_io) = tokio::io::duplex(1 << 16);
        let served = tokio::spawn(async move {
            if let Ok(running) = server.serve(server_io).await {
                let _ = running.waiting().await;
            }
        });
        (().serve(client_io).await.unwrap(), served)
    }

    fn server(dir: &TempDir) -> Server {
        Server {
            sup: Supervisors::new(),
            cleanups: Cleanups::default(),
            pidfile: dir.join("pid"),
            returned: Arc::default(),
        }
    }

    fn call(name: &'static str, script: &str) -> ClientRequest {
        let mut params = CallToolRequestParams::new(name);
        params.arguments = serde_json::json!({ "script": script }).as_object().cloned();
        ClientRequest::CallToolRequest(CallToolRequest::new(params))
    }

    async fn until(what: &str, mut ok: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while !ok() {
            assert!(std::time::Instant::now() < deadline, "never: {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// rmcp 3.4.1 doesn't abort a handler on notifications/cancelled: it cancels the request's
    /// token and drops the reply. The handler sees the token, the run is stopped in the guest
    /// once, and the handler itself still runs to its end.
    #[tokio::test]
    async fn notifications_cancelled_stops_the_foreground_run() {
        let dir = tempdir();
        let srv = server(&dir);
        let (client, _served) = connect(srv.clone()).await;
        let handle = client
            .peer()
            .send_cancellable_request(
                call("fg", &sleeper(&srv.pidfile, 41)),
                PeerRequestOptions::no_options(),
            )
            .await
            .unwrap();
        wait_for_file(&srv.pidfile).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let cancelled_at = std::time::Instant::now();
        handle.cancel(Some("test".into())).await.unwrap();
        until("the stop ran", || !srv.cleanups.calls().is_empty()).await;
        assert!(cancelled_at.elapsed() < Duration::from_secs(5));
        until("the handler returned", || {
            !srv.returned.lock().unwrap().is_empty()
        })
        .await;
        assert_eq!(*srv.returned.lock().unwrap(), ["Cancelled"]);
        assert_eq!(srv.cleanups.calls(), [StopCause::RequestCancelled]);
        assert!(!group_alive(&srv.pidfile));
        drained(&srv.sup).await;
    }

    /// A call that completes gets its reply, and rmcp cancelling the finished request's token
    /// when it sends that reply stops nothing.
    #[tokio::test]
    async fn a_completed_call_is_never_cancelled() {
        let dir = tempdir();
        let srv = server(&dir);
        let (client, _served) = connect(srv.clone()).await;
        let r = client
            .peer()
            .send_request(call("fg", &format!("{}; echo hi", marker_printf(41))))
            .await
            .unwrap();
        assert!(format!("{r:?}").contains("Exited"), "{r:?}");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(srv.cleanups.calls(), []);
        drained(&srv.sup).await;
    }

    /// The client goes away mid-run (its transport closes): once rmcp's serve loop ends, the
    /// request's token is cancelled and the run is stopped.
    #[tokio::test]
    async fn a_disconnected_client_stops_the_foreground_run() {
        let dir = tempdir();
        let srv = server(&dir);
        let (client, served) = connect(srv.clone()).await;
        let handle = client
            .peer()
            .send_cancellable_request(
                call("fg", &sleeper(&srv.pidfile, 41)),
                PeerRequestOptions::no_options(),
            )
            .await
            .unwrap();
        wait_for_file(&srv.pidfile).await;
        drop(handle);
        client.cancel().await.unwrap();
        until("the stop ran", || !srv.cleanups.calls().is_empty()).await;
        assert_eq!(srv.cleanups.calls(), [StopCause::RequestCancelled]);
        let _ = tokio::time::timeout(Duration::from_secs(10), served).await;
        drained(&srv.sup).await;
        assert!(!group_alive(&srv.pidfile));
    }

    /// Cancelling a background start's request mid-launch doesn't stop the launch or the job
    /// it starts.
    #[tokio::test]
    async fn cancelling_a_background_start_keeps_the_job() {
        let dir = tempdir();
        let srv = server(&dir);
        let (client, _served) = connect(srv.clone()).await;
        let handle = client
            .peer()
            .send_cancellable_request(call("bg", ""), PeerRequestOptions::no_options())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        handle.cancel(Some("test".into())).await.unwrap();
        wait_for_file(&srv.pidfile).await;
        until("the job recorded itself", || group(&srv.pidfile).is_some()).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let alive = group_alive(&srv.pidfile);
        kill_group(&srv.pidfile, "-KILL");
        assert!(alive, "the background job was killed with its request");
        drained(&srv.sup).await;
        assert!(srv.returned.lock().unwrap()[0].starts_with("Done"));
    }
}

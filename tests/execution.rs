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

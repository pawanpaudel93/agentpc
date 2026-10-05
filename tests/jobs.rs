//! Tests of get_job_status's guest scripts (src/jobs.rs) that run them for real: the bash one
//! under local bash with GNU coreutils (any Linux; on a Mac, Homebrew's coreutils), and the
//! PowerShell one under pwsh when this machine has it. They stand in a temp HOME /
//! USERPROFILE for the VM and drive the same `assemble` the server uses.
//!
//! A test binary of its own, like tests/execution.rs: cargo runs binaries one after another,
//! so spawning processes here cannot disturb the lock tests in the main binary.

#[allow(dead_code)]
#[path = "../src/execution.rs"]
mod execution;
#[allow(dead_code)]
#[path = "../src/jobs.rs"]
mod jobs;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use jobs::*;

fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::SeqCst)
}

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        // Windows-style records are files named with backslashes next to the directory.
        if let (Some(parent), Some(name)) = (self.0.parent(), self.0.file_name()) {
            let prefix = format!("{}\\", name.to_string_lossy());
            if let Ok(rd) = std::fs::read_dir(parent) {
                for e in rd.flatten() {
                    if e.file_name().to_string_lossy().starts_with(&prefix) {
                        let _ = std::fs::remove_file(e.path());
                    }
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> TempDir {
    let d = std::env::temp_dir().join(format!(
        "agentpc-jobs-test-{}-{}",
        std::process::id(),
        unique()
    ));
    std::fs::create_dir_all(&d).unwrap();
    TempDir(d)
}

fn req(id: u64, cursor: Option<Resume>) -> Request {
    Request::new("vm", id, 50, Duration::ZERO, cursor, "gen".into())
}

fn query(cap: usize, resume: &Option<Resume>, r: &Request) -> Query {
    let mut q = r.query();
    q.cap = cap;
    let _ = resume;
    q
}

/// Run `status` against one reply with the given cap and return the assembled status.
fn at(
    cap: usize,
    id: u64,
    resume: Option<Resume>,
    run: &dyn Fn(u64, &Query) -> GuestReport,
) -> JobStatus {
    let mut r = req(id, resume.clone());
    r.cap = cap;
    let q = query(cap, &resume, &r);
    assemble(&r, &run(id, &q))
}

fn cursor_of(s: &JobStatus) -> Resume {
    parse_cursor(s.next_cursor.as_deref().expect("a cursor"), "vm", s.job_id).unwrap()
}

/// Read a whole log through cursors of `cap` bytes, as a client would, and return the
/// pieces. Stops when a reply has nothing more.
fn read_all(cap: usize, id: u64, run: &dyn Fn(u64, &Query) -> GuestReport) -> Vec<JobStatus> {
    let mut out = Vec::new();
    let mut resume = Resume::Start;
    for _ in 0..10_000 {
        let s = at(cap, id, Some(resume.clone()), run);
        resume = cursor_of(&s);
        let more = s.has_more;
        out.push(s);
        if !more {
            break;
        }
    }
    out
}

// =============================================================================================
// Linux

/// A directory standing for a Linux VM's home, and the environment its script needs.
struct Linux {
    home: TempDir,
    path: String,
    fd_proc: bool,
}

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

impl Linux {
    fn new() -> Option<Self> {
        let Some(path) = gnu_path() else {
            eprintln!("no GNU coreutils here; the bash fixtures are skipped");
            return None;
        };
        let home = tempdir();
        std::fs::create_dir_all(home.0.join("agentpc-bg")).unwrap();
        std::fs::write(home.0.join("boot_id"), "boot-aaaa\n").unwrap();
        Some(Self {
            home,
            path,
            fd_proc: Path::new("/proc/self/fd").exists(),
        })
    }

    fn bg(&self) -> PathBuf {
        self.home.0.join("agentpc-bg")
    }

    fn file(&self, id: u64, ext: &str) -> PathBuf {
        self.bg().join(format!("{id}.{ext}"))
    }

    fn write(&self, id: u64, ext: &str, bytes: &[u8]) {
        std::fs::write(self.file(id, ext), bytes).unwrap();
    }

    fn append(&self, id: u64, ext: &str, bytes: &[u8]) {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(self.file(id, ext))
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }

    fn boot(&self, v: &str) {
        std::fs::write(self.home.0.join("boot_id"), format!("{v}\n")).unwrap();
    }

    /// The script, with its descriptor path pointed at the log by name where /proc is not
    /// there (a Mac).
    fn script(&self, id: u64, q: &Query) -> String {
        let paths = LinuxPaths {
            fd: if self.fd_proc {
                "/proc/self/fd/3".to_string()
            } else {
                self.file(id, "log").to_string_lossy().into_owned()
            },
            boot_id: self.home.0.join("boot_id").to_string_lossy().into_owned(),
        };
        linux_script_with(id, q, &paths)
    }

    fn raw(&self, id: u64, q: &Query) -> std::process::Output {
        Command::new("bash")
            .args(["-c", &self.script(id, q)])
            .env("HOME", &self.home.0)
            .env("PATH", &self.path)
            .output()
            .unwrap()
    }

    fn run(&self, id: u64, q: &Query) -> GuestReport {
        let out = self.raw(id, q);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        parse_report(&out.stdout).unwrap_or_else(|e| {
            panic!("{e}: {}", String::from_utf8_lossy(&out.stdout));
        })
    }

    /// Names, sizes and modification times of everything in the VM's job directory.
    fn snapshot(&self) -> Vec<(String, u64, std::time::SystemTime)> {
        let mut v: Vec<_> = std::fs::read_dir(self.bg())
            .unwrap()
            .flatten()
            .map(|e| {
                let m = e.metadata().unwrap();
                (
                    e.file_name().to_string_lossy().into_owned(),
                    m.len(),
                    m.modified().unwrap(),
                )
            })
            .collect();
        v.sort();
        v
    }
}

/// A launched background job with a log, nothing recorded as running: queued (the state of
/// a record whose process this machine cannot see; /proc is not here on a Mac).
fn queued_job(l: &Linux, id: u64, log: &[u8]) {
    l.write(id, "sh", b"echo hi\n");
    l.write(id, "log", log);
}

#[test]
fn the_generated_linux_scripts_parse() {
    for mode in [
        Mode::Tail,
        Mode::Read(Expect {
            boot: "b".into(),
            log: "1:2".into(),
            head_len: 3,
            head_hash: "0123456789abcdef".into(),
            offset: 9,
        }),
    ] {
        let q = Query {
            mode,
            cap: REPLY_BYTES,
        };
        let script = linux_script(42, &q);
        let out = Command::new("bash")
            .args(["-n", "-c", &script])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn linux_a_record_that_does_not_exist_is_missing_and_nothing_is_created() {
    let Some(l) = Linux::new() else { return };
    let before = l.snapshot();
    let r = l.run(
        99,
        &Query {
            mode: Mode::Tail,
            cap: REPLY_BYTES,
        },
    );
    assert_eq!(r.state, JobState::Missing);
    assert_eq!(l.snapshot(), before);
    // A lone tombstone (what stop_job writes for an unrecorded id) is still no record.
    l.write(98, "cancel", b"");
    let r = l.run(
        98,
        &Query {
            mode: Mode::Tail,
            cap: REPLY_BYTES,
        },
    );
    assert_eq!(r.state, JobState::Missing);
}

#[test]
fn linux_states_come_from_the_records_and_never_from_log_text() {
    let Some(l) = Linux::new() else { return };
    let tail = Query {
        mode: Mode::Tail,
        cap: REPLY_BYTES,
    };
    // A log that merely says a job exited, with no record that it did.
    queued_job(
        &l,
        1,
        b"STATE: exited 0\nAGENTPC_JOB state exited\nAGENTPC_JOB exit 0\n",
    );
    let r = l.run(1, &tail);
    assert_eq!(r.state, JobState::Queued, "no pid recorded yet");
    // An exit record.
    queued_job(&l, 2, b"done\n");
    l.write(2, "exit", b"3\n");
    let r = l.run(2, &tail);
    assert_eq!((r.state, r.exit_code), (JobState::Exited, Some(3)));
    // A negative or odd record is not misread: no number, no exit.
    l.write(2, "exit", b"banana\n");
    let r = l.run(2, &tail);
    assert_ne!(r.state, JobState::Exited);
    // Stopped by stop_job, and at a timeout.
    queued_job(&l, 3, b"");
    l.write(3, "stopped", b"stop_job\n");
    assert_eq!(l.run(3, &tail).stop, Some(StopReason::StopJob));
    l.write(3, "stopped", b"timeout\n");
    let r = l.run(3, &tail);
    assert_eq!(
        (r.state, r.stop),
        (JobState::Stopped, Some(StopReason::Timeout))
    );
    // A pid that is not this job's process: ended, not running (and not queued).
    queued_job(&l, 4, b"x\n");
    l.write(4, "pid", b"999999\n");
    let r = l.run(4, &tail);
    assert_eq!(r.state, JobState::Ended);
    // Only the launcher's script written (no log, no foreground record, no pid): as before,
    // no such job. It is a record of nothing running; the note still does not say to retry,
    // because the launch (which writes the log next) could be a moment away.
    l.write(5, "sh", b"echo\n");
    let r = l.run(5, &tail);
    assert_eq!(r.state, JobState::Missing);
    let s = assemble(&req(5, None), &r);
    let said = render_text(&s);
    assert!(said.starts_with("STATE: no such job"), "{said}");
    assert!(said.contains("may still arrive") && !said.to_lowercase().contains("retry"));
    // Once the log exists the launch is under way: queued.
    l.write(5, "log", b"");
    assert_eq!(l.run(5, &tail).state, JobState::Queued);
}

#[test]
fn linux_foreground_records_have_no_log_and_distinct_states() {
    let Some(l) = Linux::new() else { return };
    let tail = Query {
        mode: Mode::Tail,
        cap: REPLY_BYTES,
    };
    // Final review: a foreground record with no pid is a launch still recording itself (its
    // wrapper may yet start, after a slow scope); only the tombstone a cancellation, probe or
    // stop_job writes first proves it never will.
    l.write(10, "fg", b"");
    let r = l.run(10, &tail);
    assert_eq!(
        (r.state, r.kind, r.log),
        (
            JobState::Queued,
            Some(JobKind::Foreground),
            LogPresence::None
        )
    );
    l.write(10, "cancel", b"");
    let r = l.run(10, &tail);
    assert_eq!(
        (r.state, r.kind, r.log),
        (
            JobState::NotStarted,
            Some(JobKind::Foreground),
            LogPresence::None
        )
    );
    let s = assemble(&req(10, None), &r);
    assert!(s.log_tail.is_none() && s.next_cursor.is_none() && s.note.is_some());
    l.write(10, "exit", b"0\n");
    assert_eq!(l.run(10, &tail).state, JobState::Exited);
    l.write(11, "fg", b"");
    l.write(11, "stopped", b"timeout");
    assert_eq!(l.run(11, &tail).stop, Some(StopReason::Timeout));
    // Stopped because its run_command request was cancelled: its own reason.
    l.write(14, "fg", b"");
    l.write(14, "pid", b"999999");
    l.write(14, "stopped", b"request_cancelled\n");
    let r = l.run(14, &tail);
    assert_eq!(
        (r.state, r.stop),
        (JobState::Stopped, Some(StopReason::RequestCancelled))
    );
    l.write(12, "fg", b"");
    l.write(12, "pid", b"999999");
    assert_eq!(l.run(12, &tail).state, JobState::Ended);
    // A wrapper that wrote its pid but not yet its proof, still alive: still starting, not
    // ended (the payload only runs after the proof).
    l.write(13, "fg", b"");
    l.write(13, "pid", std::process::id().to_string().as_bytes());
    assert_eq!(l.run(13, &tail).state, JobState::Queued);
}

#[test]
fn linux_reads_never_write_into_the_vm() {
    let Some(l) = Linux::new() else { return };
    queued_job(&l, 20, b"some output\nmore\n");
    l.write(21, "fg", b"");
    let before = l.snapshot();
    let tail = Query {
        mode: Mode::Tail,
        cap: REPLY_BYTES,
    };
    for id in [20, 21, 22] {
        l.run(id, &tail);
        l.run(
            id,
            &Query {
                mode: Mode::Read(Expect::default()),
                cap: 4,
            },
        );
    }
    assert_eq!(l.snapshot(), before, "no file created, changed or removed");
    assert!(!l.file(22, "cancel").exists() && !l.file(20, "cancel").exists());
}

#[test]
fn linux_tail_and_cursor_reads_continue_without_gaps_or_repeats() {
    let Some(l) = Linux::new() else { return };
    queued_job(&l, 30, b"alpha\nbeta\ngamma\n");
    let run = |id: u64, q: &Query| l.run(id, q);
    let first = at(REPLY_BYTES, 30, None, &run);
    assert_eq!(first.log_tail.as_deref(), Some("alpha\nbeta\ngamma\n"));
    assert!(first.log_available && !first.output_truncated);
    let cur = cursor_of(&first);
    // Nothing new: an empty read that leaves the position where it was.
    let again = at(REPLY_BYTES, 30, Some(cur.clone()), &run);
    assert_eq!(again.output.as_deref(), Some(""));
    assert_eq!(again.next_cursor, first.next_cursor);
    assert!(!again.cursor_reset);
    // Repeating the same cursor returns the same data (reads do not consume).
    l.append(30, "log", b"delta\n");
    let a = at(REPLY_BYTES, 30, Some(cur.clone()), &run);
    let b = at(REPLY_BYTES, 30, Some(cur), &run);
    assert_eq!(
        (a.output.as_deref(), a.next_cursor.clone()),
        (Some("delta\n"), b.next_cursor)
    );
    // Continuing from the new cursor gets only what came after.
    l.append(30, "log", b"epsilon\n");
    let c = at(REPLY_BYTES, 30, Some(cursor_of(&a)), &run);
    assert_eq!(c.output.as_deref(), Some("epsilon\n"));
    // tail_lines still works without a cursor.
    let mut r = req(30, None);
    r.tail_lines = 2;
    let t = assemble(&r, &run(30, &r.query()));
    assert_eq!(t.log_tail.as_deref(), Some("delta\nepsilon\n"));
}

#[test]
fn linux_a_small_cap_pages_through_multibyte_text_exactly() {
    let Some(l) = Linux::new() else { return };
    let text = "héllo wörld € euro 𝄞 clef ünïcode end\n".repeat(7);
    queued_job(&l, 31, text.as_bytes());
    let run = |id: u64, q: &Query| l.run(id, q);
    for cap in [4usize, 5, 7, 16, 64] {
        let pages = read_all(cap, 31, &run);
        let joined: String = pages.iter().map(|p| p.output.clone().unwrap()).collect();
        assert_eq!(joined, text, "cap {cap}");
        for p in &pages {
            assert!(
                p.output_bytes as usize <= cap,
                "cap {cap}: {}",
                p.output_bytes
            );
            assert!(!p.invalid_utf8 && !p.cursor_reset, "cap {cap}");
        }
        let bytes: u64 = pages.iter().map(|p| p.output_bytes).sum();
        assert_eq!(
            bytes as usize,
            text.len(),
            "cap {cap}: every byte exactly once"
        );
    }
    // The cursor carries across processes: parse it back from its string each time.
    let mut resume = Resume::Start;
    let mut got = String::new();
    for _ in 0..200 {
        let s = at(9, 31, Some(resume), &run);
        got.push_str(s.output.as_deref().unwrap());
        resume = parse_cursor(s.next_cursor.as_deref().unwrap(), "vm", 31).unwrap();
        if !s.has_more {
            break;
        }
    }
    assert_eq!(got, text);
}

#[test]
fn linux_bytes_that_are_not_text_come_through_honestly() {
    let Some(l) = Linux::new() else { return };
    let log = b"ok\0nul\n\xff\xfebad\xe2\x82";
    queued_job(&l, 32, log);
    let run = |id: u64, q: &Query| l.run(id, q);
    // The job is not over: the cut character at the very end is held back, the bad bytes
    // before it are replaced, and nothing is lost.
    let s = at(REPLY_BYTES, 32, Some(Resume::Start), &run);
    assert_eq!(s.output.as_deref(), Some("ok\0nul\n\u{fffd}\u{fffd}bad"));
    assert!(s.invalid_utf8);
    let Resume::At(c) = cursor_of(&s) else {
        panic!()
    };
    assert_eq!(c.offset as usize, log.len() - 2);
    // Once the job is over, the cut character is final.
    l.write(32, "exit", b"0");
    let s2 = at(REPLY_BYTES, 32, Some(cursor_of(&s)), &run);
    assert_eq!(s2.output.as_deref(), Some("\u{fffd}"));
    let Resume::At(c) = cursor_of(&s2) else {
        panic!()
    };
    assert_eq!(c.offset as usize, log.len());
    // A log of nothing but zero bytes.
    queued_job(&l, 33, &[0u8; 10]);
    let s = at(REPLY_BYTES, 33, Some(Resume::Start), &run);
    assert_eq!(s.output_bytes, 10);
    // An empty log gives an empty, valid, bound cursor.
    queued_job(&l, 34, b"");
    let s = at(REPLY_BYTES, 34, None, &run);
    assert_eq!(s.log_tail.as_deref(), Some(""));
    assert!(s.log_available);
    l.append(34, "log", b"first\n");
    let s2 = at(REPLY_BYTES, 34, Some(cursor_of(&s)), &run);
    assert_eq!(s2.output.as_deref(), Some("first\n"));
}

#[test]
fn linux_a_huge_log_is_capped_and_its_tail_marked() {
    let Some(l) = Linux::new() else { return };
    let line = "0123456789abcdef\n";
    let log = line.repeat(5000); // 85 KB
    queued_job(&l, 35, log.as_bytes());
    let run = |id: u64, q: &Query| l.run(id, q);
    let mut r = req(35, None);
    r.tail_lines = 100_000;
    let s = assemble(&r, &run(35, &r.query()));
    let tail = s.log_tail.unwrap();
    assert!(
        tail.len() <= REPLY_BYTES && tail.len() > REPLY_BYTES - 40,
        "{}",
        tail.len()
    );
    assert!(s.output_truncated, "lines were left out");
    assert!(tail.ends_with(line), "ends at the end of the log");
    assert!(tail.contains(&format!("\n{line}")), "mostly whole lines");
    let Resume::At(c) = parse_cursor(s.next_cursor.as_deref().unwrap(), "vm", 35).unwrap() else {
        panic!()
    };
    assert_eq!(
        c.offset as usize,
        log.len(),
        "the cursor is at the end of what was captured"
    );
    // From the start, the same log arrives in capped pieces that add up.
    let pages = read_all(REPLY_BYTES, 35, &run);
    assert!(pages.len() >= 6 && pages.iter().all(|p| p.output_bytes as usize <= REPLY_BYTES));
    let joined: String = pages.iter().map(|p| p.output.clone().unwrap()).collect();
    assert_eq!(joined, log);
    assert!(pages[0].has_more && !pages.last().unwrap().has_more);
}

#[test]
fn linux_a_log_that_shrinks_is_replaced_or_rewritten_resets_explicitly() {
    let Some(l) = Linux::new() else { return };
    let run = |id: u64, q: &Query| l.run(id, q);
    let original = "line one is long enough to fingerprint\nline two\nline three\n";
    // Shrunk: truncated and rewritten shorter.
    queued_job(&l, 40, original.as_bytes());
    let cur = cursor_of(&at(REPLY_BYTES, 40, None, &run));
    l.write(40, "log", b"fresh\n");
    let s = at(REPLY_BYTES, 40, Some(cur), &run);
    assert!(s.cursor_reset);
    assert_eq!(s.cursor_reset_reason, Some(ResetReason::LogShrunk));
    assert_eq!(s.output.as_deref(), Some("fresh\n"));
    let after = at(REPLY_BYTES, 40, Some(cursor_of(&s)), &run);
    assert!(!after.cursor_reset, "the new cursor follows the new log");

    // Truncated and rewritten in place with at least as much text: only the fingerprint of
    // its beginning shows it is another log.
    queued_job(&l, 41, original.as_bytes());
    let cur = cursor_of(&at(REPLY_BYTES, 41, None, &run));
    let rewrite = "LINE ONE IS LONG ENOUGH TO FINGERPRINT\nline two\nline three\nand more\n";
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(l.file(41, "log"))
        .unwrap();
    f.set_len(0).unwrap();
    f.write_all(rewrite.as_bytes()).unwrap();
    drop(f);
    let s = at(REPLY_BYTES, 41, Some(cur), &run);
    assert_eq!(s.cursor_reset_reason, Some(ResetReason::LogRewritten));
    assert_eq!(s.output.as_deref(), Some(rewrite));

    // Replaced by another file under the same name.
    queued_job(&l, 42, original.as_bytes());
    let cur = cursor_of(&at(REPLY_BYTES, 42, None, &run));
    let other = l.bg().join("42.other");
    std::fs::write(&other, format!("{original}appended elsewhere\n")).unwrap();
    std::fs::rename(&other, l.file(42, "log")).unwrap();
    let s = at(REPLY_BYTES, 42, Some(cur), &run);
    // /proc keeps the identity of the open file; by name (a Mac) the inode differs too.
    assert!(
        matches!(s.cursor_reset_reason, Some(ResetReason::LogReplaced)),
        "{:?}",
        s.cursor_reset_reason
    );
    assert!(s.output.as_deref().unwrap().starts_with("line one"));
}

#[test]
fn linux_a_log_that_vanishes_or_a_guest_that_rebooted_resets() {
    let Some(l) = Linux::new() else { return };
    let run = |id: u64, q: &Query| l.run(id, q);
    queued_job(&l, 50, b"abc\ndef\n");
    let cur = cursor_of(&at(REPLY_BYTES, 50, None, &run));
    // Reboot: same log, another boot id.
    l.boot("boot-bbbb");
    let s = at(REPLY_BYTES, 50, Some(cur.clone()), &run);
    assert_eq!(s.cursor_reset_reason, Some(ResetReason::Reboot));
    assert_eq!(s.output.as_deref(), Some("abc\ndef\n"));
    l.boot("boot-aaaa");
    // Not reset at all while nothing changed.
    assert!(!at(REPLY_BYTES, 50, Some(cur.clone()), &run).cursor_reset);
    // The log is deleted, the job record stays.
    std::fs::remove_file(l.file(50, "log")).unwrap();
    let s = at(REPLY_BYTES, 50, Some(cur), &run);
    assert_eq!(s.cursor_reset_reason, Some(ResetReason::LogMissing));
    assert_eq!(s.output.as_deref(), Some(""));
    // ... and the cursor it gives starts at the beginning of whatever log appears.
    l.write(50, "log", b"reborn\n");
    let s2 = at(REPLY_BYTES, 50, Some(cursor_of(&s)), &run);
    assert_eq!(s2.output.as_deref(), Some("reborn\n"));
    assert!(!s2.cursor_reset);
}

#[test]
fn linux_a_cursor_is_not_fooled_by_a_cursor_for_another_vm_generation() {
    let Some(l) = Linux::new() else { return };
    let run = |id: u64, q: &Query| l.run(id, q);
    queued_job(&l, 51, b"abc\ndef\n");
    let first = at(REPLY_BYTES, 51, None, &run);
    let Resume::At(mut c) = cursor_of(&first) else {
        panic!()
    };
    c.generation = "old-generation".into();
    let s = at(REPLY_BYTES, 51, Some(Resume::At(c)), &run);
    assert_eq!(s.cursor_reset_reason, Some(ResetReason::VmChanged));
    assert_eq!(s.output.as_deref(), Some("abc\ndef\n"));
}

#[tokio::test]
async fn linux_a_wait_follows_a_growing_log_with_no_loss_or_repeat() {
    let Some(l) = Linux::new() else { return };
    let l = std::sync::Arc::new(l);
    queued_job(&l, 60, b"");
    let writer = {
        let l = l.clone();
        tokio::task::spawn_blocking(move || {
            for i in 0..8 {
                std::thread::sleep(Duration::from_millis(60));
                l.append(60, "log", format!("line {i} é€\n").as_bytes());
            }
            l.write(60, "exit", b"0\n");
        })
    };
    let pacing = Pacing {
        poll: Duration::from_millis(20),
        transport: Duration::from_secs(2),
        ceiling: Duration::from_secs(30),
        grace: Duration::from_secs(2),
    };
    let mut resume = Resume::Start;
    let mut got = String::new();
    let mut waits = 0;
    let t = std::time::Instant::now();
    loop {
        let mut r = req(60, Some(resume.clone()));
        r.wait = Duration::from_secs(5);
        let l2 = l.clone();
        let s = observe(&r, pacing, move |q, _t| {
            let (l2, q) = (l2.clone(), q.clone());
            async move { Ok(l2.run(60, &q)) }
        })
        .await;
        waits += 1;
        got.push_str(s.output.as_deref().unwrap());
        resume = cursor_of(&s);
        assert!(
            !s.wait_timed_out,
            "something always arrived within the wait"
        );
        if s.state == JobState::Exited && !s.has_more {
            break;
        }
        assert!(waits < 100);
    }
    writer.await.unwrap();
    let want: String = (0..8).map(|i| format!("line {i} é€\n")).collect();
    assert_eq!(got, want);
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    assert!(
        waits > 2,
        "it returned as output arrived, not once at the end"
    );
}

#[tokio::test]
async fn linux_a_wait_on_a_running_job_expires_and_leaves_it_alone() {
    let Some(l) = Linux::new() else { return };
    let l = std::sync::Arc::new(l);
    queued_job(&l, 61, b"working\n");
    let before = l.snapshot();
    let pacing = Pacing {
        poll: Duration::from_millis(30),
        transport: Duration::from_secs(2),
        ceiling: Duration::from_secs(30),
        grace: Duration::from_secs(2),
    };
    let mut r = req(61, None);
    r.wait = Duration::from_millis(300);
    let l2 = l.clone();
    let t = std::time::Instant::now();
    let s = observe(&r, pacing, move |q, _t| {
        let (l2, q) = (l2.clone(), q.clone());
        async move { Ok(l2.run(61, &q)) }
    })
    .await;
    assert!(s.wait_timed_out);
    assert_eq!(s.state, JobState::Queued);
    assert_eq!(s.log_tail.as_deref(), Some("working\n"));
    let took = t.elapsed();
    assert!(
        took >= Duration::from_millis(290) && took < Duration::from_secs(3),
        "{took:?}"
    );
    assert_eq!(l.snapshot(), before, "waiting wrote nothing");
}

// =============================================================================================
// Windows (PowerShell), when this machine has pwsh

fn have_pwsh() -> bool {
    Command::new("pwsh")
        .args(["-NoProfile", "-Command", "exit 0"])
        .output()
        .is_ok()
}

#[test]
fn the_generated_windows_scripts_parse() {
    if !have_pwsh() {
        eprintln!("no pwsh here; skipped");
        return;
    }
    let parse = "$e = $null; $t = $null; \
        [void][System.Management.Automation.Language.Parser]::ParseInput($env:AGENTPC_PS, [ref]$t, [ref]$e); \
        if ($e.Count) { $e | ForEach-Object { $_.ToString() }; exit 1 }";
    for mode in [
        Mode::Tail,
        Mode::Read(Expect {
            boot: "w1".into(),
            log: "c1".into(),
            head_len: 3,
            head_hash: "0123456789abcdef".into(),
            offset: 9,
        }),
    ] {
        let script = windows_script(
            42,
            &Query {
                mode,
                cap: REPLY_BYTES,
            },
        );
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
    }
}

/// A directory standing for a Windows user profile. Off Windows a backslash is part of a
/// file name, so the records `$USERPROFILE\agentpc-bg\<id>.<ext>` are files named
/// `<profile>\agentpc-bg\<id>.<ext>` beside the profile directory.
struct Windows {
    home: TempDir,
    /// The scheduled-task stub: what Get-ScheduledTask returns for any job (a state name),
    /// or none.
    task: Option<&'static str>,
}

impl Windows {
    fn new() -> Option<Self> {
        if !have_pwsh() {
            eprintln!("no pwsh here; the PowerShell fixtures are skipped");
            return None;
        }
        let home = tempdir();
        Some(Self { home, task: None })
    }

    fn file(&self, id: u64, ext: &str) -> PathBuf {
        let profile = &self.home.0;
        profile.parent().unwrap().join(format!(
            "{}\\agentpc-bg\\{id}.{ext}",
            profile.file_name().unwrap().to_str().unwrap()
        ))
    }

    fn write(&self, id: u64, ext: &str, bytes: &[u8]) {
        std::fs::write(self.file(id, ext), bytes).unwrap();
    }

    fn append(&self, id: u64, ext: &str, bytes: &[u8]) {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(self.file(id, ext))
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }

    /// Get-ScheduledTask and its info exist only on Windows: stand-ins with the same shape.
    fn stub(&self) -> String {
        match self.task {
            None => "function Get-ScheduledTask { [CmdletBinding()] param($TaskName) $null }\n"
                .to_string(),
            Some(state) => format!(
                "function Get-ScheduledTask {{ [CmdletBinding()] param($TaskName) [pscustomobject]@{{ State = '{state}' }} }}\n\
                 function Get-ScheduledTaskInfo {{ param([Parameter(ValueFromPipeline)]$t) process {{ [pscustomobject]@{{ LastRunTime = [datetime]'1999-11-30' }} }} }}\n"
            ),
        }
    }

    fn run(&self, id: u64, q: &Query) -> GuestReport {
        let stub = self.stub();
        let out = Command::new("pwsh")
            .args(["-NoProfile", "-NonInteractive", "-Command"])
            .arg(format!("{stub}{}", windows_script(id, q)))
            .env("USERPROFILE", &self.home.0)
            .output()
            .unwrap();
        parse_report(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "{e}\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        })
    }

    fn snapshot(&self) -> Vec<(String, u64, std::time::SystemTime)> {
        let prefix = format!("{}\\", self.home.0.file_name().unwrap().to_str().unwrap());
        let mut v: Vec<_> = std::fs::read_dir(self.home.0.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
            .map(|e| {
                let m = e.metadata().unwrap();
                (
                    e.file_name().to_string_lossy().into_owned(),
                    m.len(),
                    m.modified().unwrap(),
                )
            })
            .collect();
        v.sort();
        v
    }
}

#[test]
fn windows_states_and_foreground_records() {
    let Some(w) = Windows::new() else { return };
    let tail = Query {
        mode: Mode::Tail,
        cap: REPLY_BYTES,
    };
    // Records need their directory (a prefix of file names here).
    let before = w.snapshot();
    assert_eq!(w.run(1, &tail).state, JobState::Missing);
    assert_eq!(w.snapshot(), before);
    // Foreground: a record with no pid is still starting unless a tombstone proves it never
    // will (final review).
    w.write(2, "fg", b"");
    let r = w.run(2, &tail);
    assert_eq!(
        (r.state, r.kind, r.log),
        (
            JobState::Queued,
            Some(JobKind::Foreground),
            LogPresence::None
        )
    );
    // Its pid record half written (the file exists, its text not yet): still starting.
    w.write(2, "pid", b"");
    assert_eq!(w.run(2, &tail).state, JobState::Queued);
    std::fs::remove_file(w.file(2, "pid")).unwrap();
    w.write(2, "cancel", b"");
    let r = w.run(2, &tail);
    assert_eq!(
        (r.state, r.kind, r.log),
        (
            JobState::NotStarted,
            Some(JobKind::Foreground),
            LogPresence::None
        )
    );
    w.write(2, "exit", b"-1");
    let r = w.run(2, &tail);
    assert_eq!(
        (r.state, r.exit_code),
        (JobState::Exited, Some(-1)),
        "the record"
    );
    // Status of a foreground handle says what run_command said: 255, not -1.
    assert_eq!(assemble(&req(2, None), &r).exit_code, Some(255));
    w.write(2, "exit", b"511");
    assert_eq!(
        assemble(&req(2, None), &w.run(2, &tail)).exit_code,
        Some(255)
    );
    w.write(3, "fg", b"");
    w.write(3, "stopped", b"timeout");
    assert_eq!(w.run(3, &tail).stop, Some(StopReason::Timeout));
    w.write(14, "fg", b"");
    w.write(14, "pid", b"999999 1");
    w.write(14, "stopped", b"request_cancelled");
    let r = w.run(14, &tail);
    assert_eq!(
        (r.state, r.stop),
        (JobState::Stopped, Some(StopReason::RequestCancelled))
    );
    w.write(4, "fg", b"");
    w.write(4, "pid", b"999999 1");
    assert_eq!(
        w.run(4, &tail).state,
        JobState::Ended,
        "a pid with no such process"
    );
    // Background: a log and an exit record; the exit text may carry a BOM and a newline.
    w.write(5, "log", b"\xef\xbb\xbfhello\r\nworld\r\n");
    w.write(5, "exit", b"\xef\xbb\xbf7\r\n");
    let r = w.run(5, &tail);
    assert_eq!(
        (r.state, r.exit_code, r.kind),
        (JobState::Exited, Some(7), Some(JobKind::Background))
    );
    let s = assemble(&req(5, None), &r);
    assert_eq!(
        s.log_tail.as_deref(),
        Some("hello\r\nworld\r\n"),
        "the BOM is not shown"
    );
    let Resume::At(c) = cursor_of(&s) else {
        panic!()
    };
    assert_eq!(c.offset, 3 + 14, "but it counts as consumed");
    // A record with no process and no task: ended.
    w.write(6, "log", b"x\r\n");
    assert_eq!(w.run(6, &tail).state, JobState::Ended);
}

#[test]
fn windows_queued_running_and_disabled_tasks() {
    let Some(mut w) = Windows::new() else { return };
    let tail = Query {
        mode: Mode::Tail,
        cap: REPLY_BYTES,
    };
    w.task = Some("Ready");
    // A task that has never run and no log yet: queued.
    let r = w.run(7, &tail);
    assert_eq!((r.state, r.log), (JobState::Queued, LogPresence::Absent));
    // With a log it has started.
    w.write(7, "log", b"started\r\n");
    assert_eq!(w.run(7, &tail).state, JobState::Running);
    w.task = Some("Running");
    assert_eq!(w.run(7, &tail).state, JobState::Running);
    // A disabled task is one stop_job cancelled.
    w.task = Some("Disabled");
    w.write(8, "log", b"x");
    w.write(8, "stopped", b"stop_job");
    assert_eq!(w.run(8, &tail).state, JobState::Stopped);
}

#[test]
fn windows_cursor_reads_continue_reset_and_never_write() {
    let Some(w) = Windows::new() else { return };
    let run = |id: u64, q: &Query| w.run(id, q);
    // Utf-8 with a BOM, CRLF, multibyte text; read in small pieces.
    let text = "héllo wörld € 𝄞 end\r\n".repeat(4);
    let mut log = b"\xef\xbb\xbf".to_vec();
    log.extend_from_slice(text.as_bytes());
    w.write(10, "log", &log);
    w.write(10, "exit", b"0");
    let before = w.snapshot();
    for cap in [5usize, 9, 64] {
        let pages = read_all(cap, 10, &run);
        let joined: String = pages.iter().map(|p| p.output.clone().unwrap()).collect();
        assert_eq!(joined, text, "cap {cap}");
        let bytes: u64 = pages.iter().map(|p| p.output_bytes).sum();
        assert_eq!(
            bytes as usize,
            log.len(),
            "cap {cap}: the BOM counts, nothing twice"
        );
    }
    assert_eq!(w.snapshot(), before, "reading wrote nothing");

    // Growth continues from the cursor.
    w.write(11, "log", b"one\r\n");
    w.write(11, "exit", b"0");
    let first = at(REPLY_BYTES, 11, None, &run);
    w.append(11, "log", b"two\r\n");
    let s = at(REPLY_BYTES, 11, Some(cursor_of(&first)), &run);
    assert_eq!(s.output.as_deref(), Some("two\r\n"));
    assert!(!s.cursor_reset);

    // Shrunk, rewritten in place, replaced, and rebooted.
    let cur = cursor_of(&first);
    w.write(11, "log", b"x\r\n");
    let s = at(REPLY_BYTES, 11, Some(cur), &run);
    assert_eq!(s.cursor_reset_reason, Some(ResetReason::LogShrunk));

    w.write(
        12,
        "log",
        b"first line of the log, long enough\r\nsecond\r\n",
    );
    w.write(12, "exit", b"0");
    let first = at(REPLY_BYTES, 12, None, &run);
    let cur = cursor_of(&first);
    w.write(
        12,
        "log",
        b"FIRST line of the log, long enough\r\nsecond\r\nthird, longer\r\n",
    );
    let s = at(REPLY_BYTES, 12, Some(cur.clone()), &run);
    assert!(
        matches!(
            s.cursor_reset_reason,
            Some(ResetReason::LogRewritten | ResetReason::LogReplaced)
        ),
        "{:?}",
        s.cursor_reset_reason
    );
    let Resume::At(mut c) = cur else { panic!() };
    c.boot = "w1".into(); // booted in 1970
    let s = at(REPLY_BYTES, 12, Some(Resume::At(c)), &run);
    assert_eq!(s.cursor_reset_reason, Some(ResetReason::Reboot));
    // The boot token of one boot is stable between calls (no reset from clock jitter).
    let first = at(REPLY_BYTES, 12, None, &run);
    let s = at(REPLY_BYTES, 12, Some(cursor_of(&first)), &run);
    assert!(!s.cursor_reset, "{:?}", s.cursor_reset_reason);
}

#[test]
fn linux_an_empty_log_then_an_append_is_read_exactly_once() {
    let Some(l) = Linux::new() else { return };
    let run = |id: u64, q: &Query| l.run(id, q);
    queued_job(&l, 70, b"");
    // The tail of an empty log: nothing, a bound cursor at 0, no fingerprint yet.
    let first = at(REPLY_BYTES, 70, None, &run);
    assert_eq!(first.log_tail.as_deref(), Some(""));
    assert!(first.log_available && !first.output_truncated);
    let Resume::At(c) = cursor_of(&first) else {
        panic!()
    };
    assert_eq!((c.offset, c.head_len, c.head_hash.as_str()), (0, 0, ""));
    assert!(!c.log.is_empty());
    // Still empty: same cursor, no reset.
    let again = at(REPLY_BYTES, 70, Some(cursor_of(&first)), &run);
    assert_eq!(
        (again.output.as_deref(), again.cursor_reset),
        (Some(""), false)
    );
    // Appended: exactly the new text, no reset, and the next read is empty again.
    l.append(70, "log", "héllo\nwörld\n".as_bytes());
    let s = at(REPLY_BYTES, 70, Some(cursor_of(&first)), &run);
    assert_eq!(s.output.as_deref(), Some("héllo\nwörld\n"));
    assert!(!s.cursor_reset);
    let s2 = at(REPLY_BYTES, 70, Some(cursor_of(&s)), &run);
    assert_eq!((s2.output.as_deref(), s2.cursor_reset), (Some(""), false));
    // The same from a "start" cursor and from an unbound one.
    queued_job(&l, 71, b"");
    let s = at(REPLY_BYTES, 71, Some(Resume::Start), &run);
    assert_eq!(s.output.as_deref(), Some(""));
    l.append(71, "log", b"later\n");
    let s = at(REPLY_BYTES, 71, Some(cursor_of(&s)), &run);
    assert_eq!(
        (s.output.as_deref(), s.cursor_reset),
        (Some("later\n"), false)
    );
}

#[test]
fn windows_an_empty_log_then_an_append_is_read_exactly_once() {
    let Some(w) = Windows::new() else { return };
    let run = |id: u64, q: &Query| w.run(id, q);
    w.write(72, "log", b"");
    w.write(72, "exit", b"0");
    let first = at(REPLY_BYTES, 72, None, &run);
    assert_eq!(first.log_tail.as_deref(), Some(""));
    let Resume::At(c) = cursor_of(&first) else {
        panic!()
    };
    assert_eq!((c.offset, c.head_len, c.head_hash.as_str()), (0, 0, ""));
    let again = at(REPLY_BYTES, 72, Some(cursor_of(&first)), &run);
    assert_eq!(
        (again.output.as_deref(), again.cursor_reset),
        (Some(""), false)
    );
    w.append(72, "log", b"\xef\xbb\xbfh\xc3\xa9llo\r\n");
    let s = at(REPLY_BYTES, 72, Some(cursor_of(&first)), &run);
    assert_eq!(
        s.output.as_deref(),
        Some("héllo\r\n"),
        "{:?}",
        s.cursor_reset_reason
    );
    assert!(!s.cursor_reset);
    let s2 = at(REPLY_BYTES, 72, Some(cursor_of(&s)), &run);
    assert_eq!((s2.output.as_deref(), s2.cursor_reset), (Some(""), false));
}

/// Cursor fields that look like the scripts' own placeholders parse and run as data.
#[test]
fn placeholder_like_cursor_fields_do_not_break_the_scripts() {
    let names = [
        "__OWN__",
        "__BOOT__",
        "__FD__",
        "__SECTION__",
        "__CAP__",
        "__ID__",
    ];
    let queries: Vec<Query> = names
        .iter()
        .map(|n| Query {
            mode: Mode::Read(Expect {
                boot: n.to_string(),
                log: n.to_string(),
                head_len: 1,
                head_hash: "0123456789abcdef".into(),
                offset: 3,
            }),
            cap: REPLY_BYTES,
        })
        .collect();
    for q in &queries {
        let script = linux_script(42, q);
        let out = Command::new("bash")
            .args(["-n", "-c", &script])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    if let Some(l) = Linux::new() {
        queued_job(&l, 73, b"abc\n");
        for q in &queries {
            let r = l.run(73, q);
            // Not the log it names, not this boot: a reset to the start, never a broken script.
            assert_eq!(r.state, JobState::Queued);
            assert!(r.reset.is_some());
            assert_eq!(r.data, b"abc\n");
        }
    }
    if let Some(w) = Windows::new() {
        w.write(73, "log", b"abc\r\n");
        w.write(73, "exit", b"0");
        for q in &queries {
            let r = w.run(73, q);
            assert_eq!(r.state, JobState::Exited);
            assert!(r.reset.is_some());
            assert_eq!(r.data, b"abc\r\n");
        }
    }
}

// =============================================================================================
// list_jobs: metadata-only, paginated discovery (src/jobs.rs, ListQuery and friends)

/// Stands in for a credential in files list_jobs must never read. Clearly not a real one.
const SYNTHETIC: &str = "SYNTHETIC-PLACEHOLDER-NOT-A-CREDENTIAL-7f3a91";

fn lq(limit: usize) -> ListQuery {
    ListQuery {
        limit,
        high_water: 0,
        before: 0,
        scan_max: LIST_SCAN_MAX,
        budget_secs: 20,
    }
}

fn lreq(limit: Option<u64>, cursor: Option<&str>) -> ListRequest {
    let mut r = ListRequest::new("vm", limit, cursor).unwrap_or_else(|e| panic!("{e:?}"));
    r.bind_generation("gen").unwrap_or_else(|e| panic!("{e:?}"));
    r
}

fn ids_of(g: &GuestList) -> Vec<u64> {
    g.entries.iter().map(|e| e.id).collect()
}

fn job_ids(l: &JobList) -> Vec<u64> {
    l.jobs.iter().map(|j| j.job_id).collect()
}

fn forged(v: serde_json::Value) -> String {
    base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        serde_json::to_vec(&v).unwrap(),
    )
}

const GOOD_REPORT: &str = "\
AGENTPC_LIST now 1760000000\n\
AGENTPC_LIST job 30\n\
AGENTPC_LIST recs fg,pid,exit\n\
AGENTPC_LIST kind foreground\n\
AGENTPC_LIST state exited\n\
AGENTPC_LIST exit 300\n\
AGENTPC_LIST log none\n\
AGENTPC_LIST at 1759999990\n\
AGENTPC_LIST done 1759999999\n\
AGENTPC_LIST job 20\n\
AGENTPC_LIST recs sh,log\n\
AGENTPC_LIST kind background\n\
AGENTPC_LIST state running\n\
AGENTPC_LIST log present\n\
AGENTPC_LIST count 2\n\
AGENTPC_LIST more 1\n\
AGENTPC_LIST scan 40 0\n\
AGENTPC_LIST end ok\n";

#[test]
fn list_limit_defaults_to_20_and_is_bounded_1_to_100() {
    assert_eq!(LIST_DEFAULT_LIMIT, 20);
    assert_eq!(LIST_MAX_LIMIT, 100);
    assert_eq!(validate_limit(None).unwrap(), 20);
    assert_eq!(validate_limit(Some(1)).unwrap(), 1);
    assert_eq!(validate_limit(Some(100)).unwrap(), 100);
    for bad in [0u64, 101, 1000, u64::MAX] {
        let e = validate_limit(Some(bad)).unwrap_err();
        assert_eq!(e.error_code, Some(ListErrorCode::InvalidLimit), "{bad}");
        assert!(e.jobs.is_empty() && e.next_cursor.is_none() && !e.has_more);
        let said = e.error.as_deref().unwrap();
        assert!(said.contains("between 1 and 100"), "{said}");
        assert!(said.contains(&bad.to_string()), "{said}");
    }
    // The request refuses it before anything else happens.
    let e = ListRequest::new("vm", Some(0), None).unwrap_err();
    assert_eq!(e.error_code, Some(ListErrorCode::InvalidLimit));
    assert_eq!(lreq(None, None).limit, 20);
}

#[test]
fn list_cursors_bind_vm_generation_and_position_and_reject_bad_ones_before_any_guest_work() {
    let c = ListCursor {
        vm: "vm".into(),
        generation: "gen".into(),
        high_water: 900,
        last: 500,
    };
    let s = c.encode();
    assert!(s.len() < 200, "{s}");
    assert_eq!(parse_list_cursor(&s, "vm").unwrap(), c);
    // Another VM's cursor names that VM.
    let e = parse_list_cursor(&s, "other").unwrap_err();
    assert!(
        e.contains("belongs to VM 'vm'") && e.contains("'other'"),
        "{e}"
    );
    // Malformed, forged, oversized, a get_job_status cursor: all refused, none parsed.
    let valid =
        |h: u64, l: u64| serde_json::json!({"v": 1, "vm": "vm", "g": "gen", "h": h, "l": l});
    assert!(parse_list_cursor(&forged(valid(900, 500)), "vm").is_ok());
    let mut bad: Vec<String> = vec![
        String::new(),
        "start".into(),
        "garbage".into(),
        "!!!".into(),
        "A".repeat(CURSOR_MAX_CHARS + 1),
        forged(valid(500, 900)), // last above the high-water
        forged(valid(900, 0)),   // nothing to continue below
        forged(valid(0, 0)),
        forged(serde_json::json!({"v": 2, "vm": "vm", "g": "gen", "h": 9, "l": 5})),
        forged(serde_json::json!({"v": 1, "vm": "", "g": "gen", "h": 9, "l": 5})),
        forged(serde_json::json!({"v": 1, "vm": "v m", "g": "gen", "h": 9, "l": 5})),
        forged(serde_json::json!({"v": 1, "vm": "vm", "g": "g g", "h": 9, "l": 5})),
        forged(serde_json::json!({"v": 1, "vm": "vm", "g": "x".repeat(65), "h": 9, "l": 5})),
        forged(serde_json::json!({"v": 1, "vm": "vm", "g": "gen", "h": 9, "l": 5, "x": 1})),
        forged(serde_json::json!({"v": 1, "vm": "vm", "g": "gen", "h": -9, "l": 5})),
        forged(serde_json::json!({"v": 1, "vm": "vm", "g": "gen", "h": "9", "l": 5})),
        forged(serde_json::json!({"v": 1, "vm": "vm", "g": "gen", "h": u64::MAX, "l": 5})),
        forged(serde_json::json!([1, 2, 3])),
    ];
    // A get_job_status cursor is a different shape and is not accepted here.
    bad.push(
        Cursor {
            vm: "vm".into(),
            generation: "gen".into(),
            job: 5,
            boot: "b".into(),
            log: String::new(),
            head_len: 0,
            head_hash: String::new(),
            offset: 0,
        }
        .encode(),
    );
    for b in &bad {
        let short: String = b.chars().take(20).collect();
        assert!(parse_list_cursor(b, "vm").is_err(), "accepted {short}");
        let e = ListRequest::new("vm", None, Some(b)).unwrap_err();
        assert_eq!(e.error_code, Some(ListErrorCode::InvalidCursor), "{short}");
    }
    // And the reverse: a list cursor is no get_job_status cursor.
    assert!(parse_cursor(&s, "vm", 5).is_err());
    // A cursor of another VM generation is stale, said so explicitly (never a silent restart).
    let mut r = ListRequest::new("vm", None, Some(&s)).unwrap();
    let e = r.bind_generation("other-generation").unwrap_err();
    assert_eq!(e.error_code, Some(ListErrorCode::CursorStale));
    let said = e.error.as_deref().unwrap();
    assert!(
        said.contains("reset") || said.contains("restored"),
        "{said}"
    );
    assert!(said.contains("without cursor"), "{said}");
    assert!(e.jobs.is_empty() && e.next_cursor.is_none());
    r.bind_generation("gen").unwrap();
    // Without a cursor any generation binds.
    assert!(
        ListRequest::new("vm", None, None)
            .unwrap()
            .bind_generation("whatever")
            .is_ok()
    );
}

/// A damaged exit record and an unreadable log together: the note says both (appended, not
/// replaced), and neither the state nor the exit code changes.
#[test]
fn an_invalid_exit_record_with_an_unreadable_log_keeps_both_notes() {
    let both = GuestReport {
        state: JobState::Unknown,
        kind: Some(JobKind::Background),
        log: LogPresence::Unreadable,
        ..GuestReport::default()
    };
    let s = assemble(&req(5, None), &both);
    assert_eq!((s.state, s.exit_code), (JobState::Unknown, None));
    let note = s.note.as_deref().unwrap();
    assert!(
        note.contains("not valid") && note.contains("not ended"),
        "{note}"
    );
    assert!(note.contains("could not be read just now"), "{note}");
    let text = render_text(&s);
    assert!(
        text.contains("not ended") && text.contains("could not be read"),
        "{text}"
    );
    // An unreadable log alone is worded as before.
    let alone = GuestReport {
        state: JobState::Running,
        kind: Some(JobKind::Background),
        log: LogPresence::Unreadable,
        ..GuestReport::default()
    };
    let s = assemble(&req(5, None), &alone);
    assert_eq!(
        s.note.as_deref(),
        Some("The job's log exists but could not be read just now.")
    );
    assert_eq!(s.state, JobState::Running);
}

/// A restored checkpoint can keep the jobs it was taken with, so a stale cursor must not claim
/// they are gone: it says the VM's generation changed, that the records may differ now, and
/// why the listing cannot safely continue. It still refuses (never silently restarts).
#[test]
fn a_stale_list_cursor_does_not_claim_the_jobs_are_gone() {
    let c = ListCursor {
        vm: "vm".into(),
        generation: "gen".into(),
        high_water: 9,
        last: 5,
    }
    .encode();
    let mut r = ListRequest::new("vm", None, Some(&c)).unwrap();
    let e = r.bind_generation("other-generation").unwrap_err();
    assert_eq!(e.error_code, Some(ListErrorCode::CursorStale));
    let said = e.error.as_deref().unwrap().to_lowercase();
    for claim in ["are gone", "were removed", "no longer exist", "all jobs"] {
        assert!(
            !said.contains(claim),
            "claims the records are gone ({claim}): {said}"
        );
    }
    assert!(
        said.contains("generation") && said.contains("changed"),
        "{said}"
    );
    assert!(said.contains("may differ"), "{said}");
    assert!(said.contains("cannot safely continue"), "{said}");
    assert!(said.contains("without cursor"), "{said}");
    // Validation is unchanged: still no jobs, no new cursor, and the matching generation binds.
    assert!(e.jobs.is_empty() && e.next_cursor.is_none());
    r.bind_generation("gen").unwrap();
}

#[test]
fn list_query_carries_only_numbers_the_guest_script_can_trust() {
    let r = lreq(Some(5), None);
    let q = r.query();
    assert_eq!((q.limit, q.high_water, q.before), (5, 0, 0));
    assert_eq!((q.scan_max, q.budget_secs), (LIST_SCAN_MAX, 20));
    let c = ListCursor {
        vm: "vm".into(),
        generation: "gen".into(),
        high_water: 900,
        last: 500,
    }
    .encode();
    let q = lreq(Some(5), Some(&c)).query();
    assert_eq!((q.high_water, q.before), (900, 500));
    // No text from the caller reaches a script: the scripts contain only these numbers.
    for script in [linux_list_script(&q), windows_list_script(&q)] {
        assert!(script.contains("900") && script.contains("500"));
        assert!(!script.contains("__"), "an unfilled placeholder:\n{script}");
    }
}

#[test]
fn list_report_parses_and_is_checked_against_its_query() {
    let q = lq(20);
    let g = parse_list_report(GOOD_REPORT.as_bytes(), &q).unwrap();
    assert_eq!(ids_of(&g), [30, 20]);
    assert!(g.more && !g.scan_truncated && !g.unreadable);
    assert_eq!((g.now, g.scanned), (Some(1760000000), 40));
    let e = &g.entries[0];
    assert_eq!(
        (e.kind, e.state, e.exit_code, e.log),
        (
            ListedKind::Foreground,
            JobState::Exited,
            Some(300),
            LogPresence::None
        )
    );
    assert_eq!(
        (e.recorded, e.finished),
        (Some(1759999990), Some(1759999999))
    );
    assert_eq!(
        e.records,
        [RecordKind::Fg, RecordKind::Pid, RecordKind::Exit]
    );
    assert_eq!(g.entries[1].log, LogPresence::Present);

    let mutate = |from: &str, to: &str| GOOD_REPORT.replacen(from, to, 1);
    let refused = [
        // cut off before its end
        mutate("AGENTPC_LIST end ok\n", ""),
        // a per-job fact before any job
        mutate(
            "AGENTPC_LIST now 1760000000\n",
            "AGENTPC_LIST state running\n",
        ),
        // ids not strictly descending, or repeated
        mutate("AGENTPC_LIST job 20\n", "AGENTPC_LIST job 30\n"),
        mutate("AGENTPC_LIST job 20\n", "AGENTPC_LIST job 40\n"),
        // the count does not match
        mutate("count 2", "count 3"),
        // junk in a fact
        mutate("state exited", "state banana"),
        mutate("kind foreground", "kind sideways"),
        mutate("log none", "log maybe"),
        mutate("recs fg,pid,exit", "recs fg,../../etc"),
        mutate("job 30", "job 30x"),
        mutate("at 1759999990", "at -4"),
        // a job with no state
        mutate("AGENTPC_LIST state running\n", ""),
        mutate("more 1", "more 2"),
    ];
    for (i, bad) in refused.iter().enumerate() {
        assert!(
            parse_list_report(bad.as_bytes(), &q).is_err(),
            "case {i} accepted"
        );
    }
    // More jobs than asked for, or outside the cursor's window, is a broken reply.
    assert!(parse_list_report(GOOD_REPORT.as_bytes(), &lq(1)).is_err());
    let mut windowed = lq(20);
    windowed.before = 30;
    assert!(parse_list_report(GOOD_REPORT.as_bytes(), &windowed).is_err());
    windowed.before = 31;
    windowed.high_water = 25;
    assert!(parse_list_report(GOOD_REPORT.as_bytes(), &windowed).is_err());
    // Only lines with the marker count; everything else is ignored.
    let noisy = format!("banner\n{GOOD_REPORT}trailing\n");
    assert!(parse_list_report(noisy.as_bytes(), &q).is_ok());
    // An unreadable directory is reported as such, not as an empty one.
    let g = parse_list_report(b"AGENTPC_LIST error unreadable\nAGENTPC_LIST end ok\n", &q).unwrap();
    assert!(g.unreadable && g.entries.is_empty());
}

#[test]
fn list_assembles_ids_kinds_states_and_only_known_timestamps() {
    let rep = parse_list_report(GOOD_REPORT.as_bytes(), &lq(2)).unwrap();
    let r = lreq(Some(2), None);
    let l = assemble_list(&r, &rep);
    assert_eq!(l.error, None);
    assert_eq!(job_ids(&l), [30, 20]);
    assert_eq!((l.limit, l.has_more, l.scan_truncated), (2, true, false));
    // A foreground exit code is the one run_command reports: the low 8 bits.
    let a = &l.jobs[0];
    assert_eq!(
        (a.kind, a.state, a.exit_code),
        (ListedKind::Foreground, JobState::Exited, Some(44))
    );
    assert!(!a.log_available && a.stop_reason.is_none());
    assert_eq!(a.recorded_at.as_deref(), Some("2025-10-09T08:53:10Z"));
    assert_eq!(a.finished_at.as_deref(), Some("2025-10-09T08:53:19Z"));
    assert_eq!(l.listed_at.as_deref(), Some("2025-10-09T08:53:20Z"));
    // A running background job: a log, and no timestamp it does not know.
    let b = &l.jobs[1];
    assert_eq!(
        (b.kind, b.state, b.exit_code),
        (ListedKind::Background, JobState::Running, None)
    );
    assert!(b.log_available);
    assert_eq!((&b.recorded_at, &b.finished_at), (&None, &None));
    // The next page continues below the last job shown, in the same VM generation, from the
    // same high-water mark (the first page's newest id).
    let next = l.next_cursor.as_deref().unwrap();
    let Ok(c) = parse_list_cursor(next, "vm") else {
        panic!()
    };
    assert_eq!(
        (c.generation.as_str(), c.high_water, c.last),
        ("gen", 30, 20)
    );
    // A later page keeps the original high-water mark.
    let again = lreq(Some(2), Some(next));
    let rep2 = parse_list_report(
        GOOD_REPORT
            .replace("job 30", "job 19")
            .replace("job 20", "job 18")
            .as_bytes(),
        &lq(2),
    )
    .unwrap();
    let l2 = assemble_list(&again, &rep2);
    let c2 = parse_list_cursor(l2.next_cursor.as_deref().unwrap(), "vm").unwrap();
    assert_eq!((c2.high_water, c2.last), (30, 18));
    // No more: no cursor, whatever the page.
    let mut done = rep.clone();
    done.more = false;
    let l = assemble_list(&r, &done);
    assert!((!l.has_more) && l.next_cursor.is_none());
    // The history is not a snapshot, and the reply says so.
    assert!(
        l.note.as_deref().unwrap().contains("not an immutable"),
        "{:?}",
        l.note
    );
    // An empty directory: nothing, no cursor.
    let none = GuestList::default();
    let l = assemble_list(&r, &none);
    assert!(l.jobs.is_empty() && l.next_cursor.is_none() && l.error.is_none());
    // "More" with nothing shown cannot be continued: a broken reply, never a loop.
    let stuck = GuestList {
        more: true,
        ..GuestList::default()
    };
    let l = assemble_list(&r, &stuck);
    assert_eq!(l.error_code, Some(ListErrorCode::BadGuestReply));
    // An unreadable directory is an error with its own code.
    let denied = GuestList {
        unreadable: true,
        ..GuestList::default()
    };
    let l = assemble_list(&r, &denied);
    assert_eq!(l.error_code, Some(ListErrorCode::GuestUnreadable));
}

#[test]
fn list_timestamps_are_utc_civil_dates() {
    assert_eq!(rfc3339(0).as_deref(), Some("1970-01-01T00:00:00Z"));
    assert_eq!(rfc3339(951782400).as_deref(), Some("2000-02-29T00:00:00Z"));
    assert_eq!(rfc3339(1760000000).as_deref(), Some("2025-10-09T08:53:20Z"));
    assert_eq!(rfc3339(4102444799).as_deref(), Some("2099-12-31T23:59:59Z"));
    assert_eq!(rfc3339(-1), None);
    assert_eq!(rfc3339(i64::MAX), None);
}

#[test]
fn list_notes_never_suggest_running_a_transitional_job_again() {
    let report = |state: &str, recs: &str, kind: &str| {
        format!(
            "AGENTPC_LIST now 1\nAGENTPC_LIST job 5\nAGENTPC_LIST recs {recs}\nAGENTPC_LIST kind {kind}\nAGENTPC_LIST state {state}\nAGENTPC_LIST log none\nAGENTPC_LIST count 1\nAGENTPC_LIST more 0\nAGENTPC_LIST scan 3 0\nAGENTPC_LIST end ok\n"
        )
    };
    let r = lreq(None, None);
    for (state, recs, kind) in [
        ("missing", "sh", "background"),
        ("missing", "cancel", "unknown"),
        ("queued", "fg", "foreground"),
        ("queued", "log", "background"),
        ("unknown", "fg,pid,exit", "foreground"),
        ("not_started", "fg,cancel", "foreground"),
        ("ended", "pid", "unknown"),
    ] {
        let rep = parse_list_report(report(state, recs, kind).as_bytes(), &lq(20)).unwrap();
        let l = assemble_list(&r, &rep);
        let note = l.jobs[0].note.clone().unwrap_or_default();
        let text = render_list_text(&l).to_lowercase();
        for banned in [
            "retry",
            "rerun",
            "run again",
            "run it again",
            "try again",
            "safe to",
        ] {
            assert!(
                !note.to_lowercase().contains(banned) && !text.contains(banned),
                "{state}/{kind}: {banned}: {text}"
            );
        }
        if matches!(state, "missing" | "queued" | "unknown") {
            assert!(!note.is_empty(), "{state} explains itself");
        }
        if state == "missing" || state == "queued" {
            assert!(note.contains("does not show"), "{note}");
        }
    }
    // An unknown kind says so.
    let rep = parse_list_report(report("ended", "pid", "unknown").as_bytes(), &lq(20)).unwrap();
    let l = assemble_list(&r, &rep);
    assert!(
        l.jobs[0]
            .note
            .as_deref()
            .unwrap()
            .contains("foreground or background")
    );
}

#[test]
fn list_text_is_readable_and_not_a_json_mirror() {
    let rep = parse_list_report(GOOD_REPORT.as_bytes(), &lq(2)).unwrap();
    let l = assemble_list(&lreq(Some(2), None), &rep);
    let text = render_list_text(&l);
    assert!(text.contains("30") && text.contains("20"), "{text}");
    assert!(
        text.contains("foreground") && text.contains("exited 44"),
        "{text}"
    );
    assert!(text.contains("running"), "{text}");
    assert!(text.contains(l.next_cursor.as_deref().unwrap()), "{text}");
    assert!(
        !text.contains("\"job_id\"") && !text.trim_start().starts_with('{'),
        "{text}"
    );
    let result = l.clone().into_list_result();
    assert_eq!(result.content.len(), 1, "one content block, no JSON mirror");
    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        result.structured_content.as_ref().unwrap()["jobs"][0]["job_id"],
        30
    );
    // An error: the text is the error, the call is a tool error, the structure carries a code.
    let e = validate_limit(Some(0)).unwrap_err();
    let result = e.into_list_result();
    assert_eq!(result.is_error, Some(true));
    let v = result.structured_content.unwrap();
    assert_eq!(v["error_code"], "invalid_limit");
    assert_eq!(v["jobs"], serde_json::json!([]));
    // An empty listing says so.
    let l = assemble_list(&lreq(None, None), &GuestList::default());
    assert!(
        render_list_text(&l).to_lowercase().contains("no jobs"),
        "{}",
        render_list_text(&l)
    );
}

#[test]
fn list_output_schema_declares_the_typed_result() {
    let s = list_output_schema();
    assert_eq!(s["type"], "object");
    for f in [
        "jobs",
        "limit",
        "has_more",
        "next_cursor",
        "listed_at",
        "scan_truncated",
        "note",
        "error",
        "error_code",
        "schema_version",
    ] {
        assert!(s["properties"].get(f).is_some(), "{f}");
    }
    let text = serde_json::to_string(&*s).unwrap();
    for f in [
        "job_id",
        "kind",
        "state",
        "exit_code",
        "log_available",
        "recorded_at",
        "finished_at",
        "stop_reason",
        "records",
    ] {
        assert!(text.contains(&format!("\"{f}\"")), "{f}");
    }
    // The error codes are an enum in the schema.
    for code in [
        "invalid_limit",
        "invalid_cursor",
        "cursor_stale",
        "request_cancelled",
        "guest_unreadable",
    ] {
        assert!(text.contains(code), "{code}");
    }
    // Nothing about commands, environments, or log contents is in the result.
    for banned in [
        "command",
        "environment",
        "stdout",
        "stderr",
        "log_tail",
        "payload",
        "script",
    ] {
        assert!(!text.contains(&format!("\"{banned}\"")), "{banned}");
    }
}

#[tokio::test]
async fn list_a_cancelled_request_stops_the_query_and_nothing_else() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    struct Guard(Arc<AtomicBool>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let r = lreq(None, None);
    // Already cancelled: the guest is never asked.
    let calls = Arc::new(AtomicUsize::new(0));
    let c2 = calls.clone();
    let l = list_observe(
        &r,
        move |_q, _t| {
            c2.fetch_add(1, Ordering::SeqCst);
            async { Ok(GuestList::default()) }
        },
        async {},
    )
    .await;
    assert_eq!(l.error_code, Some(ListErrorCode::RequestCancelled));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(l.jobs.is_empty() && l.next_cursor.is_none());
    // Cancelled while the query is under way: the query is dropped (its ssh dies with it).
    let dropped = Arc::new(AtomicBool::new(false));
    let started = Arc::new(tokio::sync::Notify::new());
    let go = Arc::new(tokio::sync::Notify::new());
    let (d2, s2) = (dropped.clone(), started.clone());
    let fetch = move |_q: &ListQuery, _t: Duration| {
        let guard = Guard(d2.clone());
        let s = s2.clone();
        async move {
            let _guard = guard;
            s.notify_one();
            std::future::pending::<Result<GuestList, Failure>>().await
        }
    };
    let cancel = {
        let go = go.clone();
        async move { go.notified().await }
    };
    let run = tokio::spawn({
        let r = r.clone();
        async move { list_observe(&r, fetch, cancel).await }
    });
    started.notified().await;
    assert!(!dropped.load(Ordering::SeqCst), "still under way");
    go.notify_one();
    let l = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(l.error_code, Some(ListErrorCode::RequestCancelled));
    assert!(
        dropped.load(Ordering::SeqCst),
        "the query future was dropped"
    );
    assert!(
        l.error.as_deref().unwrap().contains("No job was touched"),
        "{:?}",
        l.error
    );
}

#[tokio::test]
async fn list_a_query_that_hangs_ends_at_its_deadline_as_an_unreachable_guest() {
    let mut r = lreq(None, None);
    r.attempt = Duration::from_millis(60);
    r.grace = Duration::from_millis(40);
    let t = std::time::Instant::now();
    let l = list_observe(
        &r,
        |_q, _t| std::future::pending::<Result<GuestList, Failure>>(),
        std::future::pending::<()>(),
    )
    .await;
    assert!(t.elapsed() < Duration::from_secs(3));
    assert_eq!(l.error_code, Some(ListErrorCode::GuestUnreachable));
    // Failures from the transport keep their meaning.
    let l = list_observe(
        &r,
        |_q, _t| async { Err::<GuestList, _>(Failure::bad_reply("x")) },
        std::future::pending::<()>(),
    )
    .await;
    assert_eq!(l.error_code, Some(ListErrorCode::BadGuestReply));
    let l = list_observe(
        &r,
        |_q, _t| async { Ok(GuestList::default()) },
        std::future::pending::<()>(),
    )
    .await;
    assert!(l.error.is_none());
}

// ---- real bash -----------------------------------------------------------------------------

impl Linux {
    fn list_raw(&self, q: &ListQuery) -> std::process::Output {
        Command::new("bash")
            .args(["-c", &linux_list_script(q)])
            .env("HOME", &self.home.0)
            .env("PATH", &self.path)
            .output()
            .unwrap()
    }

    fn list(&self, q: &ListQuery) -> GuestList {
        let out = self.list_raw(q);
        parse_list_report(&out.stdout, q).unwrap_or_else(|e| {
            panic!(
                "{e}\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        })
    }

    /// One page the way the server builds it.
    fn page(&self, limit: u64, cursor: Option<&str>) -> JobList {
        let r = lreq(Some(limit), cursor);
        let rep = self.list(&r.query());
        assemble_list(&r, &rep)
    }

    fn touch(&self, id: u64, ext: &str, secs: u64) {
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(self.file(id, ext))
            .unwrap();
        f.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    }

    fn remove_all(&self, id: u64) {
        for ext in [
            "sh", "log", "fg", "pid", "proof", "exit", "stopped", "cancel",
        ] {
            let _ = std::fs::remove_file(self.file(id, ext));
        }
    }
}

#[test]
fn the_generated_linux_list_script_parses() {
    for q in [
        lq(1),
        lq(100),
        ListQuery {
            high_water: 9,
            before: 5,
            ..lq(20)
        },
    ] {
        let out = Command::new("bash")
            .args(["-n", "-c", &linux_list_script(&q)])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn linux_list_of_an_empty_or_absent_directory_is_empty_and_creates_nothing() {
    let Some(l) = Linux::new() else { return };
    let g = l.list(&lq(20));
    assert!(g.entries.is_empty() && !g.more && !g.unreadable && !g.scan_truncated);
    assert!(g.now.is_some());
    // Unrelated files and names that are no records of ours are not jobs.
    for name in [
        "notes.txt",
        "5",
        "5.log.1",
        "05x.log",
        "x5.log",
        "-5.log",
        "5.LOG",
        "5.unknown",
        "0.log",
        ".5.log",
        "5..log",
    ] {
        std::fs::write(l.bg().join(name), SYNTHETIC).unwrap();
    }
    std::fs::create_dir(l.bg().join("9.log.d")).unwrap();
    std::fs::create_dir(l.bg().join("11.fg")).unwrap();
    let g = l.list(&lq(20));
    // 11.fg is a directory named like a record: not a file, so not a record.
    assert!(g.entries.is_empty(), "{:?}", ids_of(&g));
    assert!(g.scanned >= 11);
    // A VM that never ran a job has no directory at all: empty, and still nothing created.
    let fresh = Linux::new().unwrap();
    std::fs::remove_dir(fresh.bg()).unwrap();
    let g = fresh.list(&lq(20));
    assert!(g.entries.is_empty() && !g.unreadable);
    assert!(!fresh.bg().exists(), "listing created the directory");
}

#[test]
fn linux_list_groups_every_record_of_a_job_once_in_newest_first_order() {
    let Some(l) = Linux::new() else { return };
    // One job with every known record; another with two; a third by a lone launch script.
    for ext in ["sh", "log", "pid", "proof", "exit"] {
        l.write(1000, ext, b"x");
    }
    l.write(2000, "fg", b"");
    l.write(2000, "exit", b"0");
    l.write(900, "sh", b"echo hi");
    // Names that are not canonical ids of these records never group with them.
    for name in [
        "01000.log",
        "1000.log.1",
        "1000",
        "2000.log.bak",
        "x1000.exit",
    ] {
        std::fs::write(l.bg().join(name), SYNTHETIC).unwrap();
    }
    let g = l.list(&lq(20));
    assert_eq!(
        ids_of(&g),
        [2000, 1000, 900],
        "newest numeric id first, each once"
    );
    assert_eq!(
        g.entries[1].records,
        [
            RecordKind::Sh,
            RecordKind::Log,
            RecordKind::Pid,
            RecordKind::Proof,
            RecordKind::Exit
        ]
    );
    assert_eq!(g.entries[0].records, [RecordKind::Fg, RecordKind::Exit]);
    // Numeric, not lexicographic: 100000 is newer than 99999 and 20.
    for id in [20u64, 99999, 100000, 7] {
        l.write(id, "fg", b"");
    }
    let g = l.list(&lq(100));
    assert_eq!(ids_of(&g), [100000, 99999, 2000, 1000, 900, 20, 7]);
    // Big ids (the real ones are about 1.7e15) sort and compare exactly.
    for id in [1760000000000001u64, 1760000000000003, 1760000000000002] {
        l.write(id, "fg", b"");
    }
    let g = l.list(&lq(3));
    assert_eq!(
        ids_of(&g),
        [1760000000000003, 1760000000000002, 1760000000000001]
    );
    assert!(g.more);
}

#[test]
fn linux_list_agrees_with_get_job_status_on_every_state() {
    let Some(l) = Linux::new() else { return };
    let tail = Query {
        mode: Mode::Tail,
        cap: REPLY_BYTES,
    };
    // Foreground.
    l.write(10, "fg", b""); // queued: recording itself
    l.write(11, "fg", b""); // not started: a tombstone before it ran
    l.write(11, "cancel", b"");
    l.write(12, "fg", b"");
    l.write(12, "exit", b"300\n"); // exited; run_command's low 8 bits
    l.write(13, "fg", b"");
    l.write(13, "pid", b"999999");
    l.write(13, "stopped", b"request_cancelled\n");
    l.write(14, "fg", b"");
    l.write(14, "stopped", b"timeout");
    l.write(15, "fg", b"");
    l.write(15, "pid", b"999999"); // a pid that is not running: ended
    l.write(16, "fg", b"");
    l.write(16, "stopped", b"stop_job");
    // Background.
    queued_job(&l, 20, b"out\n"); // log, no pid: queued
    queued_job(&l, 21, b"out\n");
    l.write(21, "exit", b"3\n"); // exited, own code
    queued_job(&l, 22, b"out\n");
    l.write(22, "exit", b"-1\n"); // exited, negative kept as is
    queued_job(&l, 23, b"");
    l.write(23, "pid", b"999999");
    l.write(23, "proof", b"boot-aaaa 12345\n"); // not this process any more: ended
    queued_job(&l, 24, b"");
    l.write(24, "stopped", b"stop_job\n");
    queued_job(&l, 25, b"");
    l.write(25, "exit", b"banana\n"); // not a number: not an exit
    // Odd records.
    l.write(30, "sh", b"echo\n"); // only the launch script
    l.write(31, "cancel", b""); // only a tombstone
    l.write(32, "pid", b"999999"); // only a pid
    l.write(33, "exit", b"7");
    l.write(34, "proof", b"b 1");

    let g = l.list(&lq(100));
    assert_eq!(g.entries.len(), 18);
    for e in &g.entries {
        let st = l.run(e.id, &tail);
        assert_eq!(e.state, st.state, "state of {}", e.id);
        assert_eq!(e.stop, st.stop, "stop reason of {}", e.id);
        assert_eq!(e.exit_code, st.exit_code, "exit of {}", e.id);
        assert_eq!(
            e.kind == ListedKind::Foreground,
            st.kind == Some(JobKind::Foreground),
            "foreground-ness of {}",
            e.id
        );
        if e.state != JobState::Missing {
            let want = if st.kind == Some(JobKind::Foreground) {
                LogPresence::None
            } else {
                st.log
            };
            assert_eq!(e.log, want, "log of {}", e.id);
        }
    }
    // The assembled view.
    let list = assemble_list(&lreq(Some(100), None), &g);
    let by = |id: u64| list.jobs.iter().find(|j| j.job_id == id).unwrap();
    assert_eq!(
        (by(10).state, by(10).kind),
        (JobState::Queued, ListedKind::Foreground)
    );
    assert_eq!(by(11).state, JobState::NotStarted);
    assert_eq!(
        (by(12).state, by(12).exit_code),
        (JobState::Exited, Some(44))
    );
    assert_eq!(
        (by(13).state, by(13).stop_reason),
        (JobState::Stopped, Some(StopReason::RequestCancelled))
    );
    assert_eq!(by(14).stop_reason, Some(StopReason::Timeout));
    assert_eq!(by(15).state, JobState::Ended);
    assert_eq!(by(16).stop_reason, Some(StopReason::StopJob));
    assert_eq!(
        (by(20).state, by(20).kind, by(20).log_available),
        (JobState::Queued, ListedKind::Background, true)
    );
    assert_eq!(
        (by(21).state, by(21).exit_code),
        (JobState::Exited, Some(3))
    );
    assert_eq!(by(22).exit_code, Some(-1));
    assert_eq!(by(23).state, JobState::Ended);
    assert_eq!(by(24).state, JobState::Stopped);
    assert_ne!(by(25).state, JobState::Exited);
    // Lone records: nothing has recorded itself, which is not proof it never started.
    assert_eq!(
        (by(30).state, by(30).kind),
        (JobState::Missing, ListedKind::Background)
    );
    assert_eq!(
        (by(31).state, by(31).kind),
        (JobState::Missing, ListedKind::Unknown)
    );
    assert_eq!(by(32).kind, ListedKind::Unknown);
    assert_eq!(
        (by(33).state, by(33).exit_code, by(33).kind),
        (JobState::Exited, Some(7), ListedKind::Unknown)
    );
    assert_eq!(by(34).state, JobState::Missing);
    // Missing and queued ones explain that; a failed job needs no such note.
    assert!(by(30).note.is_some() && by(10).note.is_some() && by(20).note.is_some());
}

#[test]
fn linux_list_paginates_a_stable_newest_first_traversal_under_additions_and_deletions() {
    let Some(l) = Linux::new() else { return };
    let all: Vec<u64> = (1..=25).map(|i| 5000 + i * 10).collect(); // 5010 ..= 5250
    for id in &all {
        l.write(*id, "fg", b"");
        l.write(*id, "exit", b"0");
    }
    let expect: Vec<u64> = all.iter().rev().copied().collect();
    // A quiet traversal: 10, 10, 5, no repeats, no gaps, cursors only while more remain.
    let p1 = l.page(10, None);
    assert_eq!(job_ids(&p1), expect[0..10]);
    assert!(p1.has_more);
    let p2 = l.page(10, p1.next_cursor.as_deref());
    assert_eq!(job_ids(&p2), expect[10..20]);
    let p3 = l.page(10, p2.next_cursor.as_deref());
    assert_eq!(job_ids(&p3), expect[20..25]);
    assert!(!p3.has_more && p3.next_cursor.is_none());
    // Exactly one full page is not "more".
    let exact = l.page(25, None);
    assert!(!exact.has_more && exact.next_cursor.is_none());
    assert!(l.page(24, None).has_more);

    // Under change: after the first page, a newer job appears and one already shown goes;
    // two not yet shown go, and an older one is added below the cursor.
    let p1 = l.page(10, None);
    l.write(9000, "fg", b""); // newer than the traversal began: never shown by it
    l.write(9000, "exit", b"0");
    l.remove_all(expect[3]); // already shown
    l.remove_all(expect[12]); // not yet shown: it vanishes
    l.remove_all(expect[14]);
    l.write(5005, "fg", b""); // below everything: it will be reached
    l.write(5005, "exit", b"0");
    let p2 = l.page(10, p1.next_cursor.as_deref());
    let p3 = l.page(10, p2.next_cursor.as_deref());
    let mut seen: Vec<u64> = job_ids(&p1);
    seen.extend(job_ids(&p2));
    seen.extend(job_ids(&p3));
    let mut want: Vec<u64> = expect.clone();
    want.retain(|id| *id != expect[12] && *id != expect[14]);
    want.push(5005);
    assert_eq!(
        seen, want,
        "no job twice, none skipped but the deleted ones; 9000 not shown"
    );
    assert!(!seen.contains(&9000));
    assert!(seen.windows(2).all(|w| w[0] > w[1]));
    // The new job is there for a fresh traversal.
    assert_eq!(job_ids(&l.page(1, None)), [9000]);
    // Every cursor binds the first page's high-water mark.
    let c = parse_list_cursor(p1.next_cursor.as_deref().unwrap(), "vm").unwrap();
    assert_eq!(c.high_water, expect[0]);
    let c2 = parse_list_cursor(p2.next_cursor.as_deref().unwrap(), "vm").unwrap();
    assert_eq!(c2.high_water, expect[0], "not moved by the newer job");
}

#[test]
fn linux_list_window_arguments_are_enforced_by_the_guest_too() {
    let Some(l) = Linux::new() else { return };
    for id in [10u64, 20, 30, 40, 50] {
        l.write(id, "fg", b"");
    }
    let q = ListQuery {
        high_water: 40,
        before: 40,
        ..lq(20)
    };
    assert_eq!(ids_of(&l.list(&q)), [30, 20, 10]);
    let q = ListQuery {
        high_water: 35,
        before: 0,
        ..lq(20)
    };
    assert_eq!(
        ids_of(&l.list(&q)),
        [30, 20, 10],
        "above the high-water mark is not shown"
    );
    let q = ListQuery {
        high_water: 0,
        before: 20,
        ..lq(20)
    };
    assert_eq!(ids_of(&l.list(&q)), [10]);
}

#[test]
fn linux_list_stops_at_its_time_budget_with_a_cursor_that_continues() {
    let Some(l) = Linux::new() else { return };
    for id in 1..=6u64 {
        l.write(id * 100, "fg", b"");
    }
    // A zero budget: one job a page (never an empty page), and the traversal still completes.
    let mut q = lq(100);
    q.budget_secs = 0;
    let first = l.list(&q);
    assert_eq!(ids_of(&first), [600]);
    assert!(first.more);
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..20 {
        let r = lreq(Some(100), cursor.as_deref());
        let mut q = r.query();
        q.budget_secs = 0;
        let rep = l.list(&q);
        let page = assemble_list(&r, &rep);
        seen.extend(job_ids(&page));
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(seen, [600, 500, 400, 300, 200, 100]);
}

#[test]
fn linux_list_bounds_its_enumeration_and_says_when_it_stopped_early() {
    let Some(l) = Linux::new() else { return };
    for id in 1..=30u64 {
        l.write(id, "fg", b"");
    }
    let mut q = lq(100);
    q.scan_max = 10;
    let g = l.list(&q);
    assert!(g.scan_truncated, "more entries than the scan cap");
    assert!(g.scanned > 10);
    assert!(
        g.entries.len() <= 10,
        "only what was scanned: {}",
        g.entries.len()
    );
    let page = assemble_list(&lreq(Some(100), None), &g);
    assert!(page.scan_truncated);
    assert!(
        page.note.as_deref().unwrap().contains("may leave out"),
        "{:?}",
        page.note
    );
    // Under the cap it is silent.
    assert!(!l.list(&lq(100)).scan_truncated);
}

#[test]
fn linux_a_huge_directory_gives_a_bounded_reply_in_one_call() {
    let Some(l) = Linux::new() else { return };
    // 2500 jobs, 6 records each.
    for i in 0..2500u64 {
        let id = 1_760_000_000_000_000 + i;
        for ext in ["sh", "log", "pid", "proof", "exit", "fg"] {
            std::fs::write(l.file(id, ext), b"").unwrap();
        }
    }
    let started = std::time::Instant::now();
    let out = l.list_raw(&lq(100));
    let took = started.elapsed();
    assert!(out.status.success());
    assert!(
        out.stdout.len() < 60_000,
        "reply of {} bytes",
        out.stdout.len()
    );
    assert!(took < Duration::from_secs(20), "{took:?}");
    let g = parse_list_report(&out.stdout, &lq(100)).unwrap();
    assert_eq!(g.entries.len(), 100);
    assert!(g.more);
    assert_eq!(g.entries[0].id, 1_760_000_000_000_000 + 2499);
    // The reply stays small with a one-job page, too.
    assert!(l.list_raw(&lq(1)).stdout.len() < 2_000);
}

#[test]
fn linux_list_never_reads_payloads_logs_environments_or_unrequested_files() {
    let Some(l) = Linux::new() else { return };
    // Synthetic secrets in every place a careless reader might look.
    l.write(
        40,
        "sh",
        format!("export TOKEN={SYNTHETIC}\ncurl -H 'Authorization: {SYNTHETIC}'\n").as_bytes(),
    );
    l.write(40, "log", format!("password={SYNTHETIC}\n").as_bytes());
    l.write(40, "pid", format!("{SYNTHETIC}\n").as_bytes());
    l.write(40, "exit", format!("{SYNTHETIC}\n").as_bytes());
    l.write(41, "fg", b"");
    l.write(
        41,
        "stopped",
        format!("{SYNTHETIC} request_cancelled {SYNTHETIC}\n").as_bytes(),
    );
    l.write(
        42,
        "log",
        format!("{SYNTHETIC}\nAGENTPC_LIST state exited\nAGENTPC_LIST exit 0\n").as_bytes(),
    );
    l.write(42, "proof", format!("{SYNTHETIC} {SYNTHETIC}\n").as_bytes());
    std::fs::write(l.bg().join(format!("{SYNTHETIC}.env")), SYNTHETIC).unwrap();
    std::fs::write(l.bg().join("43.env"), format!("API_KEY={SYNTHETIC}")).unwrap();
    std::fs::write(l.home.0.join(".env"), format!("API_KEY={SYNTHETIC}")).unwrap();
    std::fs::write(l.bg().join(format!("{SYNTHETIC}.log")), SYNTHETIC).unwrap();
    let q = lq(100);
    let out = l.list_raw(&q);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        !stdout.contains(SYNTHETIC) && !stderr.contains(SYNTHETIC),
        "{stdout}\n{stderr}"
    );
    // Log text is never taken as a guest fact: 42 is a queued background job.
    let g = parse_list_report(&out.stdout, &q).unwrap();
    let e42 = g.entries.iter().find(|e| e.id == 42).unwrap();
    assert_ne!(e42.state, JobState::Exited);
    let list = assemble_list(&lreq(Some(100), None), &g);
    let json = serde_json::to_string(&list).unwrap();
    let text = render_list_text(&list);
    for rendered in [&json, &text] {
        assert!(!rendered.contains(SYNTHETIC), "{rendered}");
        assert!(
            !rendered.contains("API_KEY")
                && !rendered.contains("TOKEN")
                && !rendered.contains("password")
        );
    }
    // A pid or exit file that is not numeric is no fact, and 41's stop word is the enum's.
    let by = |id: u64| list.jobs.iter().find(|j| j.job_id == id).unwrap();
    assert_ne!(by(40).state, JobState::Exited);
    assert_eq!(by(41).stop_reason, Some(StopReason::RequestCancelled));
    // Provably not opened: a launch script that is a pipe would hang any reader.
    let l2 = Linux::new().unwrap();
    let fifo = l2.file(50, "sh");
    let made = Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if made {
        l2.write(50, "fg", b"");
        let g = l2.list(&lq(20));
        assert_eq!(ids_of(&g), [50]);
        assert_eq!(g.entries[0].kind, ListedKind::Foreground);
    }
}

#[test]
fn linux_list_never_writes_into_the_vm() {
    let Some(l) = Linux::new() else { return };
    queued_job(&l, 60, b"some output\n");
    l.write(61, "fg", b""); // a launch that has not recorded itself: must not get a tombstone
    l.write(62, "fg", b"");
    l.write(62, "pid", b"999999");
    l.write(63, "cancel", b"");
    let before = l.snapshot();
    for q in [
        lq(1),
        lq(100),
        ListQuery {
            before: 62,
            high_water: 63,
            ..lq(5)
        },
    ] {
        l.list(&q);
    }
    assert_eq!(
        l.snapshot(),
        before,
        "no file created, changed, touched or removed"
    );
    assert!(!l.file(61, "cancel").exists() && !l.file(60, "cancel").exists());
    assert!(!l.file(61, "stopped").exists());
}

#[test]
fn linux_list_timestamps_are_file_system_metadata_and_only_when_known() {
    let Some(l) = Linux::new() else { return };
    l.write(70, "fg", b"");
    l.write(70, "exit", b"0");
    l.touch(70, "fg", 1_700_000_000);
    l.touch(70, "exit", 1_700_000_100);
    queued_job(&l, 71, b"x");
    l.write(71, "exit", b"1");
    l.touch(71, "sh", 1_700_000_200);
    l.touch(71, "exit", 1_700_000_300);
    l.write(72, "pid", b"999999"); // no launch record: no recorded_at
    l.write(73, "fg", b"");
    l.write(73, "stopped", b"timeout");
    l.touch(73, "stopped", 1_700_000_400);
    l.write(74, "fg", b""); // still going: no finished_at
    l.touch(74, "fg", 1_700_000_500);
    let g = l.list(&lq(100));
    let by = |id: u64| g.entries.iter().find(|e| e.id == id).unwrap();
    assert_eq!(
        (by(70).recorded, by(70).finished),
        (Some(1_700_000_000), Some(1_700_000_100))
    );
    assert_eq!(
        (by(71).recorded, by(71).finished),
        (Some(1_700_000_200), Some(1_700_000_300))
    );
    assert_eq!((by(72).recorded, by(72).finished), (None, None));
    assert_eq!(by(73).finished, Some(1_700_000_400));
    assert_eq!(
        (by(74).recorded, by(74).finished),
        (Some(1_700_000_500), None)
    );
    let list = assemble_list(&lreq(Some(100), None), &g);
    let j = list.jobs.iter().find(|j| j.job_id == 70).unwrap();
    assert_eq!(j.recorded_at.as_deref(), Some("2023-11-14T22:13:20Z"));
    assert_eq!(j.finished_at.as_deref(), Some("2023-11-14T22:15:00Z"));
    // Old records are old by their files, and the guest's clock comes with the reply.
    assert!(g.now.unwrap() > 1_700_000_500);
}

#[test]
fn linux_list_permission_failures_are_reported_not_hidden() {
    let Some(l) = Linux::new() else { return };
    l.write(80, "fg", b"");
    l.write(81, "fg", b"");
    l.write(81, "exit", b"0");
    use std::os::unix::fs::PermissionsExt;
    // A record that cannot be read: that job is unknown, the others are unaffected.
    std::fs::set_permissions(l.file(81, "exit"), std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(l.file(81, "exit")).is_err() {
        let g = l.list(&lq(20));
        let by = |id: u64| g.entries.iter().find(|e| e.id == id).unwrap();
        assert_eq!(by(81).state, JobState::Unknown);
        assert_eq!(by(80).state, JobState::Queued);
        let list = assemble_list(&lreq(None, None), &g);
        assert!(
            list.jobs
                .iter()
                .find(|j| j.job_id == 81)
                .unwrap()
                .note
                .is_some()
        );
        // An unknown job carries no exit code.
        assert_eq!(by(81).exit_code, None);
    }
    std::fs::set_permissions(l.file(81, "exit"), std::fs::Permissions::from_mode(0o644)).unwrap();
    // A job directory that cannot be listed: an error of its own, not an empty list.
    std::fs::set_permissions(l.bg(), std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read_dir(l.bg()).is_err() {
        let q = lq(20);
        let g = parse_list_report(&l.list_raw(&q).stdout, &q).unwrap();
        assert!(g.unreadable && g.entries.is_empty());
        let list = assemble_list(&lreq(None, None), &g);
        assert_eq!(list.error_code, Some(ListErrorCode::GuestUnreadable));
        assert!(
            !list
                .error
                .as_deref()
                .unwrap()
                .contains(&l.bg().to_string_lossy().into_owned())
        );
    }
    std::fs::set_permissions(l.bg(), std::fs::Permissions::from_mode(0o755)).unwrap();
}

// ---- real PowerShell -------------------------------------------------------------------

impl Windows {
    /// The names the job directory holds. Off Windows a "directory" is a prefix of file
    /// names beside the profile, so the lister is a different one; on Windows the script's
    /// own lists the directory.
    fn lister(&self) -> String {
        let profile = &self.home.0;
        let name = profile.file_name().unwrap().to_str().unwrap();
        let parent = profile.parent().unwrap().to_str().unwrap();
        format!(
            "$p = '{name}\\agentpc-bg\\'; [IO.Directory]::EnumerateFiles('{parent}') | ForEach-Object {{ [IO.Path]::GetFileName($_) }} | Where-Object {{ $_.StartsWith($p, [StringComparison]::Ordinal) }} | ForEach-Object {{ $_.Substring($p.Length) }}"
        )
    }

    fn list_raw(&self, q: &ListQuery) -> std::process::Output {
        Command::new("pwsh")
            .args(["-NoProfile", "-NonInteractive", "-Command"])
            .arg(format!(
                "{}{}",
                self.stub(),
                windows_list_script_with(q, &self.lister())
            ))
            .env("USERPROFILE", &self.home.0)
            .output()
            .unwrap()
    }

    fn list(&self, q: &ListQuery) -> GuestList {
        let out = self.list_raw(q);
        parse_list_report(&out.stdout, q).unwrap_or_else(|e| {
            panic!(
                "{e}\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        })
    }
}

#[test]
fn the_generated_windows_list_script_parses() {
    if !have_pwsh() {
        eprintln!("no pwsh here; skipped");
        return;
    }
    let parse = "$e = $null; $t = $null; \
        [void][System.Management.Automation.Language.Parser]::ParseInput($env:AGENTPC_PS, [ref]$t, [ref]$e); \
        if ($e.Count) { $e | ForEach-Object { $_.ToString() }; exit 1 }";
    for q in [
        lq(1),
        lq(100),
        ListQuery {
            high_water: 9,
            before: 5,
            ..lq(20)
        },
    ] {
        let script = windows_list_script(&q);
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
    }
}

#[test]
fn windows_list_agrees_with_get_job_status_and_groups_records() {
    let Some(w) = Windows::new() else { return };
    let tail = Query {
        mode: Mode::Tail,
        cap: REPLY_BYTES,
    };
    // Empty first: no records, no "directory".
    let g = w.list(&lq(20));
    assert!(g.entries.is_empty() && !g.more && !g.unreadable);
    assert!(g.now.is_some());
    w.write(10, "fg", b""); // queued
    w.write(11, "fg", b"");
    w.write(11, "cancel", b""); // not started
    w.write(12, "fg", b"");
    w.write(12, "exit", b"300"); // exited
    w.write(13, "fg", b"");
    w.write(13, "pid", b"1 1");
    w.write(13, "stopped", b"request_cancelled");
    w.write(20, "ps1", b"Write-Output hi");
    w.write(20, "log", b"hi\r\n");
    w.write(20, "exit", b"5");
    w.write(21, "log", b"x");
    w.write(21, "stopped", b"stop_job");
    w.write(22, "ps1", b"x"); // only the launch script, no task: nothing recorded
    w.write(23, "cancel", b"");
    let prefix = w.file(0, "x").to_string_lossy().replace("0.x", "");
    for name in ["notes.txt", "5.log.1", "05.log", "x5.log", "5.sh"] {
        std::fs::write(format!("{prefix}{name}"), SYNTHETIC).unwrap();
    }
    let g = w.list(&lq(100));
    assert_eq!(ids_of(&g), [23, 22, 21, 20, 13, 12, 11, 10]);
    for e in &g.entries {
        let st = w.run(e.id, &tail);
        assert_eq!(e.state, st.state, "state of {}", e.id);
        assert_eq!(e.stop, st.stop, "stop of {}", e.id);
        assert_eq!(
            e.kind == ListedKind::Foreground,
            st.kind == Some(JobKind::Foreground),
            "kind of {}",
            e.id
        );
    }
    let list = assemble_list(&lreq(Some(100), None), &g);
    let by = |id: u64| list.jobs.iter().find(|j| j.job_id == id).unwrap();
    assert_eq!(
        (by(12).state, by(12).exit_code),
        (JobState::Exited, Some(44))
    );
    assert_eq!(
        (by(20).state, by(20).exit_code, by(20).kind),
        (JobState::Exited, Some(5), ListedKind::Background)
    );
    assert!(by(20).log_available);
    assert_eq!(by(13).stop_reason, Some(StopReason::RequestCancelled));
    assert_eq!(by(22).state, JobState::Missing);
    assert_eq!(by(23).state, JobState::Missing);
    // Synthetic placeholder files that are not records of ours do not appear anywhere.
    let json = serde_json::to_string(&list).unwrap();
    assert!(!json.contains(SYNTHETIC) && !render_list_text(&list).contains(SYNTHETIC));
    // Nothing is written by listing.
    let snap = w.snapshot();
    w.list(&lq(100));
    assert_eq!(w.snapshot(), snap);
}

#[test]
fn windows_list_scheduled_tasks_decide_background_state_and_paginate() {
    let Some(mut w) = Windows::new() else { return };
    w.task = Some("Running");
    w.write(30, "ps1", b"x");
    w.write(31, "ps1", b"x");
    w.write(32, "ps1", b"x");
    let g = w.list(&lq(100));
    assert_eq!(ids_of(&g), [32, 31, 30]);
    assert!(
        g.entries
            .iter()
            .all(|e| e.state == JobState::Running && e.kind == ListedKind::Background)
    );
    assert!(
        g.entries[0].records.contains(&RecordKind::Task),
        "{:?}",
        g.entries[0].records
    );
    // A page of two, then the rest, through the real cursors.
    let r = lreq(Some(2), None);
    let page = assemble_list(&r, &w.list(&r.query()));
    assert_eq!(job_ids(&page), [32, 31]);
    assert!(page.has_more);
    let r2 = lreq(Some(2), page.next_cursor.as_deref());
    let page2 = assemble_list(&r2, &w.list(&r2.query()));
    assert_eq!(job_ids(&page2), [30]);
    assert!(!page2.has_more && page2.next_cursor.is_none());
    // A task that never ran with no log yet is queued, as get_job_status says.
    w.task = Some("Ready");
    let g = w.list(&lq(100));
    assert!(g.entries.iter().all(|e| e.state == JobState::Queued));
    // An exited job is exited whatever its task says (the task is not even asked).
    w.write(31, "exit", b"0");
    w.task = Some("Running");
    let g = w.list(&lq(100));
    assert_eq!(
        g.entries.iter().find(|e| e.id == 31).unwrap().state,
        JobState::Exited
    );
}

#[test]
fn windows_list_timestamps_and_bounds() {
    let Some(w) = Windows::new() else { return };
    w.write(40, "fg", b"");
    w.write(40, "exit", b"0");
    for (ext, secs) in [("fg", 1_700_000_000u64), ("exit", 1_700_000_100)] {
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(w.file(40, ext))
            .unwrap();
        f.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    }
    w.write(41, "pid", b"1 1");
    let g = w.list(&lq(100));
    let by = |id: u64| g.entries.iter().find(|e| e.id == id).unwrap();
    assert_eq!(
        (by(40).recorded, by(40).finished),
        (Some(1_700_000_000), Some(1_700_000_100))
    );
    assert_eq!((by(41).recorded, by(41).finished), (None, None));
    for id in 50..60u64 {
        w.write(id, "fg", b"");
    }
    let mut q = lq(100);
    q.scan_max = 5;
    let g = w.list(&q);
    assert!(g.scan_truncated && g.entries.len() <= 5);
    let mut q = lq(100);
    q.budget_secs = 0;
    let g = w.list(&q);
    assert_eq!(g.entries.len(), 1);
    assert!(g.more);
    let q = ListQuery {
        high_water: 55,
        before: 53,
        ..lq(100)
    };
    assert_eq!(ids_of(&w.list(&q)), [52, 51, 50, 41, 40]);
}

/// The list scripts have no way to read a payload or a log, or to write: every file they open
/// is a pid, an exit code, a stop word or a proof, and nothing in them creates, changes or
/// removes anything.
#[test]
fn list_scripts_read_only_the_few_facts_and_contain_no_way_to_write() {
    let q = lq(20);
    for (script, readers) in [
        (
            linux_list_script(&q),
            &["$(<$d/$id.pid)", "$(<$d/$id.exit)"][..],
        ),
        (
            windows_list_script(&q),
            &["ReadAllText(\"$d\\$id.exit\")"][..],
        ),
    ] {
        for banned in [
            "base64",
            "tail -c",
            "head -c",
            "sha256",
            "mkdir",
            "touch",
            "rm ",
            "kill",
            "chmod",
            "mv ",
            "tee",
            "> $d",
            ">> ",
            ": >",
            "ReadAllBytes",
            "FileStream",
            "Get-Content",
            "Set-Content",
            "Out-File",
            "WriteAll",
            "Remove-Item",
            "New-Item",
            "Register-",
            "Start-",
            "Stop-",
            "Disable-",
            "env\n",
            "printenv",
        ] {
            assert!(!script.contains(banned), "{banned} in:\n{script}");
        }
        for r in readers {
            assert!(script.contains(r), "{r}");
        }
        // Every read of a file's text is of one of those facts; none names a log or a script.
        for log in [
            ".log\")",
            ".sh\")",
            ".ps1\")",
            "<$d/$id.log",
            "<$d/$id.sh",
            "$d/$id.log |",
            "cat $d/$id.log",
        ] {
            assert!(!script.contains(log), "{log}");
        }
    }
    // The only `cat` is the process proof's boot id (shared with get_job_status).
    let linux = linux_list_script(&q);
    for (at, _) in linux.match_indices("cat ") {
        assert!(
            linux[at..].starts_with("cat /proc/sys/kernel/random/boot_id"),
            "{}",
            &linux[at..at + 40]
        );
    }
}

// =============================================================================================
// Review fix round 1: a malformed exit record degrades one job, never the page

/// Exit-record contents that are not a valid exit code: signs in the wrong place, no digits,
/// too long, outside the 32-bit range, non-ASCII digits, words.
const BAD_EXITS: &[&str] = &[
    "1-2",
    "-",
    "--",
    "-0-",
    "99999999999999999999",
    "00000000000005",
    "4294967296",
    "-4294967296",
    "\u{ff11}\u{ff12}\u{ff13}",
    "\u{0661}\u{0662}",
    "abc",
    "1e3",
    "+5",
];

/// The edges of the valid range.
const GOOD_EXITS: &[(&str, i64)] = &[
    ("0", 0),
    ("7", 7),
    ("-1", -1),
    ("255", 255),
    ("4294967295", 4294967295),
    ("-4294967295", -4294967295),
    ("0000000005", 5),
];

#[test]
fn a_malformed_exit_in_the_host_parser_degrades_that_job_only() {
    let q = lq(20);
    for bad in [
        "1-2",
        "-",
        "--",
        "99999999999999999999",
        "abc",
        "４２",
        "4294967296",
        "",
    ] {
        let rep = GOOD_REPORT.replacen("exit 300", &format!("exit {bad}"), 1);
        let g = parse_list_report(rep.as_bytes(), &q)
            .unwrap_or_else(|e| panic!("{bad:?} poisoned the page: {e}"));
        assert_eq!(ids_of(&g), [30, 20], "the page and its older job survive");
        assert!(g.more, "and it still continues");
        let e = &g.entries[0];
        assert_eq!((e.state, e.exit_code), (JobState::Unknown, None), "{bad:?}");
        assert_eq!(g.entries[1].state, JobState::Running, "older job untouched");
        let l = assemble_list(&lreq(Some(20), None), &g);
        assert!(l.error.is_none() && l.next_cursor.is_some(), "{bad:?}");
        let note = l.jobs[0].note.as_deref().unwrap();
        assert!(note.contains("exit") && note.contains("unknown"), "{note}");
        assert_eq!(l.jobs[0].exit_code, None);
    }
    // An exited job that reports no code at all degrades the same way.
    let rep = GOOD_REPORT.replacen("AGENTPC_LIST exit 300\n", "", 1);
    let g = parse_list_report(rep.as_bytes(), &q).unwrap();
    assert_eq!(
        (g.entries[0].state, g.entries[0].exit_code),
        (JobState::Unknown, None)
    );
    // The protocol's shape, ids, order and count bounds stay strict.
    for (from, to) in [
        ("AGENTPC_LIST end ok\n", ""),
        ("job 20", "job 30"),
        ("count 2", "count 3"),
        ("state exited", "state banana"),
        ("job 30", "job 30x"),
        ("at 1759999990", "at -4"),
    ] {
        assert!(
            parse_list_report(GOOD_REPORT.replacen(from, to, 1).as_bytes(), &q).is_err(),
            "{from} -> {to}"
        );
    }
    // Valid codes at the range's edges are kept.
    for (txt, n) in GOOD_EXITS {
        let rep = GOOD_REPORT.replacen("exit 300", &format!("exit {txt}"), 1);
        let g = parse_list_report(rep.as_bytes(), &q).unwrap();
        assert_eq!(g.entries[0].exit_code, Some(*n), "{txt}");
    }
}

#[test]
fn linux_a_malformed_exit_record_is_unknown_and_never_blocks_older_jobs() {
    let Some(l) = Linux::new() else { return };
    let tail = Query {
        mode: Mode::Tail,
        cap: REPLY_BYTES,
    };
    // The newest jobs have bad exit records; older, valid ones must stay reachable.
    let bad_ids: Vec<u64> = (0..BAD_EXITS.len() as u64).map(|i| 9000 + i).collect();
    for (id, txt) in bad_ids.iter().zip(BAD_EXITS) {
        l.write(*id, "fg", b"");
        l.write(*id, "exit", txt.as_bytes());
    }
    for (i, (txt, _)) in GOOD_EXITS.iter().enumerate() {
        let id = 500 + i as u64;
        l.write(id, "fg", b"");
        l.write(id, "exit", txt.as_bytes());
    }
    l.write(100, "fg", b"");
    l.write(100, "exit", b"0");
    let g = l.list(&lq(100));
    assert_eq!(g.entries.len(), BAD_EXITS.len() + GOOD_EXITS.len() + 1);
    for e in &g.entries {
        if bad_ids.contains(&e.id) {
            assert_eq!(
                (e.state, e.exit_code),
                (JobState::Unknown, None),
                "id {}",
                e.id
            );
        } else {
            assert_eq!(e.state, JobState::Exited, "id {}", e.id);
        }
    }
    for (i, (_, n)) in GOOD_EXITS.iter().enumerate() {
        let e = g.entries.iter().find(|e| e.id == 500 + i as u64).unwrap();
        // foreground codes are run_command's low 8 bits
        assert_eq!(e.exit_code, Some(*n), "{}", GOOD_EXITS[i].0);
    }
    // Paged through the real cursors, three at a time: every job is reached exactly once.
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..40 {
        let page = l.page(3, cursor.as_deref());
        assert!(page.error.is_none(), "{:?}", page.error);
        seen.extend(job_ids(&page));
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(seen.len(), g.entries.len());
    assert!(seen.windows(2).all(|w| w[0] > w[1]));
    assert!(seen.contains(&100));
    // get_job_status agrees: not exited, and not guessed to have ended.
    for id in &bad_ids {
        let st = l.run(*id, &tail);
        assert_eq!(
            (st.state, st.exit_code),
            (JobState::Unknown, None),
            "status of {id}"
        );
        let s = assemble(&req(*id, None), &st);
        assert!(s.error.is_none() && s.note.is_some(), "{id}");
        assert!(
            render_text(&s).starts_with("STATE: unknown"),
            "{}",
            render_text(&s)
        );
    }
    // A background job gets the same treatment from both.
    queued_job(&l, 77, b"x");
    l.write(77, "exit", b"1-2");
    assert_eq!(l.run(77, &tail).state, JobState::Unknown);
    assert_eq!(
        l.list(&lq(100))
            .entries
            .iter()
            .find(|e| e.id == 77)
            .unwrap()
            .state,
        JobState::Unknown
    );
}

#[test]
fn windows_a_malformed_exit_record_is_unknown_and_never_blocks_older_jobs() {
    let Some(w) = Windows::new() else { return };
    let tail = Query {
        mode: Mode::Tail,
        cap: REPLY_BYTES,
    };
    let bad_ids: Vec<u64> = (0..BAD_EXITS.len() as u64).map(|i| 9000 + i).collect();
    for (id, txt) in bad_ids.iter().zip(BAD_EXITS) {
        w.write(*id, "fg", b"");
        w.write(*id, "exit", txt.as_bytes());
    }
    for (i, (txt, _)) in GOOD_EXITS.iter().enumerate() {
        let id = 500 + i as u64;
        w.write(id, "fg", b"");
        w.write(id, "exit", txt.as_bytes());
    }
    w.write(100, "fg", b"");
    w.write(100, "exit", b"0");
    let g = w.list(&lq(100));
    assert_eq!(g.entries.len(), BAD_EXITS.len() + GOOD_EXITS.len() + 1);
    for e in &g.entries {
        if bad_ids.contains(&e.id) {
            assert_eq!(
                (e.state, e.exit_code),
                (JobState::Unknown, None),
                "id {}",
                e.id
            );
        } else {
            assert_eq!(e.state, JobState::Exited, "id {}", e.id);
        }
    }
    for (i, (_, n)) in GOOD_EXITS.iter().enumerate() {
        let e = g.entries.iter().find(|e| e.id == 500 + i as u64).unwrap();
        assert_eq!(e.exit_code, Some(*n), "{}", GOOD_EXITS[i].0);
    }
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..40 {
        let r = lreq(Some(3), cursor.as_deref());
        let page = assemble_list(&r, &w.list(&r.query()));
        assert!(page.error.is_none(), "{:?}", page.error);
        seen.extend(job_ids(&page));
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(seen.len(), g.entries.len());
    assert!(seen.contains(&100));
    for id in &bad_ids {
        let st = w.run(*id, &tail);
        assert_eq!(
            (st.state, st.exit_code),
            (JobState::Unknown, None),
            "status of {id}"
        );
    }
}

#[test]
fn an_empty_listing_never_looks_complete_when_it_is_not() {
    let q = lq(20);
    // Plain empty VM: as before.
    let l = assemble_list(&lreq(None, None), &GuestList::default());
    let t = render_list_text(&l);
    assert!(t.starts_with("No jobs recorded in this VM"), "{t}");
    assert!(
        t.contains("not an immutable"),
        "the global note is kept: {t}"
    );
    // Truncated scan with nothing found in what was kept.
    let rep = parse_list_report(
        b"AGENTPC_LIST now 1\nAGENTPC_LIST count 0\nAGENTPC_LIST more 0\nAGENTPC_LIST scan 20001 1\nAGENTPC_LIST end ok\n",
        &q,
    )
    .unwrap();
    let l = assemble_list(&lreq(None, None), &rep);
    assert!(l.jobs.is_empty() && l.scan_truncated);
    let t = render_list_text(&l);
    assert!(
        t.contains("retained") && t.contains("may leave out jobs"),
        "{t}"
    );
    assert!(!t.contains("No jobs recorded in this VM"), "{t}");
    assert!(!t.to_lowercase().starts_with("no jobs recorded"), "{t}");
    // A continuation emptied by deletions: no further jobs, not an empty VM.
    let c = ListCursor {
        vm: "vm".into(),
        generation: "gen".into(),
        high_water: 900,
        last: 500,
    }
    .encode();
    let r = lreq(None, Some(&c));
    let l = assemble_list(&r, &GuestList::default());
    assert!(l.continuation);
    let t = render_list_text(&l);
    assert!(
        t.starts_with("No further jobs below the last one shown"),
        "{t}"
    );
    assert!(!t.contains("No jobs recorded in this VM"), "{t}");
    assert!(
        t.contains("not an immutable") && t.contains("deleted"),
        "{t}"
    );
    assert!(
        lreq(None, None).cursor.is_none()
            && !assemble_list(&lreq(None, None), &GuestList::default()).continuation
    );
    // Both at once.
    let l = assemble_list(&r, &rep);
    let t = render_list_text(&l);
    assert!(
        t.starts_with("No further jobs")
            && t.contains("retained")
            && t.contains("may leave out jobs"),
        "{t}"
    );
    // The structure carries it too.
    let v = l.into_list_result().structured_content.unwrap();
    assert_eq!(
        (v["continuation"].clone(), v["scan_truncated"].clone()),
        (true.into(), true.into())
    );
}

/// A `ls` that really fails is an unreadable directory, not an empty success; the end of the
/// pipe that `head` closes on a long directory is not a failure.
#[test]
fn linux_list_reports_a_real_enumeration_failure_but_not_the_head_window() {
    let Some(l) = Linux::new() else { return };
    l.write(5, "fg", b"");
    // A stand-in `ls` that fails after printing some names.
    let fake = tempdir();
    let ls = fake.0.join("ls");
    std::fs::write(&ls, "#!/bin/sh\nprintf '.\\n..\\n5.fg\\n'\necho 'ls: reading directory: Input/output error' >&2\nexit 2\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&ls, std::fs::Permissions::from_mode(0o755)).unwrap();
    let q = lq(20);
    let out = Command::new("bash")
        .args(["-c", &linux_list_script(&q)])
        .env("HOME", &l.home.0)
        .env("PATH", format!("{}:{}", fake.0.display(), l.path))
        .output()
        .unwrap();
    let g = parse_list_report(&out.stdout, &q).unwrap();
    assert!(
        g.unreadable && g.entries.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let list = assemble_list(&lreq(None, None), &g);
    assert_eq!(list.error_code, Some(ListErrorCode::GuestUnreadable));
    // The error text never leaks the system's message or a path.
    assert!(!list.error.as_deref().unwrap().contains("Input/output"));
    // A directory far over the window: `head` closes the pipe (SIGPIPE), which is no error.
    for id in 1..=300u64 {
        l.write(id + 1000, "fg", b"");
    }
    let mut q = lq(20);
    q.scan_max = 50;
    let g = l.list(&q);
    assert!(!g.unreadable && g.scan_truncated && !g.entries.is_empty());
}

/// Fix round 2: the marker that tags `ls` errors must not be spellable as a file name, or a
/// file named like it would turn every page into `guest_unreadable`.
#[test]
fn linux_list_a_file_named_like_the_error_marker_is_not_an_error() {
    let Some(l) = Linux::new() else { return };
    l.write(300, "fg", b"");
    l.write(200, "fg", b"");
    for name in [
        "AGENTPC_LSERR x",
        "AGENTPC_LSERR ",
        "AGENTPC_LSERR ls: reading directory: Input/output error",
        "/AGENTPC_LSERR/ x",
    ] {
        // A name with a slash cannot exist; those that can are planted.
        if !name.contains('/') {
            std::fs::write(l.bg().join(name), b"").unwrap();
        }
    }
    let g = l.list(&lq(20));
    assert!(!g.unreadable, "a file name poisoned the page");
    assert_eq!(ids_of(&g), [300, 200]);
    assert_eq!(l.page(1, None).error, None);
    // The marker itself is not a possible file name.
    let script = linux_list_script(&lq(20));
    let marker = script
        .split('"')
        .find(|p| p.contains("LSERR") && p.ends_with(' '))
        .unwrap_or("");
    assert!(
        marker.contains('/'),
        "the marker must contain a slash: {marker:?}"
    );
}

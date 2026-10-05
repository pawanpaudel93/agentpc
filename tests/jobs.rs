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

    fn run(&self, id: u64, q: &Query) -> GuestReport {
        // Get-ScheduledTask and its info exist only on Windows: stand-ins with the same shape.
        let stub = match self.task {
            None => "function Get-ScheduledTask { [CmdletBinding()] param($TaskName) $null }\n"
                .to_string(),
            Some(state) => format!(
                "function Get-ScheduledTask {{ [CmdletBinding()] param($TaskName) [pscustomobject]@{{ State = '{state}' }} }}\n\
                 function Get-ScheduledTaskInfo {{ param([Parameter(ValueFromPipeline)]$t) process {{ [pscustomobject]@{{ LastRunTime = [datetime]'1999-11-30' }} }} }}\n"
            ),
        };
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

//! Runs the guest scripts in src/guest.rs for real: the bash ones under local bash with a temp
//! HOME, the PowerShell ones under pwsh (when this machine has it) with stand-ins for the
//! Windows-only cmdlets. A test binary of its own, like tests/jobs.rs: it spawns processes,
//! which must not run beside the lock tests in the main binary.

#[allow(dead_code)]
#[path = "../src/guest.rs"]
mod guest;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use guest::*;

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> TempDir {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let d = std::env::temp_dir().join(format!(
        "agentpc-guest-test-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&d).unwrap();
    TempDir(d)
}

/// `script` under bash with `home` as HOME.
fn bash(home: &Path, script: &str) -> Output {
    Command::new("bash")
        .args(["-c", script])
        .env("HOME", home)
        .output()
        .unwrap()
}

/// `pwsh` with an `XDG_CACHE_HOME` of its own (never the user's `~/.cache`), as in
/// tests/jobs.rs. Keep the dir until the child has ended.
fn pwsh_command() -> (Command, TempDir) {
    let cache = tempdir();
    let mut cmd = Command::new("pwsh");
    cmd.env("XDG_CACHE_HOME", &cache.0);
    (cmd, cache)
}

fn have_pwsh() -> bool {
    let (mut cmd, _cache) = pwsh_command();
    let ok = cmd
        .args(["-NoProfile", "-Command", "exit 0"])
        .output()
        .is_ok();
    if !ok {
        eprintln!("no pwsh here; skipped");
    }
    ok
}

const PARSE: &str = "$e = $null; $t = $null; \
    [void][System.Management.Automation.Language.Parser]::ParseInput($env:AGENTPC_PS, [ref]$t, [ref]$e); \
    if ($e.Count) { $e | ForEach-Object { $_.ToString() }; exit 1 }";

fn parses_in_pwsh(script: &str) {
    let (mut cmd, _cache) = pwsh_command();
    let out = cmd
        .args(["-NoProfile", "-NonInteractive", "-Command", PARSE])
        .env("AGENTPC_PS", script)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}\n{script}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// `stubs` then `script` then `tail`, under pwsh; (status ok, stdout).
fn pwsh_run(stubs: &str, script: &str, tail: &str, profile: Option<&Path>) -> (bool, String) {
    let (mut cmd, _cache) = pwsh_command();
    cmd.args(["-NoProfile", "-NonInteractive", "-Command"])
        .arg(format!("{stubs}\n{script}\n{tail}"));
    if let Some(p) = profile {
        cmd.env("USERPROFILE", p);
    }
    let out = cmd.output().unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn the_linux_flush_is_sync_and_parses() {
    assert_eq!(LINUX_FLUSH, "sync");
    let out = Command::new("bash")
        .args(["-n", "-c", LINUX_FLUSH])
        .output()
        .unwrap();
    assert!(out.status.success());
}

#[test]
fn the_windows_flush_parses() {
    if !have_pwsh() {
        return;
    }
    parses_in_pwsh(WINDOWS_FLUSH);
}

/// Get-Volume and Write-VolumeCache exist only on Windows: stand-ins with their shape. Volumes
/// C and D are fixed NTFS; E is removable, F is FAT32, and one fixed NTFS volume has no letter.
const VOLUME_STUBS: &str = r#"function Get-Volume { [CmdletBinding()] param() @(
  [pscustomobject]@{ DriveLetter = [char]'C'; DriveType = 'Fixed'; FileSystem = 'NTFS' },
  [pscustomobject]@{ DriveLetter = [char]'D'; DriveType = 'Fixed'; FileSystem = 'NTFS' },
  [pscustomobject]@{ DriveLetter = [char]'E'; DriveType = 'Removable'; FileSystem = 'NTFS' },
  [pscustomobject]@{ DriveLetter = [char]'F'; DriveType = 'Fixed'; FileSystem = 'FAT32' },
  [pscustomobject]@{ DriveLetter = $null; DriveType = 'Fixed'; FileSystem = 'NTFS' }) }
$global:flushed = @()
function Write-VolumeCache { [CmdletBinding()] param($DriveLetter) if ("$DriveLetter" -eq $global:failOn) { throw "flush of $DriveLetter failed" }; $global:flushed += "$DriveLetter" }"#;

#[test]
fn the_windows_flush_writes_every_fixed_ntfs_volume_and_nothing_else() {
    if !have_pwsh() {
        return;
    }
    let (ok, out) = pwsh_run(
        &format!("$global:failOn = ''\n{VOLUME_STUBS}"),
        WINDOWS_FLUSH,
        "\"FLUSHED $($global:flushed -join ',')\"",
        None,
    );
    assert!(ok, "{out}");
    assert!(out.contains("FLUSHED C,D"), "{out}");
}

#[test]
fn a_failing_windows_flush_exits_nonzero() {
    if !have_pwsh() {
        return;
    }
    let (ok, out) = pwsh_run(
        &format!("$global:failOn = 'D'\n{VOLUME_STUBS}"),
        WINDOWS_FLUSH,
        "\"FLUSHED $($global:flushed -join ',')\"",
        None,
    );
    assert!(!ok, "{out}");
    assert!(!out.contains("FLUSHED"), "{out}");
}

// =============================================================================================
// Job-record cleanup (create_vm from_checkpoint)

fn cleared(out: &str) -> bool {
    out.lines().any(|l| l.trim() == CLEARED)
}

#[test]
fn the_linux_cleanup_parses() {
    let out = Command::new("bash")
        .args(["-n", "-c", LINUX_CLEAR_RECORDS])
        .output()
        .unwrap();
    assert!(out.status.success());
}

#[test]
fn linux_cleanup_deletes_only_the_job_directory() {
    let t = tempdir();
    let home = t.0.join("home");
    let bg = home.join("agentpc-bg");
    std::fs::create_dir_all(bg.join("nested")).unwrap();
    for f in [
        "1700000000001.sh",
        "1700000000001.log",
        "1700000000002.fg",
        "nested/x",
    ] {
        std::fs::write(bg.join(f), "x").unwrap();
    }
    std::fs::write(home.join("keep.txt"), "mine").unwrap();
    std::fs::create_dir_all(home.join("agentpc-bg-other")).unwrap();
    let out = bash(&home, LINUX_CLEAR_RECORDS);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(cleared(&String::from_utf8_lossy(&out.stdout)));
    assert!(!bg.exists());
    assert!(home.join("keep.txt").is_file());
    assert!(home.join("agentpc-bg-other").is_dir());
    // Nothing left to delete: still cleared.
    let again = bash(&home, LINUX_CLEAR_RECORDS);
    assert!(again.status.success());
    assert!(cleared(&String::from_utf8_lossy(&again.stdout)));
}

#[test]
fn linux_cleanup_removes_a_symlink_without_following_it() {
    let t = tempdir();
    let home = t.0.join("home");
    let outside = t.0.join("outside");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("precious"), "x").unwrap();
    std::os::unix::fs::symlink(&outside, home.join("agentpc-bg")).unwrap();
    let out = bash(&home, LINUX_CLEAR_RECORDS);
    assert!(out.status.success());
    assert!(std::fs::symlink_metadata(home.join("agentpc-bg")).is_err());
    assert!(outside.join("precious").is_file());
}

#[test]
fn linux_cleanup_fails_when_it_cannot_delete() {
    use std::os::unix::fs::PermissionsExt;
    let uid = Command::new("id").arg("-u").output().unwrap();
    if String::from_utf8_lossy(&uid.stdout).trim() == "0" {
        eprintln!("running as root: permissions don't stop it; skipped");
        return;
    }
    let t = tempdir();
    let home = t.0.join("home");
    std::fs::create_dir_all(home.join("agentpc-bg")).unwrap();
    std::fs::write(home.join("agentpc-bg/1.log"), "x").unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o555)).unwrap();
    let out = bash(&home, LINUX_CLEAR_RECORDS);
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!out.status.success());
    assert!(!cleared(&String::from_utf8_lossy(&out.stdout)));
}

/// Get-ScheduledTask and Unregister-ScheduledTask exist only on Windows: stand-ins that keep a
/// task list (every task when called without -TaskName). Only `agentpc-bg-<digits>` (no leading
/// zero, this exact case) are agentpc's job tasks.
const TASK_STUBS: &str = r#"$global:tasks = [System.Collections.ArrayList]@('agentpc-bg-1700000000001', 'agentpc-bg-42', 'agentpc-bg-x', 'agentpc-bg-0', 'agentpc-bg-12-extra', 'AGENTPC-BG-43', 'UserTask')
function Get-ScheduledTask { [CmdletBinding()] param($TaskName) @($global:tasks) | Where-Object { -not $TaskName -or $_ -like $TaskName } | ForEach-Object { [pscustomobject]@{ TaskName = $_; TaskPath = '\' } } }
function Unregister-ScheduledTask { [CmdletBinding()] param($TaskName, $TaskPath, [switch]$Confirm) if (-not $global:stuck) { $global:tasks.Remove($TaskName) } }"#;

#[test]
fn the_windows_cleanup_parses() {
    if !have_pwsh() {
        return;
    }
    parses_in_pwsh(WINDOWS_CLEAR_RECORDS);
}

#[test]
fn windows_cleanup_deletes_the_job_directory_and_only_agentpc_job_tasks() {
    if !have_pwsh() {
        return;
    }
    let t = tempdir();
    let profile = t.0.join("p");
    std::fs::create_dir_all(&profile).unwrap();
    // Off Windows a backslash is part of a file name: "$USERPROFILE\agentpc-bg" is this sibling.
    let bg = t.0.join("p\\agentpc-bg");
    std::fs::create_dir_all(&bg).unwrap();
    std::fs::write(bg.join("42.ps1"), "x").unwrap();
    let keep = t.0.join("p\\keep.txt");
    std::fs::write(&keep, "mine").unwrap();
    let (ok, out) = pwsh_run(
        &format!("$global:stuck = $false\n{TASK_STUBS}"),
        WINDOWS_CLEAR_RECORDS,
        "\"LEFT $($global:tasks -join ',')\"",
        Some(&profile),
    );
    assert!(ok, "{out}");
    assert!(cleared(&out), "{out}");
    assert!(
        out.contains("LEFT agentpc-bg-x,agentpc-bg-0,agentpc-bg-12-extra,AGENTPC-BG-43,UserTask"),
        "{out}"
    );
    assert!(!bg.exists());
    assert!(keep.is_file());
}

#[test]
fn windows_cleanup_fails_when_a_job_task_stays() {
    if !have_pwsh() {
        return;
    }
    let t = tempdir();
    let profile = t.0.join("p");
    std::fs::create_dir_all(&profile).unwrap();
    let (ok, out) = pwsh_run(
        &format!("$global:stuck = $true\n{TASK_STUBS}"),
        WINDOWS_CLEAR_RECORDS,
        "",
        Some(&profile),
    );
    assert!(!ok, "{out}");
    assert!(!cleared(&out), "{out}");
}

#[test]
fn windows_cleanup_fails_when_it_cannot_list_the_tasks() {
    if !have_pwsh() {
        return;
    }
    let t = tempdir();
    let profile = t.0.join("p");
    std::fs::create_dir_all(&profile).unwrap();
    // Task Scheduler not available yet (a cold boot): listing fails, so nothing is proven gone.
    let stub = "function Get-ScheduledTask { [CmdletBinding()] param($TaskName) \
                Write-Error 'The Task Scheduler service is not available' }";
    let (ok, out) = pwsh_run(stub, WINDOWS_CLEAR_RECORDS, "", Some(&profile));
    assert!(!ok, "{out}");
    assert!(!cleared(&out), "{out}");
}

#[test]
fn windows_cleanup_fails_when_a_task_will_not_unregister() {
    if !have_pwsh() {
        return;
    }
    let t = tempdir();
    let profile = t.0.join("p");
    std::fs::create_dir_all(&profile).unwrap();
    // The later definition replaces the stand-in's Unregister-ScheduledTask.
    let stubs = format!(
        "$global:stuck = $false\n{TASK_STUBS}\n\
         function Unregister-ScheduledTask {{ [CmdletBinding()] param($TaskName, $TaskPath, [switch]$Confirm) \
         throw 'Access is denied' }}"
    );
    let (ok, out) = pwsh_run(&stubs, WINDOWS_CLEAR_RECORDS, "", Some(&profile));
    assert!(!ok, "{out}");
    assert!(!cleared(&out), "{out}");
}

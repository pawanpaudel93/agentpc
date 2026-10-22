//! Guest-side scripts for checkpoint_vm's flush and for create_vm(from_checkpoint)'s job-record
//! cleanup. No `crate::` dependencies, so tests/guest.rs runs them for real (bash, and pwsh when
//! this machine has it). The Windows ones are raw PowerShell 5.1: `ops::windows_command` wraps
//! them for the guest's SSH shell.

/// Bash: write the guest's pending data (page cache) to disk.
pub const LINUX_FLUSH: &str = "sync";

/// PowerShell 5.1: write the pending data of every fixed NTFS volume with a drive letter (the
/// system drive if none is listed) to disk: `Write-VolumeCache`, or where that cmdlet is
/// missing, `FlushFileBuffers` on the volume handle (Windows' documented way to flush a whole
/// volume). Exits nonzero if a volume could not be flushed.
pub const WINDOWS_FLUSH: &str = r#"$vols = @(Get-Volume -ErrorAction SilentlyContinue | Where-Object { $_.DriveType -eq 'Fixed' -and $_.FileSystem -eq 'NTFS' -and $_.DriveLetter })
if (-not $vols.Count) { $vols = @([pscustomobject]@{ DriveLetter = "$env:SystemDrive".TrimEnd(':') }) }
if (Get-Command Write-VolumeCache -ErrorAction SilentlyContinue) {
  foreach ($v in $vols) { Write-VolumeCache -DriveLetter $v.DriveLetter -ErrorAction Stop }
} else {
  Add-Type -TypeDefinition @"
using System; using System.Runtime.InteropServices;
public static class AgentpcFlush {
  [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
  static extern IntPtr CreateFileW(string name, uint access, uint share, IntPtr sa, uint disposition, uint flags, IntPtr template);
  [DllImport("kernel32.dll", SetLastError = true)]
  static extern bool FlushFileBuffers(IntPtr handle);
  [DllImport("kernel32.dll")]
  static extern bool CloseHandle(IntPtr handle);
  public static bool Volume(string letter) {
    IntPtr h = CreateFileW("\\\\.\\" + letter + ":", 0x40000000, 3, IntPtr.Zero, 3, 0, IntPtr.Zero);
    if (h == new IntPtr(-1)) return false;
    try { return FlushFileBuffers(h); } finally { CloseHandle(h); }
  }
}
"@
  foreach ($v in $vols) { if (-not [AgentpcFlush]::Volume("$($v.DriveLetter)")) { exit 1 } }
}"#;

/// The line both job-record scripts print once agentpc's job records are gone.
pub const CLEARED: &str = "AGENTPC_CLEARED";

/// Bash: delete agentpc's job records in a Linux guest, the directory `~/agentpc-bg` (the only
/// place list_jobs and get_job_status read; a job's systemd scope does not survive the cold
/// boot), and nothing else. A symlink there is removed, never followed. Prints CLEARED, or
/// exits 1.
pub const LINUX_CLEAR_RECORDS: &str = r#"d=~/agentpc-bg
rm -rf -- "$d" || exit 1
if [ -e "$d" ] || [ -L "$d" ]; then exit 1; fi
echo AGENTPC_CLEARED"#;

/// PowerShell 5.1: the same in a Windows guest: `%USERPROFILE%\agentpc-bg` and the scheduled
/// tasks background jobs ran as (`agentpc-bg-<id>`; get_job_status reads them too, and nothing
/// unregisters them when a job ends). `Directory.Delete` removes a junction there without
/// following it. Other tasks and files are left alone. Listing every task (no wildcard, so no
/// "no match" error to tell apart) must succeed: a Task Scheduler that can't answer yet, or a
/// task that won't unregister, fails the cleanup. Names match case-sensitively, to the end.
/// Prints CLEARED, or exits 1.
pub const WINDOWS_CLEAR_RECORDS: &str = r#"$d = "$env:USERPROFILE\agentpc-bg"
if ([IO.Directory]::Exists($d)) { [IO.Directory]::Delete($d, $true) }
elseif ([IO.File]::Exists($d)) { [IO.File]::Delete($d) }
if ([IO.Directory]::Exists($d) -or [IO.File]::Exists($d)) { exit 1 }
function AgentpcJobTasks { @(Get-ScheduledTask -ErrorAction Stop | Where-Object { $_.TaskName -cmatch '^agentpc-bg-[1-9][0-9]{0,19}\z' }) }
try {
  foreach ($t in (AgentpcJobTasks)) { Unregister-ScheduledTask -TaskName $t.TaskName -TaskPath $t.TaskPath -Confirm:$false -ErrorAction Stop }
  if ((AgentpcJobTasks).Count) { exit 1 }
} catch { exit 1 }
'AGENTPC_CLEARED'"#;

# Runs once, at the end of the unattended install (first logon).
$ErrorActionPreference = 'Continue'
Start-Transcript -Path C:\OEM\setup.log -Append

Set-ExecutionPolicy RemoteSigned -Scope LocalMachine -Force

# A locked or sleeping screen blocks UI automation.
powercfg /change standby-timeout-ac 0
powercfg /change monitor-timeout-ac 0
powercfg /hibernate off
New-Item -Path 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\Personalization' -Force | Out-Null
Set-ItemProperty -Path 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\Personalization' -Name NoLockScreen -Value 1 -Type DWord

# cua-driver first, so the desktop is drivable before the slow OpenSSH download.
# The installer registers a logon task that runs its `serve` daemon elevated in the
# interactive session (Session 0, where sshd runs, cannot see the desktop).
# Pinned: the skill and docs describe this version's tools. Bump deliberately.
$env:CUA_DRIVER_RS_VERSION = '0.30.1'
$ProgressPreference = 'SilentlyContinue'
# Its own process: the installer sets ErrorActionPreference=Stop and calls exit.
$installer = "$env:TEMP\cua-driver-install.ps1"
Invoke-WebRequest https://github.com/trycua/cua/releases/download/cua-driver-rs-v0.30.1/install.ps1 -OutFile $installer -UseBasicParsing
powershell.exe -NoProfile -ExecutionPolicy Bypass -File $installer
# The installer only registers the logon task; start it for this session too.
& "$env:LOCALAPPDATA\Programs\Cua\cua-driver\bin\cua-driver.exe" autostart kick

# OpenSSH with key auth and PowerShell as the default shell. The download takes 10-15 min.
Add-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0
New-ItemProperty -Path 'HKLM:\SOFTWARE\OpenSSH' -Name DefaultShell -Value 'C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe' -PropertyType String -Force
$keys = 'C:\ProgramData\ssh\administrators_authorized_keys'
New-Item -ItemType Directory -Path 'C:\ProgramData\ssh' -Force | Out-Null
Copy-Item C:\OEM\authorized_keys $keys -Force
icacls $keys /inheritance:r /grant 'Administrators:F' /grant 'SYSTEM:F'
Set-Service sshd -StartupType Automatic
Start-Service sshd
if (-not (Get-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' -ErrorAction SilentlyContinue)) {
    New-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' -DisplayName 'OpenSSH Server' -Direction Inbound -Protocol TCP -LocalPort 22 -Action Allow
}

Set-Content -Path C:\OEM\done.txt -Value (Get-Date -Format o)
Stop-Transcript

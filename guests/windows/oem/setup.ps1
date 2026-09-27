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

# cua-driver: the desktop-control server agents drive (over SSH, via its daemon's pipe).
# The installer registers a logon task that runs its `serve` daemon elevated in the
# interactive session (Session 0, where sshd runs, cannot see the desktop).
# Pinned: the skill and docs describe this version's tools. Bump deliberately.
$env:CUA_DRIVER_RS_VERSION = '0.30.1'
$ProgressPreference = 'SilentlyContinue'
# The installer imports a sibling _install-common.psm1 if present, else the latest from
# cua.ai; fetch the one from the same tag so builds are reproducible.
$dir = "$env:TEMP\cua-driver-install"
New-Item -ItemType Directory -Path $dir -Force | Out-Null
Invoke-WebRequest https://github.com/trycua/cua/releases/download/cua-driver-rs-v0.30.1/install.ps1 -OutFile "$dir\install.ps1" -UseBasicParsing
Invoke-WebRequest https://raw.githubusercontent.com/trycua/cua/cua-driver-rs-v0.30.1/libs/cua-driver/scripts/_install-common.psm1 -OutFile "$dir\_install-common.psm1" -UseBasicParsing
# Its own process: the installer sets ErrorActionPreference=Stop and calls exit.
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$dir\install.ps1"
$cua = "$env:LOCALAPPDATA\Programs\Cua\cua-driver\bin\cua-driver.exe"
if (Test-Path $cua) {
    # The installer only registers the logon task; start it for this session too.
    & $cua autostart kick
} else {
    Write-Error "cua-driver install failed (installer exit $LASTEXITCODE)"
}

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

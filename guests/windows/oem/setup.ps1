# Runs once, at the end of dockur's unattended install (first logon).
$ErrorActionPreference = 'Continue'
Start-Transcript -Path C:\OEM\setup.log -Append

Set-ExecutionPolicy RemoteSigned -Scope LocalMachine -Force

# A locked or sleeping screen blocks UI automation.
powercfg /change standby-timeout-ac 0
powercfg /change monitor-timeout-ac 0
powercfg /hibernate off
New-Item -Path 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\Personalization' -Force | Out-Null
Set-ItemProperty -Path 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\Personalization' -Name NoLockScreen -Value 1 -Type DWord

# uv and its tools live at machine scope so sshd sessions (system PATH only) see them.
$uv = 'C:\uv'
$vars = @{ UV_INSTALL_DIR = $uv; UV_TOOL_DIR = "$uv\tools"; UV_TOOL_BIN_DIR = "$uv\bin"; UV_PYTHON_INSTALL_DIR = "$uv\python" }
foreach ($k in $vars.Keys) {
    [Environment]::SetEnvironmentVariable($k, $vars[$k], 'Machine')
    Set-Item "env:$k" $vars[$k]
}
$machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
[Environment]::SetEnvironmentVariable('Path', "$machinePath;$uv;$uv\bin", 'Machine')
$env:Path += ";$uv;$uv\bin"

# Windows-MCP first, so the desktop is drivable before the slow OpenSSH download.
Invoke-RestMethod https://astral.sh/uv/install.ps1 | Invoke-Expression
& "$uv\uv.exe" tool install windows-mcp --python 3.13

# Must run in the logged-on user's interactive session (a service in Session 0
# cannot see the desktop); elevated so it can drive admin windows despite UIPI.
# No auth key: the guest sits behind QEMU user-mode NAT and is reachable only via
# the host's 127.0.0.1 port forwards.
$action = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -WindowStyle Hidden -Command `"& '$uv\bin\windows-mcp.exe' serve --transport streamable-http --host 0.0.0.0 --port 8000 --allow-insecure-remote *> C:\OEM\windows-mcp.log`""
$trigger = New-ScheduledTaskTrigger -AtLogOn
$principal = New-ScheduledTaskPrincipal -GroupId 'BUILTIN\Administrators' -RunLevel Highest
$settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) -AllowStartIfOnBatteries
Register-ScheduledTask -TaskName 'windows-mcp' -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Force
New-NetFirewallRule -DisplayName 'windows-mcp' -Direction Inbound -Protocol TCP -LocalPort 8000 -Action Allow
Start-ScheduledTask -TaskName 'windows-mcp'

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

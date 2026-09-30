# Runs once, at the end of the unattended install (first logon).
# Cosmetic steps run with errors reported but ignored; a failed critical step (Invoke-Critical)
# is logged, the remaining steps still run (so SSH and the log stay reachable), and then
# done.txt, agentpc's signal that the image is ready, is not written: C:\OEM\failed.txt lists
# what failed instead, and the script exits 1.
$ErrorActionPreference = 'Continue'
Start-Transcript -Path C:\OEM\setup.log -Append
$failed = [System.Collections.Generic.List[string]]::new()

# Runs $Body with every error terminating; a failure is logged and recorded in $failed.
# Native commands don't throw: check $LASTEXITCODE with Assert-Exit.
function Invoke-Critical([string]$What, [scriptblock]$Body) {
    $ErrorActionPreference = 'Stop'
    try {
        & $Body
    } catch {
        Write-Host "FAILED: ${What}: $_"
        $failed.Add($What)
    }
}
function Assert-Exit([string]$What) {
    if ($LASTEXITCODE -ne 0) { throw "$What exited $LASTEXITCODE" }
}

# Cosmetic: a machine-wide policy can override it, which is reported and ignored.
Set-ExecutionPolicy RemoteSigned -Scope LocalMachine -Force

# A locked or sleeping screen blocks UI automation.
Invoke-Critical 'screen and sleep settings' {
    powercfg /change standby-timeout-ac 0; Assert-Exit 'powercfg standby-timeout-ac'
    powercfg /change monitor-timeout-ac 0; Assert-Exit 'powercfg monitor-timeout-ac'
    New-Item -Path 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\Personalization' -Force | Out-Null
    Set-ItemProperty -Path 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\Personalization' -Name NoLockScreen -Value 1 -Type DWord
}
# Not critical: some virtual firmware has no hibernation to turn off.
powercfg /hibernate off

# cua-driver: the desktop-control server agents drive (over SSH, via its daemon's pipe).
# The installer registers a logon task that runs its `serve` daemon elevated in the
# interactive session (Session 0, where sshd runs, cannot see the desktop).
# Pinned: the skill and docs describe this version's tools. Bump deliberately, updating every
# sha256 below: the scripts' from the downloaded files, the binaries' from the release's
# cua-driver-rs-<version>-windows-arm64.zip (the installer doesn't check what it downloads,
# so the installed binaries are checked against it afterwards).
$cuaVersion = '0.30.3'
$env:CUA_DRIVER_RS_VERSION = $cuaVersion
# Telemetry off: VMs don't report to a third party.
$env:CUA_DRIVER_RS_TELEMETRY_ENABLED = '0'
$ProgressPreference = 'SilentlyContinue'
$cua = "$env:LOCALAPPDATA\Programs\Cua\cua-driver\bin\cua-driver.exe"
Invoke-Critical 'cua-driver install' {
    # The installer imports a sibling _install-common.psm1 if present, else the latest from
    # cua.ai; fetch the one from the same tag so builds are reproducible.
    $dir = "$env:TEMP\cua-driver-install"
    New-Item -ItemType Directory -Path $dir -Force | Out-Null
    $downloads = @(
        @{ Url = "https://github.com/trycua/cua/releases/download/cua-driver-rs-v$cuaVersion/install.ps1"
           File = 'install.ps1'
           Sha256 = 'eff52caa1a24a99f8a68ee6798df5be4128e360d1ca284beb0152e37b4bb088f' },
        @{ Url = "https://raw.githubusercontent.com/trycua/cua/cua-driver-rs-v$cuaVersion/libs/cua-driver/scripts/_install-common.psm1"
           File = '_install-common.psm1'
           Sha256 = '324bca98ad19f0487d4afd36a9e2d06478fcfb8e1e20225cdd8ec8ef5150e720' }
    )
    foreach ($d in $downloads) {
        $path = Join-Path $dir $d.File
        Invoke-WebRequest $d.Url -OutFile $path -UseBasicParsing
        $got = (Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash
        if ($got -ne $d.Sha256) { throw "$($d.File): sha256 $got, expected $($d.Sha256)" }
    }
    # Its own process: the installer sets ErrorActionPreference=Stop and calls exit.
    powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$dir\install.ps1"
    Assert-Exit 'cua-driver installer'
    # bin is a junction to the installed version's directory.
    $bin = Split-Path $cua
    $installed = @{
        'cua-driver.exe'       = '11b5e28784400d9dc67089ad9ba9ed7a3469aa11718d1f987444d2be4e8dce51'
        'cua-cursor-theme.exe' = 'd3a0f61c0f72df5e6747b9e6d7c7552bb4f69694556fa6a8f9c6ed3e14c26745'
        'cua-driver-uia.exe'   = '6052af93212ce0bff88bf3fa882a1dcdc232b9c45e5afec3e92199dc43c74fe0'
    }
    foreach ($f in $installed.Keys) {
        $got = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $bin $f)).Hash
        if ($got -ne $installed[$f]) { throw "installed ${f}: sha256 $got, expected $($installed[$f])" }
    }
    # 2>&1 of a native command makes error records; keep them as text.
    $ErrorActionPreference = 'Continue'
    $version = (& $cua --version 2>&1 | Out-String).Trim()
    Assert-Exit 'cua-driver --version'
    if ($version -notmatch "(^|[^0-9.])$([regex]::Escape($cuaVersion))([^0-9.]|$)") {
        throw "cua-driver --version reports '$version', expected $cuaVersion"
    }
    & $cua telemetry disable
    Assert-Exit 'cua-driver telemetry disable'
}
# The installer only registers the logon task; start it for this session too. Not critical:
# the task starts it at the next logon, and agentpc's readiness check waits for the daemon.
if (Test-Path $cua) {
    & $cua autostart kick
    if ($LASTEXITCODE -ne 0) { Write-Host "warning: cua-driver autostart kick exited $LASTEXITCODE" }
}

# OpenSSH with key auth and PowerShell as the default shell. The download takes 10-15 min.
Invoke-Critical 'OpenSSH server' {
    Add-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0 | Out-Null
    New-ItemProperty -Path 'HKLM:\SOFTWARE\OpenSSH' -Name DefaultShell -Value 'C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe' -PropertyType String -Force | Out-Null
    $keys = 'C:\ProgramData\ssh\administrators_authorized_keys'
    New-Item -ItemType Directory -Path 'C:\ProgramData\ssh' -Force | Out-Null
    Copy-Item C:\OEM\authorized_keys $keys -Force
    icacls $keys /inheritance:r /grant 'Administrators:F' /grant 'SYSTEM:F'
    Assert-Exit 'icacls'
    Set-Service sshd -StartupType Automatic
    Start-Service sshd
    if (-not (Get-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' -ErrorAction SilentlyContinue)) {
        New-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' -DisplayName 'OpenSSH Server' -Direction Inbound -Protocol TCP -LocalPort 22 -Action Allow | Out-Null
    }
}

if ($failed.Count -gt 0) {
    Set-Content -Path C:\OEM\failed.txt -Value ("setup failed: " + ($failed -join ', '))
    Write-Host "setup failed: $($failed -join ', ') (done.txt not written)"
    Stop-Transcript
    exit 1
}
Set-Content -Path C:\OEM\done.txt -Value (Get-Date -Format o)
Stop-Transcript

# Agent-friendly defaults, applied whenever an image's snapshot is captured. Idempotent.
# Cosmetic steps run with errors reported but ignored; a failed critical step (Invoke-Critical)
# is reported, the rest still run, and the script exits 1 so the capture fails.
$ErrorActionPreference = 'Continue'
$failed = [System.Collections.Generic.List[string]]::new()

# Runs $Body with every error terminating; a failure is reported and recorded in $failed.
# Native commands don't throw: check $LASTEXITCODE with Assert-Exit.
function Invoke-Critical([string]$What, [scriptblock]$Body) {
    $ErrorActionPreference = 'Stop'
    try {
        & $Body
    } catch {
        [Console]::Error.WriteLine("prepare: ${What} failed: $_")
        $failed.Add($What)
    }
}
function Assert-Exit([string]$What) {
    if ($LASTEXITCODE -ne 0) { throw "$What exited $LASTEXITCODE" }
}

function Set-Reg($Path, $Name, $Value) {
    # New-Item -Force on an existing key would recreate it and drop its other values.
    if (-not (Test-Path $Path)) { New-Item -Path $Path -Force | Out-Null }
    Set-ItemProperty -Path $Path -Name $Name -Value $Value -Type DWord
}

# SmartScreen blocks the unsigned installers an agent is often asked to test.
Invoke-Critical 'SmartScreen policy' {
    Set-Reg 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\System' 'EnableSmartScreen' 0
    Set-Reg 'HKLM:\SOFTWARE\Policies\Microsoft\Edge' 'SmartScreenEnabled' 0
}

# No update downloads or restarts in the middle of a task.
Invoke-Critical 'Windows Update policy and service' {
    Set-Reg 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU' 'NoAutoUpdate' 1
    # Already stopped is fine; what matters is that it stays disabled.
    Stop-Service wuauserv -Force -ErrorAction SilentlyContinue
    Set-Service wuauserv -StartupType Disabled
    $start = (Get-Service wuauserv).StartType
    if ($start -ne 'Disabled') { throw "wuauserv start type is $start" }
}

# Edge: no first-run wizard, sign-in or default-browser prompts.
Invoke-Critical 'Edge policy' {
    $edge = 'HKLM:\SOFTWARE\Policies\Microsoft\Edge'
    Set-Reg $edge 'HideFirstRunExperience' 1
    Set-Reg $edge 'BrowserSignin' 0
    Set-Reg $edge 'DefaultBrowserSettingEnabled' 0
}

# Tips, suggestions, "finish setting up" screens and OneDrive setup cover the UI.
# The per-user ones are cosmetic; the machine policies are critical.
$cdm = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\ContentDeliveryManager'
foreach ($n in 'SubscribedContent-310093Enabled', 'SubscribedContent-338389Enabled',
    'SubscribedContent-338393Enabled', 'SubscribedContent-353694Enabled',
    'SoftLandingEnabled', 'SystemPaneSuggestionsEnabled') {
    Set-Reg $cdm $n 0
}
Set-Reg 'HKCU:\Software\Microsoft\Windows\CurrentVersion\UserProfileEngagement' 'ScoobeSystemSettingEnabled' 0
Invoke-Critical 'consumer features and OneDrive policy' {
    Set-Reg 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\CloudContent' 'DisableWindowsConsumerFeatures' 1
    Set-Reg 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\OneDrive' 'DisableFileSyncNGSC' 1
}

# Toast notifications pop up over the windows the agent is working with. (Cosmetic.)
Set-Reg 'HKCU:\Software\Microsoft\Windows\CurrentVersion\PushNotifications' 'ToastEnabled' 0

# Notepad's first launch shows an "automatically saves your progress" tip over the text.
# The packaged app keeps its settings in its own hive; these are the values it writes when
# the tip is dismissed. Their types are WinRT ones reg add can't write, so import them.
$np = "$env:LOCALAPPDATA\Packages\Microsoft.WindowsNotepad_8wekyb3d8bbwe\Settings\settings.dat"
if ((Test-Path $np) -and -not (Get-Process Notepad -ErrorAction SilentlyContinue)) {
    Invoke-Critical 'Notepad settings import' {
        # reg.exe reports success on stderr: 2>&1 must not turn that into a terminating error.
        $ErrorActionPreference = 'Continue'
        $reg = "$env:TEMP\agentpc-notepad.reg"
        @'
Windows Registry Editor Version 5.00

[HKEY_USERS\agentpc-notepad\LocalState]
"TeachingTipVersion"=hex(5f5e105):00,00,00,00,f7,44,8f,39,67,4f,dd,01
"TeachingTipCheckCount"=hex(5f5e105):01,00,00,00,f7,44,8f,39,67,4f,dd,01
"TeachingTipExplicitClose"=hex(5f5e10b):01,85,69,e9,48,67,4f,dd,01
"RecentFilesFirstLoad"=hex(5f5e10b):00,f5,f8,48,39,67,4f,dd,01
'@ | Set-Content -Path $reg -Encoding Unicode -ErrorAction Stop
        try {
            reg load HKU\agentpc-notepad $np 2>&1 | Out-Null
            Assert-Exit 'reg load'
            try {
                reg import $reg 2>&1 | Out-Null
                Assert-Exit 'reg import'
            } finally {
                [gc]::Collect()
                reg unload HKU\agentpc-notepad 2>&1 | Out-Null
                if ($LASTEXITCODE -ne 0) { Write-Warning "reg unload exited $LASTEXITCODE" }
            }
        } finally {
            Remove-Item $reg -ErrorAction SilentlyContinue
        }
    }
}

if ($failed.Count -gt 0) {
    [Console]::Error.WriteLine("prepare: failed: $($failed -join ', ')")
    exit 1
}

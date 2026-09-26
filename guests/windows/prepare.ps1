# Agent-friendly defaults, applied whenever an image's snapshot is captured. Idempotent.
$ErrorActionPreference = 'Continue'

function Set-Reg($Path, $Name, $Value) {
    # New-Item -Force on an existing key would recreate it and drop its other values.
    if (-not (Test-Path $Path)) { New-Item -Path $Path -Force | Out-Null }
    Set-ItemProperty -Path $Path -Name $Name -Value $Value -Type DWord
}

# SmartScreen blocks the unsigned installers an agent is often asked to test.
Set-Reg 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\System' 'EnableSmartScreen' 0
Set-Reg 'HKLM:\SOFTWARE\Policies\Microsoft\Edge' 'SmartScreenEnabled' 0

# No update downloads or restarts in the middle of a task.
Set-Reg 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU' 'NoAutoUpdate' 1
Stop-Service wuauserv -Force -ErrorAction SilentlyContinue
Set-Service wuauserv -StartupType Disabled

# Edge: no first-run wizard, sign-in or default-browser prompts.
$edge = 'HKLM:\SOFTWARE\Policies\Microsoft\Edge'
Set-Reg $edge 'HideFirstRunExperience' 1
Set-Reg $edge 'BrowserSignin' 0
Set-Reg $edge 'DefaultBrowserSettingEnabled' 0

# Tips, suggestions, "finish setting up" screens and OneDrive setup cover the UI.
$cdm = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\ContentDeliveryManager'
foreach ($n in 'SubscribedContent-310093Enabled', 'SubscribedContent-338389Enabled',
    'SubscribedContent-338393Enabled', 'SubscribedContent-353694Enabled',
    'SoftLandingEnabled', 'SystemPaneSuggestionsEnabled') {
    Set-Reg $cdm $n 0
}
Set-Reg 'HKCU:\Software\Microsoft\Windows\CurrentVersion\UserProfileEngagement' 'ScoobeSystemSettingEnabled' 0
Set-Reg 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\CloudContent' 'DisableWindowsConsumerFeatures' 1
Set-Reg 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\OneDrive' 'DisableFileSyncNGSC' 1

# Toast notifications pop up over the windows the agent is working with.
Set-Reg 'HKCU:\Software\Microsoft\Windows\CurrentVersion\PushNotifications' 'ToastEnabled' 0

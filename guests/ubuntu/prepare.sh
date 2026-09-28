#!/bin/sh
# Agent-friendly defaults, applied (as root) whenever an image's snapshot is captured.
# Idempotent.
set -eu
export DEBIAN_FRONTEND=noninteractive

# Background apt runs would hold the dpkg lock right after a VM resumes, so an agent's
# `apt install` would fail.
systemctl disable --now unattended-upgrades.service apt-daily.timer apt-daily-upgrade.timer \
    >/dev/null 2>&1 || true

# cua-driver's browser tools drive Chromium-family browsers only, and Ubuntu's Chromium
# and Firefox packages are snaps. Google publishes an arm64 .deb.
if ! command -v google-chrome >/dev/null 2>&1; then
    curl -fsSL -o /tmp/chrome.deb \
        https://dl.google.com/linux/direct/google-chrome-stable_current_arm64.deb
    apt-get -o DPkg::Lock::Timeout=300 install -y -q /tmp/chrome.deb >/dev/null
    rm -f /tmp/chrome.deb
fi
# VMs are throwaway: no background browser updates.
rm -f /etc/apt/sources.list.d/google-chrome.list
apt-mark hold google-chrome-stable >/dev/null

# A new profile opens Chrome's Additional Terms of Service dialog, which blocks agents.
# The "First Run" marker skips first-run UI. (cua-driver's own isolated browsers pass
# --no-first-run themselves.)
install -d -o agent -g agent /home/agent/.config /home/agent/.config/google-chrome
[ -e "/home/agent/.config/google-chrome/First Run" ] ||
    install -o agent -g agent -m 644 /dev/null "/home/agent/.config/google-chrome/First Run"

# Chrome builds its accessibility tree only when told to, so without this get_window_state
# sees just the window frame, not the page. Every launcher (google-chrome,
# google-chrome-stable, the menu entries, xdg-open) goes through this script; the package
# is held, so the edit sticks. cua-driver's isolated browsers run the binary directly.
launcher=/opt/google/chrome/google-chrome
grep -q -- '--force-renderer-accessibility' "$launcher" ||
    sed -i 's|^exec -a "$0" "$HERE/chrome" "$@"$|exec -a "$0" "$HERE/chrome" --force-renderer-accessibility "$@"|' "$launcher"
grep -q -- '--force-renderer-accessibility' "$launcher"

mkdir -p /etc/opt/chrome/policies/managed
cat > /etc/opt/chrome/policies/managed/agentpc.json <<'EOF'
{
  "DefaultBrowserSettingEnabled": false,
  "BrowserSignin": 0,
  "SyncDisabled": true,
  "MetricsReportingEnabled": false,
  "PromotionalTabsEnabled": false,
  "PasswordManagerEnabled": false
}
EOF

# Smaller images: drop downloaded packages and old logs, then hand free blocks back to the
# qcow2 (the disk is attached with discard). Package lists stay so `apt install` just works.
apt-get clean
journalctl --vacuum-size=16M >/dev/null 2>&1 || true
fstrim -a >/dev/null 2>&1 || true

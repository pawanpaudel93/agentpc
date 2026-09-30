#!/bin/sh
# Agent-friendly defaults, applied (as root) whenever an image's snapshot is captured.
# Idempotent.
set -eu

# Background timers would hold the pacman lock (or refresh keys and databases) right after
# a VM resumes, so an agent's `pacman -S` would fail. One at a time: a unit this image
# doesn't have must not stop the others.
for unit in pacman-filesdb-refresh.timer archlinux-keyring-wkd-sync.timer; do
    systemctl disable --now "$unit" >/dev/null 2>&1 || true
done

# cua-driver's browser tools drive Chromium-family browsers only, and Arch Linux ARM has
# no Google Chrome build. Arch doesn't support partial upgrades, so installing means -Syu.
if ! command -v chromium >/dev/null 2>&1; then
    pacman -Syu --needed --noconfirm chromium >/dev/null
fi

# A new profile opens first-run UI, which blocks agents. The "First Run" marker skips it.
# (cua-driver's own isolated browsers pass --no-first-run themselves.)
install -d -o agent -g agent /home/agent/.config /home/agent/.config/chromium
[ -e "/home/agent/.config/chromium/First Run" ] ||
    install -o agent -g agent -m 644 /dev/null "/home/agent/.config/chromium/First Run"

# Chromium builds its accessibility tree only when told to, so without this
# get_window_state sees just the window frame, not the page. Arch's /usr/bin/chromium is
# chromium-launcher, which adds the flags in /etc/chromium-flags.conf (then the user's
# ~/.config/chromium-flags.conf) to every launch: the menu entries and xdg-open included.
# cua-driver's isolated browsers run the binary directly.
flags=/etc/chromium-flags.conf
grep -q -- '--force-renderer-accessibility' "$flags" 2>/dev/null ||
    echo '--force-renderer-accessibility' >> "$flags"

mkdir -p /etc/chromium/policies/managed
cat > /etc/chromium/policies/managed/agentpc.json <<'EOF'
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
# qcow2 (the disk is attached with discard). The sync databases stay so `pacman -S` works.
# (-Scc's cache prompt defaults to No, so --noconfirm alone would keep the packages.)
yes | pacman -Scc >/dev/null 2>&1 || true
journalctl --vacuum-size=16M >/dev/null 2>&1 || true
fstrim -a >/dev/null 2>&1 || true

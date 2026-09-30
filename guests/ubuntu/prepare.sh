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
# and Firefox packages are snaps. Google publishes arm64 packages in its apt repository,
# signed by its Linux package signing key, pinned here by fingerprint.
if ! command -v google-chrome >/dev/null 2>&1; then
    google_key=EB4C1BFD4F042F6DDDCCEC917721F63BD38B4796
    keyring=/usr/share/keyrings/google-chrome.gpg
    list=/etc/apt/sources.list.d/google-chrome.list
    command -v gpg >/dev/null 2>&1 || apt-get -o DPkg::Lock::Timeout=300 install -y -q gpg >/dev/null
    tmp=$(mktemp -d)
    curl -fsSL -o "$tmp/key.pub" https://dl.google.com/linux/linux_signing_key.pub
    fprs=$(gpg --homedir "$tmp" --show-keys --with-colons "$tmp/key.pub" 2>/dev/null |
        awk -F: '$1 == "pub" { p = 1; next } p && $1 == "fpr" { print $10; p = 0 }')
    if [ "$fprs" != "$google_key" ]; then
        echo "prepare: Google's signing key isn't $google_key (got: $fprs)" >&2
        exit 1
    fi
    gpg --homedir "$tmp" --dearmor < "$tmp/key.pub" > "$keyring.tmp"
    mv "$keyring.tmp" "$keyring"
    chmod 644 "$keyring"
    rm -rf "$tmp"
    echo "deb [arch=arm64 signed-by=$keyring] https://dl.google.com/linux/chrome/deb/ stable main" > "$list"
    # Refresh just this source; the others' lists stay as they are.
    apt-get -o DPkg::Lock::Timeout=300 update -q -o Dir::Etc::sourcelist="$list" \
        -o Dir::Etc::sourceparts=- -o APT::Get::List-Cleanup=0 >/dev/null
    apt-get -o DPkg::Lock::Timeout=300 install -y -q google-chrome-stable >/dev/null
fi
# VMs are throwaway: no background browser updates. (Chrome's package also writes its own
# source file on install.)
rm -f /etc/apt/sources.list.d/google-chrome.list /etc/apt/sources.list.d/google-chrome.sources \
    /var/lib/apt/lists/dl.google.com_*
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

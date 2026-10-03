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
# no Google Chrome build. build.sh installs Chromium; installing it here would mean a full
# -Syu (Arch doesn't support partial upgrades), so a missing one is an image bug.
if ! command -v chromium >/dev/null 2>&1; then
    echo "prepare: chromium is not installed; rebuild the image (agentpc image build arch)" >&2
    exit 1
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

# Power button: the ACPI press `agentpc stop` sends must shut the guest down.
# xfce4-power-manager takes logind's handle-power-key inhibitor and then, at its default
# power-button-action (0, "do nothing"), ignores the press, so a stop waited out its timeout
# and was forced. logind-handle-power-key hands the key back to logind (HandlePowerKey,
# default poweroff), session or not: system-wide for every new session, and in the running
# one (the snapshot's) through its xfconfd, which makes the power manager drop the inhibitor.
xfpm=/etc/xdg/xfce4/xfconf/xfce-perchannel-xml/xfce4-power-manager.xml
mkdir -p "${xfpm%/*}"
cat > "$xfpm" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>

<channel name="xfce4-power-manager" version="1.0">
  <property name="xfce4-power-manager" type="empty">
    <property name="logind-handle-power-key" type="bool" value="true"/>
  </property>
</channel>
EOF
bus=/run/user/$(id -u agent)/bus
if [ -S "$bus" ]; then
    runuser -u agent -- env DBUS_SESSION_BUS_ADDRESS="unix:path=$bus" xfconf-query \
        -c xfce4-power-manager -p /xfce4-power-manager/logind-handle-power-key \
        -n -t bool -s true ||
        echo "prepare: could not hand the power key to logind in the running session" >&2
fi

# SSH takes agentpc's key only. Every VM's SSH port is reachable from the other VMs (at
# 10.0.2.2) and from other users of this Mac, and the shared agent/agent login would open it
# to them. sshd keeps the first value it reads, so 00- comes before any other drop-in.
mkdir -p /etc/ssh/sshd_config.d
printf '%s\n' '# agentpc: key logins only' 'PasswordAuthentication no' 'KbdInteractiveAuthentication no' \
    > /etc/ssh/sshd_config.d/00-agentpc.conf
grep -qs '^Include /etc/ssh/sshd_config.d/\*.conf' /etc/ssh/sshd_config ||
    sed -i '1i Include /etc/ssh/sshd_config.d/*.conf' /etc/ssh/sshd_config
sshd -t
systemctl reload ssh 2>/dev/null || systemctl reload sshd

# Smaller images: drop downloaded packages and old logs, then hand free blocks back to the
# qcow2 (the disk is attached with discard). The sync databases stay so `pacman -S` works.
# (-Scc's cache prompt defaults to No, so --noconfirm alone would keep the packages.)
yes | pacman -Scc >/dev/null 2>&1 || true
journalctl --vacuum-size=16M >/dev/null 2>&1 || true
fstrim -a >/dev/null 2>&1 || true

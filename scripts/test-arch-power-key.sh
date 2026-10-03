#!/bin/sh
# Tests guests/arch/prepare.sh's power-button block without a VM: the block is cut out of the
# script, its /etc/xdg and /run/user paths moved into a temp dir, and `id` and `runuser`
# stubbed. Needs python3 (to check the XML and to make the session-bus socket).
set -u
cd "$(dirname "$0")/.." || exit 1
t=$(mktemp -d)
trap 'rm -rf "$t"' EXIT
fails=0
check() { # name expected actual
    if [ "$2" = "$3" ]; then
        echo "ok    $1"
    else
        echo "FAIL  $1"
        echo "      want: $2"
        echo "      got:  $3"
        fails=$((fails + 1))
    fi
}

awk '/^# Power button:/{p=1} p{print} p&&/^fi$/{exit}' guests/arch/prepare.sh |
    sed -e "s|/etc/xdg/|$t/etc/xdg/|" -e "s|/run/user/|$t/run/user/|" > "$t/block.sh"
check "block found in prepare.sh" "1" "$(grep -c 'xfconf-query' "$t/block.sh")"

mkdir -p "$t/bin"
printf '#!/bin/sh\n[ "$*" = "-u agent" ] && echo 1000\n' > "$t/bin/id"
printf '#!/bin/sh\nprintf "%%s|" "$@" >> "%s/runuser.log"; echo >> "%s/runuser.log"\nexit "${RUNUSER_EXIT:-0}"\n' \
    "$t" "$t" > "$t/bin/runuser"
chmod 755 "$t/bin"/*
run() { PATH="$t/bin:$PATH" sh -eu "$t/block.sh" 2> "$t/stderr"; }
xml="$t/etc/xdg/xfce4/xfconf/xfce-perchannel-xml/xfce4-power-manager.xml"

# No session running (an image build before the desktop is up): only the system default.
run
check "no session: exits 0" "0" "$?"
check "no session: runuser not called" "no" "$([ -e "$t/runuser.log" ] && echo yes || echo no)"
check "system default: property" "xfce4-power-manager /xfce4-power-manager/logind-handle-power-key bool true" \
    "$(python3 - "$xml" <<'EOF'
import sys, xml.etree.ElementTree as ET
ch = ET.parse(sys.argv[1]).getroot()
def walk(e, path):
    for p in e.findall("property"):
        q = path + "/" + p.get("name")
        if p.get("type") != "empty":
            print(ch.get("name"), q, p.get("type"), p.get("value"))
        walk(p, q)
walk(ch, "")
EOF
)"
cp "$xml" "$t/first.xml"

# A running session (the snapshot's): also set live through the agent's session bus.
mkdir -p "$t/run/user/1000"
python3 -c 'import socket, sys; socket.socket(socket.AF_UNIX).bind(sys.argv[1])' "$t/run/user/1000/bus"
run
check "session: exits 0" "0" "$?"
check "session: runuser call" \
    "-u|agent|--|env|DBUS_SESSION_BUS_ADDRESS=unix:path=$t/run/user/1000/bus|xfconf-query|-c|xfce4-power-manager|-p|/xfce4-power-manager/logind-handle-power-key|-n|-t|bool|-s|true|" \
    "$(cat "$t/runuser.log")"
check "idempotent system default" "same" "$(cmp -s "$xml" "$t/first.xml" && echo same || echo differs)"

# A failing live update warns but doesn't stop the rest of prepare.sh.
RUNUSER_EXIT=1 run
check "live failure: exits 0" "0" "$?"
check "live failure: warns" "1" "$(grep -c 'could not hand the power key' "$t/stderr")"

[ "$fails" -eq 0 ] && echo "all passed" || { echo "$fails failed"; exit 1; }

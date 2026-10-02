#!/bin/sh
# Tests for guests/helpers/, the shell helpers agentpc installs in Linux guests, run without a
# VM: stubs stand in for sudo, FEXBash, uname and dpkg (AGENTPC_REAL), and fake ELF headers
# for programs. HELPER_SH is the shell the helpers run under (default sh); CI runs it with
# dash (Ubuntu's /bin/sh) and bash (Arch's): HELPER_SH=dash dash scripts/test-guest-helpers.sh
set -u
sh_=${HELPER_SH:-sh}
cd "$(dirname "$0")/../guests/helpers" || exit 1
helpers=$PWD
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

# --- stubs: each prints how it was called ---
mkdir -p "$t/real"
for c in sudo FEXBash; do
    printf '#!/bin/sh\nprintf "%%s|" %s "$@"; echo\n' "$c" > "$t/real/$c"
done
printf '#!/bin/sh\ncase "$*" in -m) echo aarch64 ;; -a) echo "Linux vm 6.8.0 aarch64 GNU/Linux" ;; *) echo "uname $*" ;; esac\n' \
    > "$t/real/uname"
cp "$t/real/uname" "$t/real/arch"
printf '#!/bin/sh\necho "real-dpkg $*"\n' > "$t/real/dpkg"
chmod 755 "$t/real"/*
export AGENTPC_REAL="$t/real"

# --- fexbash-sudo ---
sudo_() { "$sh_" "$helpers/fexbash-sudo" "$@"; }
x='FEXBash|-c|exec "$@"|fexbash'
check "sudo cmd" "sudo|/x/$x|id|" "$(sudo_ id | sed "s|$t/real|/x|g")"
check "sudo VAR=v cmd args" "sudo|V=1|/x/$x|bash|-s|--|a|" "$(sudo_ V=1 bash -s -- a | sed "s|$t/real|/x|g")"
check "sudo -u user cmd" "sudo|-u|agent|/x/$x|id|" "$(sudo_ -u agent id | sed "s|$t/real|/x|g")"
check "sudo -Eu user cmd (bundled)" "sudo|-Eu|agent|/x/$x|id|" "$(sudo_ -Eu agent id | sed "s|$t/real|/x|g")"
check "sudo -uagent cmd (attached)" "sudo|-uagent|/x/$x|id|" "$(sudo_ -uagent id | sed "s|$t/real|/x|g")"
check "sudo --user agent cmd" "sudo|--user|agent|/x/$x|id|" "$(sudo_ --user agent id | sed "s|$t/real|/x|g")"
check "sudo --user=agent cmd" "sudo|--user=agent|/x/$x|id|" "$(sudo_ --user=agent id | sed "s|$t/real|/x|g")"
check "sudo -E -- cmd" "sudo|-E|--|/x/$x|id|" "$(sudo_ -E -- id | sed "s|$t/real|/x|g")"
check "sudo -l passes through" "sudo|-l|" "$(sudo_ -l)"
check "sudo -v passes through" "sudo|-v|" "$(sudo_ -v)"
check "sudo -k alone passes through" "sudo|-k|" "$(sudo_ -k)"
check "sudo -k cmd still runs the command as x86" "sudo|-k|/x/$x|id|" "$(sudo_ -k id | sed "s|$t/real|/x|g")"
check "sudo -i opens an x86 bash" "sudo|-i|/x/FEXBash|" "$(sudo_ -i | sed "s|$t/real|/x|g")"
check "an argument with spaces survives" "sudo|/x/$x|sh|-c|echo a b|" "$(sudo_ sh -c 'echo a b' | sed "s|$t/real|/x|g")"

# --- maintscript-uname / maintscript-dpkg ---
mkdir -p "$t/bin"
cp "$helpers/maintscript-uname" "$t/bin/uname"
cp "$helpers/maintscript-uname" "$t/bin/arch"
cp "$helpers/maintscript-dpkg" "$t/bin/dpkg"
chmod 755 "$t/bin"/*
u() { "$sh_" "$t/bin/uname" "$@"; }
a() { "$sh_" "$t/bin/arch" "$@"; }
d() { "$sh_" "$t/bin/dpkg" "$@"; }
check "uname -m outside a maintainer script" aarch64 "$(u -m)"
check "uname -m in an amd64 maintainer script" x86_64 "$(DPKG_MAINTSCRIPT_ARCH=amd64 u -m)"
check "uname -a in an amd64 maintainer script" "Linux vm 6.8.0 x86_64 GNU/Linux" \
    "$(DPKG_MAINTSCRIPT_ARCH=amd64 u -a)"
check "uname -m in an i386 maintainer script" i686 "$(DPKG_MAINTSCRIPT_ARCH=i386 u -m)"
check "uname -m in an arm64 maintainer script" aarch64 "$(DPKG_MAINTSCRIPT_ARCH=arm64 u -m)"
check "arch in an amd64 maintainer script" x86_64 "$(DPKG_MAINTSCRIPT_ARCH=amd64 a -m)"
check "dpkg --print-architecture outside" "real-dpkg --print-architecture" "$(d --print-architecture)"
check "dpkg --print-architecture in amd64" amd64 "$(DPKG_MAINTSCRIPT_ARCH=amd64 d --print-architecture)"
check "dpkg -l in amd64 is the real dpkg" "real-dpkg -l" "$(DPKG_MAINTSCRIPT_ARCH=amd64 d -l)"

# --- agentpc-fex (the systemd generator) ---
elf() { # path e_machine: a file with an ELF header for that machine (62 x86_64, 3 i386, 183 arm64)
    printf '\177ELF' > "$1"
    head -c 14 /dev/zero >> "$1"
    printf "$(printf '\\%03o\\%03o' "$(($2 % 256))" "$(($2 / 256))")" >> "$1"
    head -c 40 /dev/zero >> "$1"
    chmod 755 "$1"
}
elf "$t/x86prog" 62
elf "$t/i386prog" 3
elf "$t/armprog" 183
mkdir -p "$t/etc" "$t/run" "$t/lib" "$t/sp ace"
elf "$t/sp ace/prog" 62
export AGENTPC_UNIT_DIRS="$t/etc $t/run $t/lib"
unit() { # dir name body
    printf '[Service]\n%s\n' "$3" > "$1/$2"
}
hard='MemoryDenyWriteExecute=yes'
unit "$t/lib" x86.service "ExecStart=$t/x86prog --flag
$hard"
unit "$t/lib" i386.service "ExecStart=-@$t/i386prog
LockPersonality=yes"
unit "$t/lib" arm.service "ExecStart=$t/armprog
$hard"
unit "$t/lib" soft.service "ExecStart=$t/x86prog"
unit "$t/lib" quoted.service "ExecStart=\"$t/sp ace/prog\" arg
$hard"
# A native unit whose x86 ExecStart comes from a drop-in in /run.
unit "$t/lib" dropin.service "ExecStart=$t/armprog
$hard"
mkdir -p "$t/run/dropin.service.d"
printf '[Service]\nExecStart=\nExecStart=%s\n' "$t/x86prog" > "$t/run/dropin.service.d/10-x86.conf"
# An x86 unit a drop-in switches to a native program; the /etc copy hides the /lib one.
unit "$t/lib" back.service "ExecStart=$t/x86prog
$hard"
mkdir -p "$t/lib/back.service.d" "$t/etc/back.service.d"
printf '[Service]\nExecStart=%s\n' "$t/x86prog" > "$t/lib/back.service.d/50-x.conf"
printf '[Service]\nExecStart=\nExecStart=%s\n' "$t/armprog" > "$t/etc/back.service.d/50-x.conf"
# The /etc unit file overrides the /lib one.
unit "$t/lib" over.service "ExecStart=$t/armprog
$hard"
unit "$t/etc" over.service "ExecStart=$t/x86prog
$hard"
# A oneshot unit with two ExecStart=: an x86 one, then a native one; systemd runs both.
unit "$t/lib" multi.service "Type=oneshot
ExecStart=$t/x86prog
ExecStart=$t/armprog
$hard"
"$sh_" "$helpers/agentpc-fex" "$t/out"
got=$(cd "$t/out" 2>/dev/null && ls -d -- *.d | sort | tr '\n' ' ')
check "generator relaxes exactly the hardened x86 units" \
    "dropin.service.d i386.service.d multi.service.d over.service.d quoted.service.d x86.service.d " "$got"
check "generator drop-in content" "[Service] MemoryDenyWriteExecute=no LockPersonality=no" \
    "$(grep -v '^#' "$t/out/x86.service.d/zz-agentpc-fex.conf" | tr '\n' ' ' | sed 's/ $//')"
check "the drop-in sorts after the unit's own (hardening.conf, 50-x.conf)" zz-agentpc-fex.conf \
    "$(printf '%s\n' hardening.conf 50-x.conf zz-agentpc-fex.conf | sort | tail -1)"

echo
if [ "$fails" -gt 0 ]; then
    echo "$fails failed"
    exit 1
fi
echo "all passed"

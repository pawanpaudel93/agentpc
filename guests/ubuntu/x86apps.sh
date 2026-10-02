#!/bin/sh
# x86_64 and i386 Linux programs for `ubuntu-<release>-x86apps` images, through FEX, which
# translates each program to arm64 (the kernel and desktop stay native). Applied (as root)
# whenever the image's snapshot is captured, before prepare.sh. Idempotent.
set -eu
export DEBIAN_FRONTEND=noninteractive

# FEX, pinned like cua-driver: built from source with /tmp/agentpc-fex.patch
# (guests/ubuntu/fex.patch), because FEX's PPA keeps only its newest release. Static-pie, so
# the binfmt_misc interpreter also starts inside Docker and Podman containers, where the
# patch runs it without FEXServer on the container's own x86 files. Tuned for armv8.4 (LSE
# atomics, RCpc loads), which every Apple Silicon chip has; the default would tune for the
# build machine's CPU and could crash on an older one. Bump deliberately.
fex_version=2609.1
# The commit the FEX-$fex_version tag points to (its peeled ^{} commit): a moved tag fails the build.
fex_commit=9fbdc00bd6401aff3b32d79e78ff98b8a13e4dcf
# The patch is part of the build: a changed one rebuilds FEX like a new version does.
# agentpc uploads the patch before running this; without it the checksum below would be empty
# and set off a rebuild that fails at the end.
[ -f /tmp/agentpc-fex.patch ] || { echo "no /tmp/agentpc-fex.patch" >&2; exit 1; }
fex_patch=$(sha256sum /tmp/agentpc-fex.patch | cut -d" " -f1)
if [ "$(cat /var/lib/agentpc/fex-version 2>/dev/null)" != "$fex_version" ] ||
    [ "$(cat /var/lib/agentpc/fex-patch 2>/dev/null)" != "$fex_patch" ]; then
    # Images from before the pin had FEX from its PPA.
    if dpkg -s fex-emu-armv8.4 >/dev/null 2>&1; then
        dpkg-divert --quiet --local --rename --remove /usr/bin/FEX 2>/dev/null || true
        apt-mark unhold fex-emu-armv8.4 fex-emu-binfmt32 fex-emu-binfmt64 >/dev/null
        apt-get purge -y -q fex-emu-armv8.4 fex-emu-binfmt32 fex-emu-binfmt64 >/dev/null
        rm -f /etc/apt/sources.list.d/fex-emu-ubuntu-fex-*
    fi
    build_deps="clang lld cmake ninja-build nasm"
    apt-get -o DPkg::Lock::Timeout=300 install -y -q git squashfuse $build_deps >/dev/null
    src=$(mktemp -d)
    git clone -q --depth 1 --branch "FEX-$fex_version" --recurse-submodules --shallow-submodules \
        https://github.com/FEX-Emu/FEX "$src/FEX"
    got=$(git -C "$src/FEX" rev-parse HEAD)
    if [ "$got" != "$fex_commit" ]; then
        echo "FEX tag FEX-$fex_version is at $got, expected $fex_commit: refusing to build" >&2
        exit 1
    fi
    git -C "$src/FEX" apply /tmp/agentpc-fex.patch
    # The build's output goes to a log; on failure its tail goes to stderr, which agentpc shows.
    log=$src/build.log
    run() {
        "$@" >>"$log" 2>&1 || { echo "FEX build failed: $1 (log: $log)" >&2; tail -n 60 "$log" >&2; exit 1; }
    }
    run cmake -S "$src/FEX" -B "$src/build" -G Ninja -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_INSTALL_PREFIX=/usr -DCMAKE_C_COMPILER=clang -DCMAKE_CXX_COMPILER=clang++ \
        -DCMAKE_CXX_SCAN_FOR_MODULES=OFF -DCMAKE_EXE_LINKER_FLAGS="-static-pie -fuse-ld=lld" \
        -DTUNE_CPU=none -DTUNE_ARCH=armv8.4-a -DBUILD_TESTING=OFF -DBUILD_THUNKS=OFF \
        -DBUILD_FEXCONFIG=OFF -DENABLE_GDB_SYMBOLS=OFF -DENABLE_OFFLINE_TELEMETRY=OFF \
        -DENABLE_CCACHE=OFF
    run ninja -C "$src/build" install
    rm -rf "$src"
    apt-get purge -y -q --autoremove $build_deps >/dev/null
    mkdir -p /var/lib/agentpc
    # binfmt_misc's F flag holds the interpreter open: re-register the new one. Before the
    # stamps, so a failed registration is retried on the next run instead of skipped.
    systemctl restart systemd-binfmt
    echo "$fex_version" > /var/lib/agentpc/fex-version
    echo "$fex_patch" > /var/lib/agentpc/fex-patch
fi
rm -f /tmp/agentpc-fex.patch

# The x86 libraries programs load (libc, libstdc++, ...): FEX's squashfs of an x86 Ubuntu,
# 0.5 GB instead of 1.9 GB unpacked. The kernel mounts it read-only at boot (fstab), rather
# than FEX through FUSE at each start, which a systemd service with PrivateDevices= or
# NoNewPrivileges= can't do.
. /etc/os-release
name="Ubuntu_$(echo "$VERSION_ID" | tr . _)"
rootfs=/usr/share/fex-emu/RootFS/$name.sqsh
mnt=/usr/share/fex-emu/RootFS/$name
# Pinned and checked per release (sha256 of the file rootfs.fex-emu.gg lists; its own hash
# is xxh3); a stamp records the installed pin, so a bump reaches existing images.
case "$VERSION_ID" in
    24.04) rootfs_url=https://rootfs.fex-emu.gg/Ubuntu_24_04/2026-08-11/Ubuntu_24_04.sqsh
        rootfs_sha256=2854b06d3ff1b8f6e526135bfb6dd5b7b30ab3ab73e79ae933a3d9fed959a178 ;;
    22.04) rootfs_url=https://rootfs.fex-emu.gg/Ubuntu_22_04/2025-01-08/Ubuntu_22_04.sqsh
        rootfs_sha256=1bbbd33486eaac93b187a59ba2173665efdbaa3274dd1d8eeb4bd829147f1981 ;;
    *) rootfs_url='' rootfs_sha256='' ;;
esac
rootfs_stamp=/var/lib/agentpc/fex-rootfs
mkdir -p /usr/share/fex-emu/RootFS /var/lib/agentpc
if [ -n "$rootfs_url" ]; then
    if [ "$(cat "$rootfs_stamp" 2>/dev/null)" != "$rootfs_url:$rootfs_sha256" ]; then
        # Images from before the pin may already hold this file: check it before downloading.
        if ! { [ -f "$rootfs" ] && echo "$rootfs_sha256  $rootfs" | sha256sum -c --quiet >/dev/null 2>&1; }; then
            curl -fsSL -o "$rootfs.tmp" "$rootfs_url"
            echo "$rootfs_sha256  $rootfs.tmp" | sha256sum -c --quiet
            # The old one must come off first: left mounted (busy), it would stay in use
            # under a stamp that names the new one.
            if mountpoint -q "$mnt"; then
                umount "$mnt" || { echo "x86 root filesystem at $mnt is busy; stop what uses it" >&2; exit 1; }
            fi
            mv "$rootfs.tmp" "$rootfs"
        fi
        echo "$rootfs_url:$rootfs_sha256" > "$rootfs_stamp"
    fi
else
    # Everything a guest downloads is checked against a pin; there is none for this release
    # (and one already here from before was never checked).
    echo "no pinned x86 root filesystem for Ubuntu $VERSION_ID: x86apps images are 22.04 or 24.04" >&2
    exit 1
fi
if ! mountpoint -q "$mnt"; then
    # Images built before the squashfs had it unpacked here.
    rm -rf "$mnt"
    mkdir -p "$mnt"
fi
sed -i "\\#^$rootfs #d" /etc/fstab
echo "$rootfs $mnt squashfs loop,ro,nofail 0 0" >> /etc/fstab
systemctl daemon-reload
mountpoint -q "$mnt" || mount "$mnt"

# The global config, so every user (root too) gets it. DiskCache keeps translated code in
# ~/.cache/fex-emu: agents relaunch the same tools, and a warm start is ~5x faster. Go
# programs crash under FEX when the runtime preempts goroutines with signals; Env sets
# GODEBUG for translated programs only. agentpc runs this helper after every boot: with
# "hardware" when QEMU put every vCPU in TSO mode, so FEX can stop emulating x86 memory
# ordering (up to ~40% faster), else "emulated". The image keeps "emulated", which is always safe.
cat > /usr/local/sbin/agentpc-fex-tso <<EOF
#!/bin/sh
set -eu
case "\${1:-}" in
    hardware) tso=', "TSOEnabled": "0"' ;;
    emulated) tso= ;;
    *) echo "usage: agentpc-fex-tso hardware|emulated" >&2; exit 2 ;;
esac
printf '{"Config": {"RootFS": "%s", "DiskCache": "1", "Env": "GODEBUG=asyncpreemptoff=1"%s}}\n' \\
    "$mnt" "\$tso" > /usr/share/fex-emu/Config.json.tmp
mv /usr/share/fex-emu/Config.json.tmp /usr/share/fex-emu/Config.json
EOF
chmod 755 /usr/local/sbin/agentpc-fex-tso
/usr/local/sbin/agentpc-fex-tso emulated

# `sudo fex-unit <unit>` lets a systemd service run an x86 program. FEX translates x86 code
# as the program runs, so it writes code and then executes it, and it sets the process
# personality: MemoryDenyWriteExecute= and LockPersonality= forbid both, and the program then
# dies at start without a message. fex-unit adds a drop-in that allows them (--undo removes it).
install -m 755 /tmp/agentpc-helpers/fex-unit /usr/local/bin/fex-unit

# dpkg runs an amd64 package's maintainer scripts (preinst, postinst, ...) with the native
# arm64 shell, so `uname -m` in them says aarch64 and an arch check in a vendor .deb refuses to
# install, though the package's x86 programs run fine through FEX. dpkg sets
# DPKG_MAINTSCRIPT_ARCH for those scripts; while it is amd64 or i386, these answer as that
# machine. Anywhere else they are the real commands. They sit first on `sudo dpkg`'s PATH;
# apt runs dpkg with its own DPkg::Path, so that gets /usr/local/bin too.
install -m 755 /tmp/agentpc-helpers/maintscript-uname /usr/local/bin/uname
install -m 755 /tmp/agentpc-helpers/maintscript-uname /usr/local/bin/arch
install -m 755 /tmp/agentpc-helpers/maintscript-dpkg /usr/local/bin/dpkg
cat > /etc/apt/apt.conf.d/99agentpc-dpkg-path <<'EOF'
// agentpc: the PATH `sudo dpkg -i` has, so maintainer scripts run by apt find the x86 answers
// in /usr/local/bin (uname, arch, dpkg) for amd64/i386 packages.
DPkg::Path "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
EOF

# FEXBash puts /usr/libexec/agentpc/fexbash first on PATH, for this sudo: the real sudo is
# setuid, so it runs natively, and so would everything it starts (`curl ... | sudo bash -s`
# would see aarch64 again). It keeps sudo's options and VAR=value assignments but has sudo start
# the command through FEXBash, so the command stays x86.
mkdir -p /usr/libexec/agentpc/fexbash
install -m 755 /tmp/agentpc-helpers/fexbash-sudo /usr/libexec/agentpc/fexbash/sudo

# A systemd generator does what fex-unit does for every service whose ExecStart is an x86
# program, so hardened x86 services work as installed. It runs at boot and on every
# `systemctl daemon-reload`; arm64 services keep their settings. fex-unit stays for units
# that start an x86 program some other way (a script, a wrapper).
mkdir -p /etc/systemd/system-generators
install -m 755 /tmp/agentpc-helpers/agentpc-fex /etc/systemd/system-generators/agentpc-fex
systemctl daemon-reload

# x86 libraries a program needs beyond the RootFS: `sudo apt install libfoo:amd64` (or an
# amd64 .deb). FEX looks in the RootFS first, then the real filesystem, so they load from
# /usr/lib/x86_64-linux-gnu. Ubuntu serves amd64 from archive.ubuntu.com and arm64 from
# ports.ubuntu.com, so each source is pinned to its architecture.
# Marked done only once the package lists are in: a failed `apt-get update` is retried on the
# next run (every step here is safe to repeat), not skipped because amd64 is registered.
if [ ! -f /var/lib/agentpc/amd64-apt ]; then
    deb822=/etc/apt/sources.list.d/ubuntu.sources
    if [ -f "$deb822" ]; then
        grep -q '^Architectures:' "$deb822" || sed -i '/^Types: deb$/a Architectures: arm64' "$deb822"
    fi
    [ -f /etc/apt/sources.list ] && sed -i 's/^deb http/deb [arch=arm64] http/' /etc/apt/sources.list
    c=$VERSION_CODENAME
    cat > /etc/apt/sources.list.d/ubuntu-amd64.sources <<EOF
Types: deb
URIs: http://archive.ubuntu.com/ubuntu
Suites: $c $c-updates $c-backports $c-security
Components: main restricted universe multiverse
Architectures: amd64
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg
EOF
    dpkg --add-architecture amd64
    apt-get -o DPkg::Lock::Timeout=300 update -q >/dev/null
    touch /var/lib/agentpc/amd64-apt
fi

# FEX works, and so does the kernel's binfmt registration (an x86 program run directly).
FEXBash -c true
"$mnt/usr/bin/true"

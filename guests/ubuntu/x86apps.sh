#!/bin/sh
# x86_64 and i386 Linux programs for `ubuntu-<release>-x86apps` images, through FEX, which
# translates each program to arm64 (the kernel and desktop stay native). Applied (as root)
# whenever the image's snapshot is captured, before prepare.sh. Idempotent.
set -eu
export DEBIAN_FRONTEND=noninteractive

# armv8.4 builds use LSE atomics and RCpc loads, which every Apple Silicon chip has.
if ! command -v FEX >/dev/null 2>&1; then
    add-apt-repository -y ppa:fex-emu/fex >/dev/null
    apt-get -o DPkg::Lock::Timeout=300 install -y -q \
        fex-emu-armv8.4 fex-emu-binfmt32 fex-emu-binfmt64 squashfuse >/dev/null
fi
apt-mark hold fex-emu-armv8.4 fex-emu-binfmt32 fex-emu-binfmt64 >/dev/null

# The x86 libraries programs load (libc, libstdc++, ...): FEX's squashfs of an x86 Ubuntu,
# which FEX mounts on first use. 0.5 GB instead of 1.9 GB unpacked, and no slower to start.
. /etc/os-release
name="Ubuntu_$(echo "$VERSION_ID" | tr . _)"
rootfs=/usr/share/fex-emu/RootFS/$name.sqsh
if [ ! -f "$rootfs" ]; then
    tmp=$(mktemp -d)
    HOME=$tmp XDG_DATA_HOME=$tmp/data XDG_CONFIG_HOME=$tmp/config FEXRootFSFetcher -y -a \
        --distro-name=ubuntu --distro-version="$VERSION_ID" --distro-list-first \
        --force-ui=tty >/dev/null
    mkdir -p /usr/share/fex-emu/RootFS
    mv "$tmp/data/fex-emu/RootFS/$name.sqsh" "$rootfs"
    rm -rf "$tmp"
fi
# Images built before the squashfs had it unpacked.
rm -rf "/usr/share/fex-emu/RootFS/$name"

# The global config, so every user (root too) gets it. DiskCache keeps translated code in
# ~/.cache/fex-emu: agents relaunch the same tools, and a warm start is ~5x faster. Go
# programs crash under FEX when the runtime preempts goroutines with signals; Env sets
# GODEBUG for translated programs only.
cat > /usr/share/fex-emu/Config.json <<EOF
{"Config": {"RootFS": "$rootfs", "DiskCache": "1", "Env": "GODEBUG=asyncpreemptoff=1"}}
EOF

# x86 libraries a program needs beyond the RootFS: `sudo apt install libfoo:amd64` (or an
# amd64 .deb). FEX looks in the RootFS first, then the real filesystem, so they load from
# /usr/lib/x86_64-linux-gnu. Ubuntu serves amd64 from archive.ubuntu.com and arm64 from
# ports.ubuntu.com, so each source is pinned to its architecture.
if ! dpkg --print-foreign-architectures | grep -qx amd64; then
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
fi

FEXBash -c true

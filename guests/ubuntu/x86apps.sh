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
        fex-emu-armv8.4 fex-emu-binfmt32 fex-emu-binfmt64 >/dev/null
fi
apt-mark hold fex-emu-armv8.4 fex-emu-binfmt32 fex-emu-binfmt64 >/dev/null

# The x86 libraries programs load (libc, libstdc++, ...), unpacked rather than a squashfs so
# nothing needs mounting when a VM resumes. The qcow2 is compressed for the registry anyway.
. /etc/os-release
name="Ubuntu_$(echo "$VERSION_ID" | tr . _)"
rootfs=/usr/share/fex-emu/RootFS/$name
if [ ! -d "$rootfs/usr" ]; then
    tmp=$(mktemp -d)
    HOME=$tmp XDG_DATA_HOME=$tmp/data XDG_CONFIG_HOME=$tmp/config FEXRootFSFetcher -y -x \
        --distro-name=ubuntu --distro-version="$VERSION_ID" --distro-list-first \
        --force-ui=tty >/dev/null
    mkdir -p /usr/share/fex-emu/RootFS
    mv "$tmp/data/fex-emu/RootFS/$name" "$rootfs"
    rm -rf "$tmp"
fi

# The global config, so every user (root too) gets it. DiskCache keeps translated code in
# ~/.cache/fex-emu: agents relaunch the same tools, and a warm start is ~5x faster. Go
# programs crash under FEX when the runtime preempts goroutines with signals; Env sets
# GODEBUG for translated programs only.
cat > /usr/share/fex-emu/Config.json <<EOF
{"Config": {"RootFS": "$rootfs", "DiskCache": "1", "Env": "GODEBUG=asyncpreemptoff=1"}}
EOF

"$rootfs/usr/bin/true"

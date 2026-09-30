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
if [ "$(cat /usr/share/fex-emu/agentpc-build 2>/dev/null)" != "$fex_version" ]; then
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
        https://github.com/FEX-Emu/FEX "$src/FEX" 2>/dev/null
    git -C "$src/FEX" apply /tmp/agentpc-fex.patch
    cmake -S "$src/FEX" -B "$src/build" -G Ninja -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_INSTALL_PREFIX=/usr -DCMAKE_C_COMPILER=clang -DCMAKE_CXX_COMPILER=clang++ \
        -DCMAKE_CXX_SCAN_FOR_MODULES=OFF -DCMAKE_EXE_LINKER_FLAGS="-static-pie -fuse-ld=lld" \
        -DTUNE_CPU=none -DTUNE_ARCH=armv8.4-a -DBUILD_TESTING=OFF -DBUILD_THUNKS=OFF \
        -DBUILD_FEXCONFIG=OFF -DENABLE_GDB_SYMBOLS=OFF -DENABLE_OFFLINE_TELEMETRY=OFF \
        -DENABLE_CCACHE=OFF >/dev/null
    ninja -C "$src/build" install >/dev/null
    rm -rf "$src"
    apt-get purge -y -q --autoremove $build_deps >/dev/null
    echo "$fex_version" > /usr/share/fex-emu/agentpc-build
    # binfmt_misc's F flag holds the interpreter open: re-register the new one.
    systemctl restart systemd-binfmt
fi
rm -f /tmp/agentpc-fex.patch

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
# GODEBUG for translated programs only. agentpc runs this helper after every boot: with
# "hardware" when QEMU put every vCPU in TSO mode, so FEX can stop emulating x86 memory
# ordering (up to ~40% faster), else "emulate". The image keeps "emulate", which is always safe.
cat > /usr/local/sbin/agentpc-fex-tso <<EOF
#!/bin/sh
set -eu
case "\${1:-}" in
    hardware) tso=', "TSOEnabled": "0"' ;;
    emulate) tso= ;;
    *) echo "usage: agentpc-fex-tso hardware|emulate" >&2; exit 2 ;;
esac
printf '{"Config": {"RootFS": "%s", "DiskCache": "1", "Env": "GODEBUG=asyncpreemptoff=1"%s}}\n' \\
    "$rootfs" "\$tso" > /usr/share/fex-emu/Config.json.tmp
mv /usr/share/fex-emu/Config.json.tmp /usr/share/fex-emu/Config.json
EOF
chmod 755 /usr/local/sbin/agentpc-fex-tso
/usr/local/sbin/agentpc-fex-tso emulate

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

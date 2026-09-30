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

# x86 containers: the packaged FEX is dynamically linked, so it can't start inside a
# container (its arm64 libraries aren't there), and it needs a FEXServer the container can't
# reach. Rebuild the same release static-pie with /tmp/agentpc-fex.patch (guests/ubuntu/
# fex.patch: no server inside a container, plus a fix the static build needs) and divert the
# packaged binary. binfmt_misc's F flag holds the interpreter open, so re-register it.
if ! file -L /usr/bin/FEX | grep -q static-pie; then
    version=$(FEXGetConfig --version)
    build_deps="clang lld cmake ninja-build nasm"
    apt-get -o DPkg::Lock::Timeout=300 install -y -q git $build_deps >/dev/null
    src=$(mktemp -d)
    git clone -q --depth 1 --branch "FEX-$version" --recurse-submodules --shallow-submodules \
        https://github.com/FEX-Emu/FEX "$src/FEX" 2>/dev/null
    git -C "$src/FEX" apply /tmp/agentpc-fex.patch
    cmake -S "$src/FEX" -B "$src/build" -G Ninja -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_INSTALL_PREFIX=/usr -DCMAKE_C_COMPILER=clang -DCMAKE_CXX_COMPILER=clang++ \
        -DCMAKE_CXX_SCAN_FOR_MODULES=OFF -DCMAKE_EXE_LINKER_FLAGS="-static-pie -fuse-ld=lld" \
        -DBUILD_TESTING=OFF -DBUILD_THUNKS=OFF -DBUILD_FEXCONFIG=OFF -DENABLE_ASSERTIONS=OFF \
        >/dev/null
    ninja -C "$src/build" FEX >/dev/null
    dpkg-divert --quiet --local --rename --add /usr/bin/FEX
    install -m 755 "$src/build/Bin/FEX" /usr/bin/FEX
    rm -rf "$src"
    apt-get purge -y -q --autoremove $build_deps >/dev/null
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

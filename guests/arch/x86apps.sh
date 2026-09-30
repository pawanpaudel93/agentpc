#!/bin/sh
# x86_64 and i386 Linux programs for `arch-rolling-x86apps` images, through FEX, which
# translates each program to arm64 (the kernel and desktop stay native). The Arch twin of
# guests/ubuntu/x86apps.sh. Applied (as root) whenever the image's snapshot is captured,
# before prepare.sh. Idempotent.
set -eu

# FEX, pinned and built from source with /tmp/agentpc-fex.patch exactly as on Ubuntu (see
# guests/ubuntu/x86apps.sh): static-pie so x86 containers work, tuned for armv8.4. Arch Linux
# ARM doesn't package FEX. The build runs in /var/tmp: Arch's /tmp is a small RAM disk.
fex_version=2609.1
if [ "$(cat /var/lib/agentpc/fex-version 2>/dev/null)" != "$fex_version" ]; then
    # llvm brings llvm-ar, which the ThinLTO build needs.
    build_deps="clang lld llvm cmake ninja nasm"
    # shellcheck disable=SC2086
    pacman -S --noconfirm --needed git python $build_deps >/dev/null
    src=$(mktemp -d -p /var/tmp)
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
    # shellcheck disable=SC2086
    pacman -Rns --noconfirm $build_deps >/dev/null
    mkdir -p /var/lib/agentpc
    echo "$fex_version" > /var/lib/agentpc/fex-version
    # binfmt_misc's F flag holds the interpreter open: re-register the new one.
    systemctl restart systemd-binfmt
fi
rm -f /tmp/agentpc-fex.patch

# The x86 libraries programs load: FEX's x86_64 Arch Linux root filesystem (glibc, GTK, Mesa,
# NSS, 32-bit libraries, its own pacman), pinned and checked. Unpacked rather than mounted, so
# fex-pacman can install more into it: Arch has no multiarch, so x86 libraries can't come
# from the arm64 system's pacman.
rootfs=/usr/share/fex-emu/RootFS/ArchLinux
rootfs_date=2026-08-11
rootfs_sha256=5d0c1a38590c68e5c2597c2c8a26d2f80170b1b738c857d63e1cdadada5f5f2a
if [ ! -d "$rootfs/usr" ]; then
    pacman -S --noconfirm --needed squashfs-tools >/dev/null
    sqsh=/var/tmp/ArchLinux.sqsh
    curl -fsSL -o "$sqsh" "https://rootfs.fex-emu.gg/ArchLinux/$rootfs_date/ArchLinux.sqsh"
    echo "$rootfs_sha256  $sqsh" | sha256sum -c --quiet
    mkdir -p /usr/share/fex-emu/RootFS
    unsquashfs -q -f -d "$rootfs" "$sqsh" >/dev/null
    rm -f "$sqsh"
    pacman -Rns --noconfirm squashfs-tools >/dev/null
    # Its pacman: no seccomp download sandbox (FEX doesn't support seccomp), and the Arch
    # Linux Archive snapshot of the tree's own date, so installs never mean a partial upgrade
    # (and never replace its source-built Mesa).
    sed -i 's/^#DisableSandboxSyscalls/DisableSandboxSyscalls/' "$rootfs/etc/pacman.conf"
    grep -q '^DisableSandboxSyscalls' "$rootfs/etc/pacman.conf" ||
        echo DisableSandboxSyscalls >> "$rootfs/etc/pacman.conf"
    echo "Server = https://archive.archlinux.org/repos/$(echo "$rootfs_date" | tr - /)/\$repo/os/\$arch" \
        > "$rootfs/etc/pacman.d/mirrorlist"
fi

# `sudo fex-pacman -Sy --noconfirm --needed <pkg>` installs x86 packages into the tree. FEX
# only reads through its RootFS, so writes need a real chroot; the .containerenv marker makes
# our FEX build run self-contained there (no FEXServer, the chroot as its root). While pacman
# runs, the tree gets its own user database and DNS config back; afterwards they're moved out
# again so x86 programs see this VM's users, hosts and resolver.
cat > /usr/local/bin/fex-pacman <<'EOF'
#!/bin/bash
# Install x86_64 Arch Linux packages for x86 programs: sudo fex-pacman -Sy --noconfirm --needed <pkg>
set -euo pipefail
[ "$(id -u)" = 0 ] || exec sudo "$0" "$@"
R=/usr/share/fex-emu/RootFS/ArchLinux
ids="passwd passwd- group group- shadow shadow- gshadow gshadow- subuid subgid"
cleanup() {
    for m in dev sys proc tmp; do umount -R "$R/$m" 2>/dev/null || true; done
    rm -f "$R/run/.containerenv" "$R/etc/resolv.conf"
    mkdir -p "$R/chroot/etc"
    for f in $ids; do if [ -e "$R/etc/$f" ]; then mv -f "$R/etc/$f" "$R/chroot/etc/$f"; fi; done
}
trap cleanup EXIT
for f in $ids; do if [ -e "$R/chroot/etc/$f" ]; then mv -f "$R/chroot/etc/$f" "$R/etc/$f"; fi; done
cp -L /etc/resolv.conf "$R/etc/resolv.conf"
mkdir -p "$R/proc" "$R/sys" "$R/dev" "$R/tmp" "$R/run"
touch "$R/run/.containerenv"
mount -t proc proc "$R/proc"
mount --rbind /sys "$R/sys"; mount --make-rslave "$R/sys"
mount --rbind /dev "$R/dev"; mount --make-rslave "$R/dev"
mount -t tmpfs tmpfs "$R/tmp"
FEX_ROOTFS= chroot "$R" /usr/bin/pacman "$@"
EOF
chmod 755 /usr/local/bin/fex-pacman

# The global config, so every user (root too) gets it; see guests/ubuntu/x86apps.sh. agentpc
# runs this helper after every boot: "hardware" when every vCPU runs in TSO mode, else
# "emulated". The image keeps "emulated", which is always safe.
cat > /usr/local/sbin/agentpc-fex-tso <<EOF
#!/bin/sh
set -eu
case "\${1:-}" in
    hardware) tso=', "TSOEnabled": "0"' ;;
    emulated) tso= ;;
    *) echo "usage: agentpc-fex-tso hardware|emulated" >&2; exit 2 ;;
esac
printf '{"Config": {"RootFS": "%s", "DiskCache": "1", "Env": "GODEBUG=asyncpreemptoff=1"%s}}\n' \\
    "$rootfs" "\$tso" > /usr/share/fex-emu/Config.json.tmp
mv /usr/share/fex-emu/Config.json.tmp /usr/share/fex-emu/Config.json
EOF
chmod 755 /usr/local/sbin/agentpc-fex-tso
/usr/local/sbin/agentpc-fex-tso emulated

FEXBash -c true

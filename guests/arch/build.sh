#!/bin/bash
# Builds the Arch Linux ARM image onto a blank disk (the only argument, e.g. /dev/vdb). Runs as
# root in a throwaway Ubuntu VM: macOS can't make ext4 or run pacman, and an arm64 Ubuntu
# can chroot into Arch Linux ARM natively. agentpc then turns the disk into the image and
# captures its snapshot, which runs guests/arch/prepare.sh.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
disk=${1:?usage: build.sh <disk>}

echo "==> installing build tools and downloading Arch Linux ARM"
apt-get -o DPkg::Lock::Timeout=300 install -y -q arch-install-scripts libarchive-tools dosfstools \
    gdisk gnupg >/dev/null

# Arch Linux ARM's generic aarch64 root filesystem. Its download host has no valid HTTPS, so
# the tarball is checked against the Arch Linux ARM Build System key, pinned by fingerprint
# (listed at archlinuxarm.org/about/package-signing), which is fetched over HTTPS.
# The tarball and its signature live in $cache, which agentpc fills from its own copy before
# the build and reads back after it: a cached pair that verifies is used as is; otherwise both
# are downloaded there (with the tarball's Last-Modified in $tarball.date) and left for agentpc.
signer=68B3537F39A313B3E574D06777193F152BDBE6A6
name=ArchLinuxARM-aarch64-latest.tar.gz
cache=/home/agent/agentpc-alarm
tarball=$cache/$name
export GNUPGHOME=/tmp/alarm-gnupg
rm -rf "$GNUPGHOME"; install -d -m 700 "$GNUPGHOME"
curl -fsSL "https://keyserver.ubuntu.com/pks/lookup?op=get&search=0x$signer" | gpg -q --import
verify() {
    local status
    # Captured first: grep -q exiting early would SIGPIPE gpg, which pipefail turns into a failure.
    status=$(gpg -q --status-fd 1 --verify "$tarball.sig" "$tarball" 2>/dev/null) || return 1
    grep -q "VALIDSIG $signer" <<<"$status"
}
download() {
    local headers
    mkdir -p "$cache"
    rm -f "$tarball" "$tarball.sig" "$tarball.date"
    headers=$(mktemp)
    curl -fsSL -D "$headers" -o "$tarball.part" "http://os.archlinuxarm.org/os/$name"
    curl -fsSL -o "$tarball.sig.part" "http://os.archlinuxarm.org/os/$name.sig"
    mv "$tarball.part" "$tarball"
    mv "$tarball.sig.part" "$tarball.sig"
    # -L's redirects each dump their headers: the last Last-Modified is the file's.
    sed -n 's/^[Ll]ast-[Mm]odified: *//p' "$headers" | tr -d '\r' | tail -n 1 > "$tarball.date"
    rm -f "$headers"
}
if [[ -f $tarball && -f $tarball.sig ]] && verify; then
    origin="cached tarball"
    if [[ -s $tarball.date ]]; then origin=$(<"$tarball.date"); fi
else
    if [[ -f $tarball || -f $tarball.sig ]]; then
        echo "cached Arch Linux ARM tarball incomplete or unverified: downloading it" >&2
    fi
    download
    verify || { echo "Arch Linux ARM tarball signature check failed (signer $signer)" >&2; exit 1; }
    origin=$(<"$tarball.date")
    if [[ -z $origin ]]; then origin="downloaded tarball (no Last-Modified)"; fi
fi
# agentpc reads the pair back as the agent user.
chown -R agent:agent "$cache"
chmod -R a+rX "$cache"
echo "alarm-tarball: $origin"

echo "==> partitioning and unpacking"
# A 512 MiB EFI system partition, which becomes /boot (systemd-boot loads the kernel from it),
# and the root filesystem.
sgdisk -Z "$disk" >/dev/null
sgdisk -n1:0:+512M -t1:ef00 -c1:ESP -n2:0:0 -t2:8300 -c2:root "$disk" >/dev/null
partprobe "$disk"; udevadm settle
mkfs.vfat -F32 -n ESP "${disk}1" >/dev/null
mkfs.ext4 -q -F -L archroot "${disk}2"

root=/mnt/arch
mkdir -p "$root"
mount "${disk}2" "$root"
# As root, so ownership and extended attributes survive. FAT can't hold them, so /boot is
# unpacked onto ext4 first and copied onto the ESP.
bsdtar -xpf "$tarball" -C "$root"
mkdir -p /tmp/esp
mount "${disk}1" /tmp/esp
cp -r "$root/boot/." /tmp/esp/
rm -rf "${root:?}/boot/"*
umount /tmp/esp
mount "${disk}1" "$root/boot"

cat > "$root/root/setup.sh" <<'SETUP'
#!/bin/bash
set -euo pipefail
echo "==> installing packages (XFCE, Chromium, ...)"
pacman-key --init
pacman-key --populate archlinuxarm
sed -i 's/^#ParallelDownloads.*/ParallelDownloads = 8/' /etc/pacman.conf
pacman -Syu --noconfirm
pacman -S --noconfirm --needed xorg-server xfce4 lightdm lightdm-gtk-greeter chromium \
    openssh sudo dmidecode libxi at-spi2-core xdotool curl git

# The tarball's default user would hold uid 1000, which agentpc's desktop session paths use.
userdel -r alarm 2>/dev/null || true
useradd -m -u 1000 -G wheel -s /bin/bash agent
echo agent:agent | chpasswd
echo 'agent ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/agent
chmod 440 /etc/sudoers.d/agent
echo arch-agent > /etc/hostname
ln -sf /usr/share/zoneinfo/UTC /etc/localtime
sed -i 's/^#en_US.UTF-8/en_US.UTF-8/' /etc/locale.gen
locale-gen >/dev/null
echo LANG=en_US.UTF-8 > /etc/locale.conf

# X11 session with autologin (cua-driver fully supports X11); a blanked screen blocks
# screenshots and input.
groupadd -rf autologin
gpasswd -a agent autologin >/dev/null
mkdir -p /etc/lightdm/lightdm.conf.d /etc/X11/xorg.conf.d
printf '[Seat:*]\nautologin-user=agent\nautologin-session=xfce\nuser-session=xfce\n' \
    > /etc/lightdm/lightdm.conf.d/50-agent.conf
cat > /etc/X11/xorg.conf.d/10-noblank.conf <<'EOF'
Section "ServerFlags"
  Option "BlankTime" "0"
  Option "StandbyTime" "0"
  Option "SuspendTime" "0"
  Option "OffTime" "0"
EndSection
EOF

# Images are shared, so SSH trusts whichever Mac boots them: agentpc passes its public key as
# an SMBIOS OEM string (-smbios type=11), installed at every boot (as on the Ubuntu image).
cat > /usr/local/sbin/agentpc-ssh-key <<'EOF'
#!/bin/sh
key=$(dmidecode -t 11 2>/dev/null | sed -n 's/^.*agentpc-ssh-key=//p' | head -n 1)
[ -n "$key" ] || exit 0
install -d -m 700 -o agent -g agent /home/agent/.ssh
printf '%s\n' "$key" > /home/agent/.ssh/authorized_keys
chown agent:agent /home/agent/.ssh/authorized_keys
chmod 600 /home/agent/.ssh/authorized_keys
EOF
chmod 755 /usr/local/sbin/agentpc-ssh-key
cat > /etc/systemd/system/agentpc-ssh-key.service <<'EOF'
[Unit]
Description=Install the agentpc host's SSH key
Before=sshd.service
[Service]
Type=oneshot
ExecStart=/usr/local/sbin/agentpc-ssh-key
[Install]
WantedBy=multi-user.target
EOF
systemctl enable agentpc-ssh-key.service sshd.service lightdm.service \
    systemd-networkd.service systemd-resolved.service
systemctl set-default graphical.target

echo "==> installing the bootloader and cua-driver"
# virtio-gpu in the initramfs, so the display comes up at boot and after kernel updates.
grep -q 'virtio_gpu' /etc/mkinitcpio.conf ||
    sed -i 's/^MODULES=(\(.*\))/MODULES=(\1 virtio_gpu)/' /etc/mkinitcpio.conf
mkinitcpio -P >/dev/null

# systemd-boot at the removable-media path, so the empty UEFI variable store a new image
# starts with still finds it.
bootctl install --no-variables --esp-path=/boot >/dev/null
printf 'default arch.conf\ntimeout 0\n' > /boot/loader/loader.conf
printf 'title Arch Linux ARM\nlinux /Image\ninitrd /initramfs-linux.img\noptions root=LABEL=archroot rw console=tty0 console=ttyAMA0\n' \
    > /boot/loader/entries/arch.conf
printf 'LABEL=archroot / ext4 defaults 0 1\nLABEL=ESP /boot vfat defaults,umask=0077 0 2\n' > /etc/fstab

# Pinned and checked like the Ubuntu image's (guests/ubuntu/user-data; keep the sha256s in
# sync); telemetry off.
su - agent -c 'set -e; d=$(mktemp -d); cd "$d"
  raw=https://raw.githubusercontent.com/trycua/cua/cua-driver-rs-v0.30.3/libs/cua-driver/scripts
  curl -fsSLO https://github.com/trycua/cua/releases/download/cua-driver-rs-v0.30.3/install.sh
  curl -fsSLO "$raw/_install-rust.sh"
  curl -fsSLO "$raw/_install-common.sh"
  printf "%s\n" \
    "317ba3a49fdba10f2a7f1b9f392c1bc1b7657f3aae85e1e2e43684cf17a1bf3b  install.sh" \
    "2d7aa18f56b33a04cf79e001f4c48abdee7b54234e5fd5868c9327b4a47568e6  _install-rust.sh" \
    "5bc3aa010eb8667a099b582a9ada9a8f93001745b842cc7cf3cc6c472520cf29  _install-common.sh" |
    sha256sum -c --quiet || { echo "cua-driver installer checksum mismatch" >&2; exit 1; }
  CUA_DRIVER_RS_VERSION=0.30.3 CUA_DRIVER_RS_TELEMETRY_ENABLED=0 bash ./install.sh >/dev/null
  cd ~/.cua-driver/packages/releases/0.30.3-aarch64-unknown-linux-gnu
  printf "%s\n" \
    "b9d31159bc1358c7069173cea0e025e8b3b3febad50cfce8059548a943e87187  cua-driver" \
    "f9f9e0db8cb12ce631bb6b74deec0ece7823bb4e3ff31d48b493e9c4b590c2b9  cua-cursor-theme" |
    sha256sum -c --quiet || { echo "installed cua-driver checksum mismatch" >&2; exit 1; }
  ~/.local/bin/cua-driver telemetry disable >/dev/null; rm -rf "$d"'
# agentpc's readiness check looks for this, as on the cloud-init-provisioned Ubuntu image.
mkdir -p /var/lib/cloud
touch /var/lib/cloud/agent-ready
# Downloaded packages (pacman -Scc asks twice, and yes would die of SIGPIPE under pipefail).
rm -rf /var/cache/pacman/pkg/*
SETUP
chmod 755 "$root/root/setup.sh"
arch-chroot "$root" /root/setup.sh
rm "$root/root/setup.sh"
umount -R "$root"
sync
echo "arch image built"

#!/bin/bash
# Builds the Arch Linux ARM image onto a blank disk (the only argument, e.g. /dev/vdb). Runs as
# root in a throwaway Ubuntu VM: macOS can't make ext4 or run pacman, and an arm64 Ubuntu
# can chroot into Arch Linux ARM natively. agentpc then turns the disk into the image and
# captures its snapshot, which runs guests/arch/prepare.sh.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
disk=${1:?usage: build.sh <disk>}

echo "==> downloading Arch Linux ARM"
apt-get -o DPkg::Lock::Timeout=300 install -y -q arch-install-scripts libarchive-tools dosfstools \
    gdisk gnupg >/dev/null

# Arch Linux ARM's generic aarch64 root filesystem. Its download host has no valid HTTPS, so
# the tarball is checked against the Arch Linux ARM Build System key, pinned by fingerprint
# (listed at archlinuxarm.org/about/package-signing), which is fetched over HTTPS.
signer=68B3537F39A313B3E574D06777193F152BDBE6A6
tarball=ArchLinuxARM-aarch64-latest.tar.gz
cd /tmp
curl -fsSL -o "$tarball" "http://os.archlinuxarm.org/os/$tarball"
curl -fsSL -o "$tarball.sig" "http://os.archlinuxarm.org/os/$tarball.sig"
export GNUPGHOME=/tmp/alarm-gnupg
rm -rf "$GNUPGHOME"; install -d -m 700 "$GNUPGHOME"
curl -fsSL "https://keyserver.ubuntu.com/pks/lookup?op=get&search=0x$signer" | gpg -q --import
gpg -q --status-fd 1 --verify "$tarball.sig" "$tarball" | grep -q "VALIDSIG $signer"
echo "alarm-tarball: $(curl -fsSI "http://os.archlinuxarm.org/os/$tarball" -L |
    sed -n 's/^[Ll]ast-[Mm]odified: *//p' | tr -d '\r' | tail -n 1)"

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
rm -f "$tarball" "$tarball.sig"

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

# Pinned like the Ubuntu image's (guests/ubuntu/user-data); telemetry off.
su - agent -c 'set -e; d=$(mktemp -d); cd "$d"
  curl -fsSLO https://github.com/trycua/cua/releases/download/cua-driver-rs-v0.30.3/install.sh
  curl -fsSLO https://raw.githubusercontent.com/trycua/cua/cua-driver-rs-v0.30.3/libs/cua-driver/scripts/_install-rust.sh
  CUA_DRIVER_RS_VERSION=0.30.3 CUA_DRIVER_RS_TELEMETRY_ENABLED=0 bash ./install.sh >/dev/null
  ~/.local/bin/cua-driver telemetry disable >/dev/null; rm -rf "$d"'
# agentpc's readiness check looks for this, as on the cloud-init-provisioned Ubuntu image.
mkdir -p /var/lib/cloud
touch /var/lib/cloud/agent-ready
yes | pacman -Scc >/dev/null
SETUP
chmod 755 "$root/root/setup.sh"
arch-chroot "$root" /root/setup.sh
rm "$root/root/setup.sh"
umount -R "$root"
sync
echo "arch image built"

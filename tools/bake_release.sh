#!/bin/bash
# bake_release.sh — Build a distributable oxigotchi SD card image from scratch
# Run inside WSL: sudo bash /mnt/c/msys64/home/user/oxigotchi/tools/bake_release.sh
#
# Requirements:
#   - A base Raspberry Pi OS Lite (64-bit, Bookworm+) .img file
#   - Cross-compiled oxigotchi binary at rust/target/aarch64-unknown-linux-gnu/release/oxigotchi
#   - pishrink.sh installed (/usr/local/bin/pishrink.sh)
#   - WSL with losetup, zip, aarch64-linux-gnu-gcc
#
# Output: oxigotchi-release.img.zip in the repo root
set -euo pipefail

echo "============================================="
echo "=== Oxigotchi Release Image Builder       ==="
echo "============================================="
echo ""

# ─── Configuration ───
REPO=/mnt/c/msys64/home/${USER:-$(whoami)}/oxigotchi
BASE_IMG="${1:-/mnt/d/oxigotchi-release.img}"
RELEASE_IMG="/mnt/d/oxigotchi-release.img"
BINARY="$REPO/rust/target/aarch64-unknown-linux-gnu/release/oxigotchi"
VERSION="3.3.0"

if [ ! -f "$BASE_IMG" ]; then
    echo "ERROR: Base image not found at $BASE_IMG"
    echo "Usage: sudo bash $0 [path-to-base-pi-os.img]"
    exit 1
fi

if [ ! -f "$BINARY" ]; then
    echo "ERROR: Cross-compiled binary not found at $BINARY"
    echo "Build it first: cd rust && cargo build --release --target aarch64-unknown-linux-gnu"
    exit 1
fi

# Verify binary is aarch64
if ! file "$BINARY" | grep -q "aarch64\|ARM aarch64"; then
    echo "ERROR: Binary is not aarch64: $(file "$BINARY")"
    exit 1
fi

# ─── 0a. Cross-compile wlan_keepalive ───
echo "=== 0a. Cross-compile wlan_keepalive ==="
aarch64-linux-gnu-gcc -O2 -static -o /tmp/wlan_keepalive "$REPO/tools/wlan_keepalive.c"
file /tmp/wlan_keepalive
echo "  Done"

# ─── 0b. Copy base image ───
echo ""
echo "=== 0b. Copy base image ==="
if [ "$(realpath "$BASE_IMG")" != "$(realpath "$RELEASE_IMG")" ]; then
    cp "$BASE_IMG" "$RELEASE_IMG"
    echo "  Copied to $RELEASE_IMG"
else
    echo "  Base IS release image — working in-place"
fi

# ─── 0c. Cleanup previous mounts ───
echo ""
echo "=== 0c. Cleanup previous mounts ==="
sudo umount /mnt/piroot 2>/dev/null || true
sudo umount /mnt/piboot 2>/dev/null || true
sudo losetup -D 2>/dev/null || true
sleep 1

# ─── 1. Mount image ───
echo ""
echo "=== 1. Mount image ==="
sudo losetup -fP "$RELEASE_IMG"
LOOPDEV=$(losetup -j "$RELEASE_IMG" | head -1 | cut -d: -f1)
echo "Loop device: $LOOPDEV"
# Give kernel time to create partition devices; use partprobe if needed
sleep 2
if [ ! -b "${LOOPDEV}p2" ]; then
    echo "  Partition devices missing — running partprobe..."
    sudo partprobe "$LOOPDEV"
    sleep 2
fi
if [ ! -b "${LOOPDEV}p2" ]; then
    echo "ERROR: Partition devices still missing after partprobe"
    sudo losetup -D
    exit 1
fi
sudo mkdir -p /mnt/piboot /mnt/piroot
sudo mount "${LOOPDEV}p2" /mnt/piroot
sudo mount "${LOOPDEV}p1" /mnt/piboot
PI=/mnt/piroot
echo "  Image mounted at $PI"

# Trap to ensure cleanup on error
cleanup() {
    echo ""
    echo "=== Cleanup: unmounting ==="
    sync
    sudo umount /mnt/piboot 2>/dev/null || true
    sudo umount /mnt/piroot 2>/dev/null || true
    sudo losetup -D 2>/dev/null || true
}
trap cleanup EXIT

# ─── 2. Install oxigotchi binary ───
echo ""
echo "=== 2. Install oxigotchi binary ==="
sudo cp "$BINARY" "$PI/usr/local/bin/rusty-oxigotchi"
sudo chmod +x "$PI/usr/local/bin/rusty-oxigotchi"
sudo chown root:root "$PI/usr/local/bin/rusty-oxigotchi"
file "$PI/usr/local/bin/rusty-oxigotchi"
echo "  Binary installed"

# ─── 3. Config directory ───
echo ""
echo "=== 3. Create config directory ==="
sudo mkdir -p "$PI/etc/oxigotchi"
cat > /tmp/oxigotchi-config.toml <<'CFGEOF'
# Oxigotchi default config — edit to match your setup
# See https://github.com/YOUR_REPO/oxigotchi for documentation

[general]
name = "oxigotchi"
# WiFi aggression: 1=Chill, 2=Lurk, 3=Prowl, 4=Hunt, 5=RAGE, 6=FURY, 7=YOLO
# Level 1 is safest for new users — scan only, no active attacks
rage_level = 1

[wifi]
# Networks to never attack (your home network)
whitelist = ["YourNetwork", "YourNetwork-5G"]

[bluetooth]
enabled = true
phone_name = ""
auto_connect = true

[wpa_sec]
# Get your API key from https://wpa-sec.stanev.org
api_key = ""

[display]
# Waveshare e-ink model: "2in13_v4" for most Pi Zero 2W setups
model = "2in13_v4"
CFGEOF
sudo cp /tmp/oxigotchi-config.toml "$PI/etc/oxigotchi/config.toml"
echo "  Config installed at /etc/oxigotchi/config.toml"

# ─── 4. Lua plugins ───
echo ""
echo "=== 4. Install Lua plugins ==="
sudo mkdir -p "$PI/etc/oxigotchi/plugins"
for p in "$REPO"/rust/plugins/*.lua; do
    sudo cp "$p" "$PI/etc/oxigotchi/plugins/$(basename "$p")"
    echo "  Copied $(basename "$p")"
done
echo "  $(ls "$REPO"/rust/plugins/*.lua | wc -l) plugins installed"

# ─── 5. Face PNGs ───
echo ""
echo "=== 5. Install face PNGs ==="
sudo mkdir -p "$PI/etc/oxigotchi/faces"
for f in "$REPO"/faces/eink/*.png; do
    sudo cp "$f" "$PI/etc/oxigotchi/faces/$(basename "$f")"
done
echo "  $(ls "$REPO"/faces/eink/*.png | wc -l) face PNGs installed"

# ─── 6. Helper scripts ───
echo ""
echo "=== 6. Install helper scripts ==="
# WiFi recovery
sudo cp "$REPO/tools/wifi-recovery.sh" "$PI/usr/local/bin/wifi-recovery.sh"
# Boot diagnostics
sudo cp "$REPO/tools/bootlog.sh" "$PI/usr/local/bin/bootlog.sh"
# USB0 fallback
sudo cp "$REPO/scripts/usb0-fallback.sh" "$PI/usr/local/bin/usb0-fallback.sh"
sudo cp "$REPO/tools/pisugar-single-tap.sh" "$PI/usr/local/bin/pisugar-single-tap.sh"
sudo cp "$REPO/tools/pisugar-double-tap.sh" "$PI/usr/local/bin/pisugar-double-tap.sh"
sudo cp "$REPO/tools/pisugar-long-press.sh" "$PI/usr/local/bin/pisugar-long-press.sh"
# WiFi ndev fix
sudo cp "$REPO/scripts/fix_ndev_on_boot.sh" "$PI/usr/local/bin/fix_ndev_on_boot.sh"
# BT keepalive
sudo cp "$REPO/scripts/bt-keepalive.sh" "$PI/usr/local/bin/bt-keepalive.sh"
# Buffer cleaner
sudo cp "$REPO/scripts/buffer-cleaner.sh" "$PI/usr/local/bin/buffer-cleaner.sh"
# wlan_keepalive binary
sudo cp /tmp/wlan_keepalive "$PI/usr/local/bin/wlan_keepalive"

# CRLF fix + chmod all scripts
for f in "$PI"/usr/local/bin/*.sh "$PI/usr/local/bin/wlan_keepalive"; do
    if [ -f "$f" ]; then
        sudo sed -i 's/\r$//' "$f"
        sudo chmod +x "$f"
    fi
done
echo "  All scripts installed, CRLF fixed, chmod +x"

# ─── 7. Systemd services ───
echo ""
echo "=== 7. Install systemd services ==="
SVC_DIR="$PI/etc/systemd/system"

# Copy service files from repo
for svc in rusty-oxigotchi resize-rootfs emergency-ssh wifi-recovery wlan-keepalive pisugar-button-init \
           bootlog usb0-fallback fix-ndev nm-watchdog bt-agent bt-keepalive \
           buffer-cleaner epd-startup wifi-watchdog; do
    if [ -f "$REPO/services/${svc}.service" ]; then
        sudo cp "$REPO/services/${svc}.service" "$SVC_DIR/${svc}.service"
        sudo sed -i 's/\r$//' "$SVC_DIR/${svc}.service"
        sudo chmod 644 "$SVC_DIR/${svc}.service"
        echo "  Installed: ${svc}.service"
    fi
done

# Timer units
for timer in bt-keepalive buffer-cleaner nm-watchdog; do
    if [ -f "$REPO/services/${timer}.timer" ]; then
        sudo cp "$REPO/services/${timer}.timer" "$SVC_DIR/${timer}.timer"
        sudo sed -i 's/\r$//' "$SVC_DIR/${timer}.timer"
        sudo chmod 644 "$SVC_DIR/${timer}.timer"
        echo "  Installed: ${timer}.timer"
    fi
done

# Path unit for patches
if [ -f "$REPO/services/oxigotchi-patches.path" ]; then
    sudo cp "$REPO/services/oxigotchi-patches.path" "$SVC_DIR/oxigotchi-patches.path"
    sudo cp "$REPO/services/oxigotchi-patches.service" "$SVC_DIR/oxigotchi-patches.service"
    sudo sed -i 's/\r$//' "$SVC_DIR/oxigotchi-patches.path" "$SVC_DIR/oxigotchi-patches.service"
    sudo chmod 644 "$SVC_DIR/oxigotchi-patches.path" "$SVC_DIR/oxigotchi-patches.service"
    echo "  Installed: oxigotchi-patches.path + .service"
fi

# ─── 8. Enable services ───
echo ""
echo "=== 8. Enable services ==="
sudo mkdir -p "$SVC_DIR/multi-user.target.wants"
sudo mkdir -p "$SVC_DIR/timers.target.wants"

# Core services
for svc in rusty-oxigotchi resize-rootfs emergency-ssh wifi-recovery wlan-keepalive pisugar-button-init \
           bootlog usb0-fallback fix-ndev bt-agent wifi-watchdog; do
    if [ -f "$SVC_DIR/${svc}.service" ]; then
        sudo ln -sf "/etc/systemd/system/${svc}.service" "$SVC_DIR/multi-user.target.wants/${svc}.service"
        echo "  Enabled: $svc"
    fi
done

# Timers
for timer in bt-keepalive buffer-cleaner nm-watchdog; do
    if [ -f "$SVC_DIR/${timer}.timer" ]; then
        sudo ln -sf "/etc/systemd/system/${timer}.timer" "$SVC_DIR/timers.target.wants/${timer}.timer"
        echo "  Enabled timer: $timer"
    fi
done

# Mask pisugar-server — oxigotchi manages PiSugar directly via I2C
sudo chroot "$PI" systemctl mask pisugar-server 2>/dev/null || true
sudo chroot "$PI" systemctl mask pisugar-watchdog.timer 2>/dev/null || true
echo "  Masked pisugar-server and pisugar-watchdog.timer"

# PiSugar 3 runtime detection — disables pisugar-server if PiSugar detected on I2C
cat > "$PI/usr/local/bin/disable-pisugar-server.sh" << 'PSEOF'
#!/bin/bash
# Detect PiSugar 3 on I2C and disable pisugar-server if present.
if python3 -c "
import fcntl, os
bus = os.open('/dev/i2c-1', os.O_RDWR)
fcntl.ioctl(bus, 0x0703, 0x57)
os.read(bus, 1)
os.close(bus)
" 2>/dev/null; then
    logger "oxigotchi: PiSugar 3 detected, disabling pisugar-server"
    systemctl disable --now pisugar-server 2>/dev/null
    systemctl disable --now pisugar-watchdog.timer 2>/dev/null
fi
PSEOF
sudo chmod +x "$PI/usr/local/bin/disable-pisugar-server.sh"

# ─── 9. Hostname ───
echo ""
echo "=== 9. Set hostname ==="
echo "oxigotchi" | sudo tee "$PI/etc/hostname" > /dev/null
sudo sed -i '/^127\.0\.1\.1[[:space:]]/s/[[:space:]].*$/\toxigotchi/' "$PI/etc/hosts"
if ! grep -q '127.0.1.1' "$PI/etc/hosts"; then
    echo "127.0.1.1	oxigotchi" | sudo tee -a "$PI/etc/hosts" > /dev/null
fi
echo "  Hostname: oxigotchi"

# ─── 10. User setup (pi:raspberry) ───
echo ""
echo "=== 10. User setup ==="
# Set password via first-boot service (chpasswd on the Pi itself is reliable;
# cross-compiled openssl hashes can fail due to glibc/PAM differences)
cat <<'UNIT' | sudo tee "$PI/etc/systemd/system/oxigotchi-firstboot.service" > /dev/null
[Unit]
Description=Oxigotchi first-boot setup
After=multi-user.target
ConditionPathExists=/etc/oxigotchi/.firstboot

[Service]
Type=oneshot
ExecStart=/bin/bash -c 'echo "pi:raspberry" | chpasswd && rm /etc/oxigotchi/.firstboot && systemctl disable oxigotchi-firstboot.service'
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
UNIT
sudo mkdir -p "$PI/etc/oxigotchi"
sudo touch "$PI/etc/oxigotchi/.firstboot"
sudo ln -sf /etc/systemd/system/oxigotchi-firstboot.service \
    "$PI/etc/systemd/system/multi-user.target.wants/oxigotchi-firstboot.service"
echo "  Password: raspberry (set by first-boot service)"

# ─── 11. SSH configuration ───
echo ""
echo "=== 11. SSH configuration ==="
sudo mkdir -p "$PI/etc/ssh/sshd_config.d"
printf 'PasswordAuthentication yes\nPermitRootLogin yes\nKbdInteractiveAuthentication yes\nUseDNS no\n' | sudo tee "$PI/etc/ssh/sshd_config.d/99-oxigotchi.conf" > /dev/null

# Generate SSH host keys (fresh, unique per image build)
sudo rm -f "$PI"/etc/ssh/ssh_host_*
sudo ssh-keygen -A -f "$PI" 2>&1 || true
sudo chmod 600 $PI/etc/ssh/ssh_host_*_key 2>/dev/null || true
sudo chmod 644 $PI/etc/ssh/ssh_host_*_key.pub 2>/dev/null || true

# NO authorized_keys — public release, password auth only
# Remove immutable bits first (base image may have chattr +i on authorized_keys)
sudo chattr -i "$PI/home/pi/.ssh/authorized_keys" 2>/dev/null || true
sudo rm -rf "$PI/home/pi/.ssh" 2>/dev/null || true
echo "  SSH: password auth, no authorized_keys, fresh host keys"

# ─── 12. NetworkManager dual-IP config ───
echo ""
echo "=== 12. NetworkManager config ==="
NM_DIR="$PI/etc/NetworkManager/system-connections"
# Remove ALL existing NM connections (may contain personal BT tether profiles)
sudo rm -f "$NM_DIR"/*.nmconnection 2>/dev/null
# Clean NM internal state (caches device names, connection history)
sudo rm -rf "$PI/var/lib/NetworkManager/internal-"* 2>/dev/null
sudo rm -f "$PI/var/lib/NetworkManager/seen-bssids" 2>/dev/null
sudo rm -f "$PI/var/lib/NetworkManager/timestamps" 2>/dev/null
sudo rm -f "$PI/var/lib/NetworkManager/NetworkManager-intern.conf" 2>/dev/null
sudo rm -f "$PI/var/lib/NetworkManager/NetworkManager.state" 2>/dev/null
sudo mkdir -p "$NM_DIR"

cat > /tmp/nm-usb0.conf <<'NM_USB'
[connection]
id=USB Gadget
uuid=69b44b21-30f2-4bbf-a0d9-af5637ef9e25
type=ethernet
autoconnect=true
autoconnect-priority=10
interface-name=usb0

[ethernet]

[ipv4]
address1=10.0.0.2/24
address2=192.168.137.2/24
gateway=10.0.0.1
dns=8.8.8.8;1.1.1.1;
method=manual

[ipv6]
addr-gen-mode=default
method=disabled

[proxy]
NM_USB
sudo cp /tmp/nm-usb0.conf "$NM_DIR/USB Gadget.nmconnection"
sudo chmod 600 "$NM_DIR/USB Gadget.nmconnection"
echo "  Dual-IP: 10.0.0.2 + 192.168.137.2 on usb0"

# BT PAN unmanaged-devices config — prevents NM from hijacking bnep0
CONF_D="$PI/etc/NetworkManager/conf.d"
sudo mkdir -p "$CONF_D"
sudo cp "$REPO/config/99-bt-pan.conf" "$CONF_D/99-bt-pan.conf"
sudo chmod 644 "$CONF_D/99-bt-pan.conf"
echo "  NM: bnep* and bt-pan* marked unmanaged (conf.d/99-bt-pan.conf)"

# ─── 13. System configuration ───
echo ""
echo "=== 13. System configuration ==="

# Default target: multi-user (no desktop)
sudo ln -sf /lib/systemd/system/multi-user.target "$PI/etc/systemd/system/default.target"
echo "  Default target: multi-user"

# Neutralize rc.local
printf '#!/bin/bash\nexit 0\n' | sudo tee "$PI/etc/rc.local" > /dev/null
sudo chmod +x "$PI/etc/rc.local"
echo "  rc.local neutralized"

# Swap: 100MB
printf 'CONF_SWAPSIZE=100\nCONF_SWAPFILE=/var/swap\n' | sudo tee "$PI/etc/dphys-swapfile" > /dev/null
echo "  Swap: 100MB"

# tmpfs for /tmp
if ! grep -q '/tmp' "$PI/etc/fstab"; then
    echo 'tmpfs /tmp tmpfs defaults,noatime,nosuid,size=50m 0 0' | sudo tee -a "$PI/etc/fstab" > /dev/null
fi
echo "  /tmp: tmpfs"

# Disable apt daily timers
sudo rm -f "$PI/etc/systemd/system/timers.target.wants/apt-daily.timer" 2>/dev/null
sudo rm -f "$PI/etc/systemd/system/timers.target.wants/apt-daily-upgrade.timer" 2>/dev/null
echo "  apt daily timers disabled"

# Timezone: UTC (neutral for public release)
sudo ln -sf /usr/share/zoneinfo/UTC "$PI/etc/localtime" 2>/dev/null
echo "  Timezone: UTC"

# Hardware watchdog in config.txt
BOOT_CFG="$PI/boot/firmware/config.txt"
if [ ! -f "$BOOT_CFG" ]; then
    BOOT_CFG="/mnt/piboot/config.txt"
fi
if [ -f "$BOOT_CFG" ]; then
    if ! grep -q 'dtparam=watchdog=on' "$BOOT_CFG"; then
        echo 'dtparam=watchdog=on' | sudo tee -a "$BOOT_CFG" > /dev/null
    fi
    echo "  Hardware watchdog enabled"
fi

# Blacklist camera module
sudo mkdir -p "$PI/etc/modprobe.d"
echo "blacklist bcm2835_v4l2" | sudo tee "$PI/etc/modprobe.d/blacklist-camera.conf" > /dev/null
echo "  Camera module blacklisted"

# modules.conf — ensure brcmfmac loads
MODULES_CONF="$PI/etc/modules-load.d/modules.conf"
if [ -f "$MODULES_CONF" ]; then
    if ! grep -q 'brcmfmac' "$MODULES_CONF"; then
        echo 'brcmfmac' | sudo tee -a "$MODULES_CONF" > /dev/null
    fi
else
    sudo mkdir -p "$(dirname "$MODULES_CONF")"
    echo 'brcmfmac' | sudo tee "$MODULES_CONF" > /dev/null
fi
echo "  brcmfmac in modules.conf"

# Disable cloud-init
sudo mkdir -p "$PI/etc/cloud"
sudo touch "$PI/etc/cloud/cloud-init.disabled"
echo "  cloud-init disabled"

# ─── 14. Disable unwanted services ───
echo ""
echo "=== 14. Disable unwanted services ==="
for svc in ModemManager systemd-networkd usb0-ip rpi-usb-gadget-ics userconfig \
           pi-helper rpi-eeprom-update; do
    sudo rm -f "$PI/etc/systemd/system/multi-user.target.wants/${svc}.service" 2>/dev/null
    echo "  Disabled: $svc"
done
sudo rm -f "$PI/etc/systemd/network/10-usb0.network" 2>/dev/null

# Kill pwnagotchi and bettercap — rusty-oxigotchi replaces them entirely.
# Mask (not just disable) prevents re-enabling.
for svc in pwnagotchi bettercap; do
    sudo rm -f "$PI/etc/systemd/system/multi-user.target.wants/${svc}.service" 2>/dev/null
    sudo ln -sf /dev/null "$PI/etc/systemd/system/${svc}.service" 2>/dev/null
    echo "  Masked: $svc"
done
# Remove the old splash service — rusty-oxigotchi renders its own boot/shutdown faces
sudo rm -f "$PI/etc/systemd/system/oxigotchi-splash.service" 2>/dev/null
sudo rm -f "$PI/etc/systemd/system/sysinit.target.wants/oxigotchi-splash.service" 2>/dev/null
sudo rm -f "$PI/etc/systemd/system/pwnagotchi.service.d/pwnagotchi-splash-delay.conf" 2>/dev/null
sudo rmdir "$PI/etc/systemd/system/pwnagotchi.service.d" 2>/dev/null || true
echo "  Removed: oxigotchi-splash.service (rusty-oxigotchi handles display)"

# ─── 15. Handshake directory ───
echo ""
echo "=== 15. Handshake directory ==="
sudo mkdir -p "$PI/etc/oxigotchi/handshakes"
echo "  /etc/oxigotchi/handshakes created"

# ─── 16. Clean bloat ───
echo ""
echo "=== 16. Clean disk bloat ==="
sudo rm -rf "$PI/root/.rustup" 2>/dev/null
sudo rm -rf "$PI/root/go" 2>/dev/null
# Scrub ALL of /home/pi — wipe everything, then recreate the clean skeleton.
# This prevents dev artifacts (dumps, HCD files, scripts, keys) from leaking into releases.
sudo find "$PI/home/pi" -mindepth 1 -delete 2>/dev/null
sudo mkdir -p "$PI/home/pi/.ssh"
sudo chmod 700 "$PI/home/pi/.ssh"
echo "  /home/pi fully scrubbed (clean skeleton restored)"
# Add navigation symlinks and README for easy post-flash orientation
sudo ln -sfn /etc/oxigotchi          "$PI/home/pi/config"
sudo ln -sfn /etc/oxigotchi/plugins  "$PI/home/pi/plugins"
sudo ln -sfn /etc/oxigotchi/faces    "$PI/home/pi/faces"
sudo ln -sfn /usr/local/bin/rusty-oxigotchi "$PI/home/pi/rusty-oxigotchi"
sudo ln -sfn /etc/systemd/system     "$PI/home/pi/services"
sudo cp "$REPO/tools/README-pi.md"   "$PI/home/pi/README.md"
echo "  navigation symlinks and README deployed to /home/pi"
# Nuke ALL logs (journal contains NM/BT connection names and device MACs)
sudo rm -rf "$PI/var/log/journal" 2>/dev/null
sudo mkdir -p "$PI/var/log/journal"
sudo rm -f "$PI/var/log/syslog"* "$PI/var/log/daemon.log"* "$PI/var/log/messages"* 2>/dev/null
sudo rm -f "$PI/var/log/auth.log"* "$PI/var/log/kern.log"* 2>/dev/null
sudo rm -rf "$PI/var/log/cloud-init"* 2>/dev/null
sudo rm -f "$PI/var/log/btmp"* "$PI/var/log/wtmp"* "$PI/var/log/lastlog" 2>/dev/null
sudo rm -rf "$PI/var/cache/apt/archives/"*.deb 2>/dev/null
sudo rm -rf "$PI/usr/share/doc/"* 2>/dev/null
sudo rm -rf "$PI/usr/share/man/"* 2>/dev/null
# Clear bash/shell history
sudo rm -f "$PI/home/pi/.bash_history" 2>/dev/null
sudo rm -f "$PI/root/.bash_history" 2>/dev/null
# Clear runtime state (forces fresh defaults on first boot)
sudo rm -f "$PI/var/lib/oxigotchi/state.json" 2>/dev/null
sudo rm -f "$PI/var/lib/angryoxide/state" 2>/dev/null
# Clear Bluetooth bonds (may contain paired phone MACs)
sudo rm -rf "$PI/var/lib/bluetooth/"*/cache 2>/dev/null
for btdir in "$PI/var/lib/bluetooth/"*/; do
    [ -d "$btdir" ] && sudo find "$btdir" -maxdepth 1 -mindepth 1 -type d -exec rm -rf {} + 2>/dev/null
done
echo "  Bloat cleaned"

# ─── 17. Create sentinel ───
echo ""
echo "=== 17. Create sentinel file ==="
echo "oxigotchi v${VERSION} release $(date -u '+%Y-%m-%dT%H:%M:%SZ')" | sudo tee "$PI/.oxigotchi-baked" > /dev/null
echo "  Sentinel created"

# Remove rootfs-expanded sentinel so resize runs on first boot
sudo rm -f "$PI/var/lib/.rootfs-expanded" 2>/dev/null
echo "  rootfs-expanded sentinel removed (will auto-expand on first boot)"


# ═══════════════════════════════════════════════
# VERIFICATION
# ═══════════════════════════════════════════════
echo ""
echo "============================================="
echo "=== VERIFICATION ==="
echo "============================================="
echo ""
ERRORS=0

verify() {
    local label="$1"
    local path="$2"
    if [ -e "$path" ]; then
        echo "  OK: $label"
    else
        echo "  MISSING: $label ($path)"
        ERRORS=$((ERRORS+1))
    fi
}

echo "--- Binary ---"
verify "rusty-oxigotchi" "$PI/usr/local/bin/rusty-oxigotchi"
file "$PI/usr/local/bin/rusty-oxigotchi" 2>/dev/null | grep -o "aarch64\|ARM"

echo ""
echo "--- Config ---"
verify "config.toml" "$PI/etc/oxigotchi/config.toml"
verify "plugins dir" "$PI/etc/oxigotchi/plugins"
echo "  Plugin count: $(ls "$PI/etc/oxigotchi/plugins/"*.lua 2>/dev/null | wc -l)"
echo "  Face count: $(ls "$PI/etc/oxigotchi/faces/"*.png 2>/dev/null | wc -l)"

echo ""
echo "--- Hostname ---"
cat "$PI/etc/hostname"
grep '127.0.1.1' "$PI/etc/hosts" || echo "  WARNING: no 127.0.1.1 in hosts"

echo ""
echo "--- SSH ---"
echo "  Host keys: $(ls $PI/etc/ssh/ssh_host_*_key 2>/dev/null | wc -l)"
cat "$PI/etc/ssh/sshd_config.d/99-oxigotchi.conf" 2>/dev/null
if [ -f "$PI/home/pi/.ssh/authorized_keys" ]; then
    echo "  WARNING: authorized_keys exists (should be removed for public release)"
    ERRORS=$((ERRORS+1))
else
    echo "  OK: No authorized_keys (public release)"
fi

echo ""
echo "--- User ---"
if grep -q '^pi:' "$PI/etc/shadow"; then
    echo "  OK: pi user exists"
else
    echo "  MISSING: pi user in shadow"
    ERRORS=$((ERRORS+1))
fi

echo ""
echo "--- Services ---"
for svc in rusty-oxigotchi resize-rootfs emergency-ssh wifi-recovery wlan-keepalive pisugar-button-init \
           bootlog usb0-fallback fix-ndev bt-agent wifi-watchdog; do
    verify "$svc.service" "$PI/etc/systemd/system/${svc}.service"
done

echo ""
echo "--- Enabled services ---"
echo "  multi-user.target.wants:"
ls "$PI/etc/systemd/system/multi-user.target.wants/" 2>/dev/null | sort | sed 's/^/    /'
echo "  timers.target.wants:"
ls "$PI/etc/systemd/system/timers.target.wants/" 2>/dev/null | sort | sed 's/^/    /'

echo ""
echo "--- Helper scripts ---"
for f in wifi-recovery.sh bootlog.sh usb0-fallback.sh fix_ndev_on_boot.sh bt-keepalive.sh \
         buffer-cleaner.sh wlan_keepalive; do
    verify "$f" "$PI/usr/local/bin/$f"
done

echo ""
echo "--- CRLF check ---"
CRLF_BAD=0
for f in "$PI"/usr/local/bin/*.sh; do
    if [ -f "$f" ] && grep -qlP '\r' "$f" 2>/dev/null; then
        echo "  BAD CRLF: $(basename "$f")"
        CRLF_BAD=$((CRLF_BAD+1))
    fi
done
if [ "$CRLF_BAD" -eq 0 ]; then
    echo "  OK: No CRLF in scripts"
else
    ERRORS=$((ERRORS+CRLF_BAD))
fi

echo ""
echo "--- NetworkManager ---"
ls "$NM_DIR/"*.nmconnection 2>/dev/null | sed 's/^/  /'
verify "99-bt-pan.conf" "$CONF_D/99-bt-pan.conf"

echo ""
echo "--- First-boot auto-expand ---"
verify "resize-rootfs.service" "$SVC_DIR/resize-rootfs.service"
if [ -f "$PI/var/lib/.rootfs-expanded" ]; then
    echo "  BAD: rootfs-expanded sentinel exists (resize won't run)"
    ERRORS=$((ERRORS+1))
else
    echo "  OK: No sentinel (will auto-expand on first boot)"
fi

echo ""
echo "--- Personal info check ---"
# Check for personal SSIDs, emails, SSH keys
PERSONAL=0
if grep -rq "userind\|user@" "$PI/etc/oxigotchi/" 2>/dev/null; then
    echo "  BAD: Personal email found in config"
    PERSONAL=$((PERSONAL+1))
fi
if [ -f "$PI/home/pi/.ssh/authorized_keys" ]; then
    echo "  BAD: authorized_keys present"
    PERSONAL=$((PERSONAL+1))
fi
# Check /home/pi for dev artifacts (dumps, HCD files, scripts, etc.)
HOMEFILE_COUNT=$(sudo find "$PI/home/pi" -mindepth 1 \
    ! -path "$PI/home/pi/.ssh" ! -path "$PI/home/pi/.ssh/*" \
    ! -name "README.md" \
    ! -name "config" ! -name "plugins" ! -name "faces" \
    ! -name "rusty-oxigotchi" ! -name "services" \
    2>/dev/null | wc -l)
if [ "$HOMEFILE_COUNT" -gt 0 ]; then
    echo "  BAD: /home/pi contains $HOMEFILE_COUNT unexpected file(s):"
    sudo find "$PI/home/pi" -mindepth 1 \
        ! -path "$PI/home/pi/.ssh" ! -path "$PI/home/pi/.ssh/*" \
        ! -name "README.md" \
        ! -name "config" ! -name "plugins" ! -name "faces" \
        ! -name "rusty-oxigotchi" ! -name "services" \
        2>/dev/null | head -10 | sed 's/^/    /'
    PERSONAL=$((PERSONAL+1))
fi
if [ "$PERSONAL" -eq 0 ]; then
    echo "  OK: No personal info detected"
else
    ERRORS=$((ERRORS+PERSONAL))
fi

echo ""
echo "--- Disk usage ---"
sudo du -sh "$PI/" 2>/dev/null

# ─── Unmount ───
echo ""
echo "=== Unmounting ==="
# Remove trap so we don't double-unmount
trap - EXIT
sync
sudo umount /mnt/piboot
sudo umount /mnt/piroot
sudo losetup -D

echo ""
if [ "$ERRORS" -gt 0 ]; then
    echo "============================================="
    echo "=== IMAGE BAKED WITH $ERRORS ERRORS       ==="
    echo "============================================="
    echo ""
    echo "Fix errors and re-run. Image NOT shrunk/zipped."
    exit 1
fi

# ─── Scrub deleted data from raw blocks ───
echo ""
echo "=== Scrub: zeroing unused blocks + journal ==="
sudo losetup -fP "$RELEASE_IMG"
LOOPDEV2=$(losetup -j "$RELEASE_IMG" | head -1 | cut -d: -f1)
if [ -n "$LOOPDEV2" ]; then
    # Strip ext4 journal (contains cached copies of deleted data)
    sudo tune2fs -O ^has_journal "${LOOPDEV2}p2" 2>&1 | tail -2
    sudo e2fsck -fy "${LOOPDEV2}p2" 2>&1 | tail -3
    # Zero all unused blocks (including freed journal blocks)
    sudo zerofree -v "${LOOPDEV2}p2" 2>&1 | tail -3
    # Recreate a fresh empty journal
    sudo tune2fs -j "${LOOPDEV2}p2" 2>&1 | tail -2
    sudo losetup -D
    echo "  Journal stripped, blocks zeroed, fresh journal created"
else
    echo "  WARNING: Could not set up loop device for scrub"
fi

# ─── Shrink image ───
echo ""
echo "=== Shrinking image with pishrink.sh ==="
sudo pishrink.sh -s "$RELEASE_IMG" 2>&1 | tail -10
echo "  Shrink complete"

# ─── Zip ───
echo ""
echo "=== Zipping release image ==="
ZIP_OUT="/mnt/d/oxigotchi-v${VERSION}-release.img.zip"
rm -f "$ZIP_OUT"
cd /mnt/d && zip -9 "$(basename "$ZIP_OUT")" "$(basename "$RELEASE_IMG")"
echo ""
echo "============================================="
echo "=== RELEASE IMAGE READY                   ==="
echo "============================================="
echo ""
echo "Image: $RELEASE_IMG"
echo "Zip:   $ZIP_OUT"
echo "Size:  $(du -sh "$ZIP_OUT" | cut -f1)"
echo ""
echo "Users burn this to SD card, boot the Pi, and connect via:"
echo "  ssh pi@10.0.0.2  (password: raspberry)"
echo "  ssh pi@192.168.137.2  (Windows RNDIS fallback)"
echo ""
echo "First boot will auto-expand the filesystem to fill the SD card."

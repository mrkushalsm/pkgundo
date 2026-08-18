#!/usr/bin/env bash
# One-time setup: creates a disposable Arch Linux VM for testing pkgundo,
# and takes a "clean" snapshot to revert to before every test run.
#
# Deliberately does NOT use cloud-init: the Arch cloud image's cloud-init
# "Network Stage" hangs indefinitely inside a plain libvirt NAT network
# (confirmed by hours of testing — masking the systemd wait-online units
# didn't fix it either, the hang is inside cloud-init's own logic). Instead,
# the VM is configured directly on the disk image with virt-customize
# before it's ever booted, and cloud-init itself is masked off so it can
# never run and never hang.
#
# Prerequisites (Arch host):
#   sudo pacman -S virt-manager qemu-full libvirt dnsmasq xorriso guestfs-tools
#   sudo systemctl enable --now libvirtd
#   sudo usermod -aG libvirt "$USER"   # log out/in after this
#
# Usage: ./setup-vm.sh

cd "$(dirname "${BASH_SOURCE[0]}")"
source ./lib.sh

require_tools virt-install virsh qemu-img curl ssh-keygen virt-customize

echo "== Ensuring the libvirt 'default' network exists and is running =="
if ! virsh net-info default >/dev/null 2>&1; then
    virsh net-define /usr/share/libvirt/networks/default.xml
fi
# Just try starting it; "already active" is not a real failure.
NET_START_OUT="$(virsh net-start default 2>&1)" || \
    { echo "$NET_START_OUT" | grep -qi "already active" || { echo "$NET_START_OUT" >&2; exit 1; }; }
virsh net-autostart default >/dev/null 2>&1 || true

if virsh dominfo "$VM_NAME" >/dev/null 2>&1; then
    echo "VM '$VM_NAME' already exists. Remove it first if you want to recreate it:"
    echo "  virsh destroy $VM_NAME; virsh undefine $VM_NAME --remove-all-storage"
    exit 1
fi

echo "== Downloading Arch cloud image (skipped if already present) =="
if [ ! -f "$BASE_IMAGE" ]; then
    curl -L --fail -o "$BASE_IMAGE.tmp" "$ARCH_CLOUD_IMAGE_URL"
    mv "$BASE_IMAGE.tmp" "$BASE_IMAGE"
fi

echo "== Creating VM disk (standalone copy, not a backing-file chain) =="
qemu-img convert -O qcow2 "$BASE_IMAGE" "$VM_DISK"
qemu-img resize "$VM_DISK" "${VM_DISK_GB}G"

echo "== Generating SSH keypair for the test VM =="
if [ ! -f "$SSH_KEY" ]; then
    ssh-keygen -t ed25519 -N "" -f "$SSH_KEY" -C "pkgundo-vm-test"
fi

echo "== Configuring the VM offline (no cloud-init involved) =="
# The DHCP config below (20-wired.network) is normally written by cloud-init
# itself; since cloud-init is masked off (it hangs indefinitely in a plain
# libvirt NAT network — see header comment), the interface never gets
# brought up at all without writing this ourselves.
virt-customize -a "$VM_DISK" \
    --run-command 'useradd -m -G wheel -s /bin/bash pkgundo || true' \
    --password 'pkgundo:password:pkgundo' \
    --run-command 'echo "pkgundo ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/pkgundo && chmod 440 /etc/sudoers.d/pkgundo' \
    --ssh-inject "pkgundo:file:${SSH_KEY}.pub" \
    --run-command 'systemctl enable sshd' \
    --run-command 'systemctl mask cloud-init.service cloud-init-local.service cloud-config.service cloud-final.service systemd-networkd-wait-online.service NetworkManager-wait-online.service 2>/dev/null || true' \
    --run-command 'printf "[Match]\nName=en* eth*\n\n[Network]\nDHCP=yes\n" > /etc/systemd/network/20-wired.network' \
    --run-command 'systemctl enable systemd-networkd systemd-resolved' \
    --hostname pkgundo-test

echo "== Creating the VM (this boots it for the first time) =="
virt-install \
    --name "$VM_NAME" \
    --memory "$VM_RAM_MB" \
    --vcpus "$VM_VCPUS" \
    --disk "path=$VM_DISK,format=qcow2" \
    --os-variant archlinux \
    --network network=default \
    --graphics none \
    --console pty,target_type=serial \
    --import \
    --noautoconsole

echo "== Waiting for the VM to get an IP =="
IP=""
for _ in $(seq 1 30); do
    IP="$(vm_ip || true)"
    [ -n "$IP" ] && break
    sleep 5
done
if [ -z "$IP" ]; then
    echo "VM never got an IP address. Check 'virsh --connect qemu:///system console $VM_NAME' for boot issues." >&2
    exit 1
fi
echo "VM IP: $IP"

wait_for_ssh "$IP"

echo "== Growing the root filesystem to use the full resized disk =="
# qemu-img resize only grows the underlying disk file; cloud-init would
# normally grow the partition/filesystem to match (its growpart/resizefs
# modules). Without cloud-init, the guest is stuck with root's original
# ~1.7G partition on a much bigger disk, which isn't enough for the package
# install below. vda3 is the last partition on the disk, so growing it to
# fill the remaining space is safe (nothing after it to collide with).
ssh_vm "$IP" "sudo growpart /dev/vda 3 && sudo btrfs filesystem resize max /"

echo "== Installing rust/base-devel/git/rsync over SSH (has real network access, unlike offline image prep) =="
ssh_vm "$IP" "sudo pacman -Sy --noconfirm --needed rust base-devel git rsync"

echo "== Taking the 'clean' snapshot to revert to before every test =="
# A full (memory+disk) internal snapshot, so reverting resumes instantly at
# this exact running state instead of requiring a fresh boot each time.
virsh snapshot-create-as "$VM_NAME" "$SNAPSHOT_NAME" \
    "Freshly provisioned, before any pkgundo test run"

echo
echo "Done. VM '$VM_NAME' is ready at $IP, snapshot '$SNAPSHOT_NAME' saved."
echo "Login: pkgundo / pkgundo (console) or SSH with the key at $SSH_KEY"
echo "Run ./smoke-test.sh next."

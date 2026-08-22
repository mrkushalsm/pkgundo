#!/usr/bin/env bash
# One-time setup: creates a disposable Debian VM for testing pkgundo's
# apt/dpkg integration, and takes a "clean" snapshot to revert to before
# every test run. Sibling to setup-vm.sh (Arch) — same libvirt/SSH plumbing
# via lib.sh, but Debian's cloud image needs a genuinely different
# image-prep, package-install, and disk-resize sequence (different
# partition/filesystem layout, apt instead of pacman, `sudo` group instead
# of `wheel`, `ssh.service` instead of `sshd.service`).
#
# Deliberately does NOT use cloud-init, for the same reason as setup-vm.sh:
# cloud-init's "Network Stage" can hang indefinitely in a plain libvirt NAT
# network. The VM is configured directly on the disk image with
# virt-customize before it's ever booted, and cloud-init is masked off.
#
# Prerequisites (Arch host): same as setup-vm.sh.
#
# Usage:
#   VM_NAME=pkgundo-test-debian BASE_IMAGE_FILENAME=debian-base.qcow2 ./setup-vm-debian.sh
# (VM_NAME/BASE_IMAGE_FILENAME are exported by this script itself below —
# just run ./setup-vm-debian.sh directly.)

cd "$(dirname "${BASH_SOURCE[0]}")"

# Isolate this VM from the Arch one entirely — separate name (separate
# WORKDIR under /var/lib/libvirt/images/), separate base-image filename.
export VM_NAME="${VM_NAME:-pkgundo-test-debian}"
export BASE_IMAGE_FILENAME="${BASE_IMAGE_FILENAME:-debian-base.qcow2}"
source ./lib.sh

DEBIAN_CLOUD_IMAGE_URL="https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-genericcloud-amd64.qcow2"

require_tools virt-install virsh qemu-img curl ssh-keygen virt-customize

echo "== Ensuring the libvirt 'default' network exists and is running =="
if ! virsh net-info default >/dev/null 2>&1; then
    virsh net-define /usr/share/libvirt/networks/default.xml
fi
NET_START_OUT="$(virsh net-start default 2>&1)" || \
    { echo "$NET_START_OUT" | grep -qi "already active" || { echo "$NET_START_OUT" >&2; exit 1; }; }
virsh net-autostart default >/dev/null 2>&1 || true

if virsh dominfo "$VM_NAME" >/dev/null 2>&1; then
    echo "VM '$VM_NAME' already exists. Remove it first if you want to recreate it:"
    echo "  virsh destroy $VM_NAME; virsh undefine $VM_NAME --remove-all-storage"
    exit 1
fi

echo "== Downloading Debian cloud image (skipped if already present) =="
if [ ! -f "$BASE_IMAGE" ]; then
    curl -L --fail -o "$BASE_IMAGE.tmp" "$DEBIAN_CLOUD_IMAGE_URL"
    mv "$BASE_IMAGE.tmp" "$BASE_IMAGE"
fi

echo "== Creating VM disk (standalone copy, not a backing-file chain) =="
qemu-img convert -O qcow2 "$BASE_IMAGE" "$VM_DISK"
qemu-img resize "$VM_DISK" "${VM_DISK_GB}G"

echo "== Generating SSH keypair for the test VM =="
if [ ! -f "$SSH_KEY" ]; then
    ssh-keygen -t ed25519 -N "" -f "$SSH_KEY" -C "pkgundo-vm-test-debian"
fi

echo "== Configuring the VM offline (no cloud-init involved) =="
# Debian's cloud image ships cloud-init by default; mask it off the same
# way as the Arch script, for the same NAT-hang reason. `sudo` is the
# admin group on Debian (not `wheel`); the SSH service unit is
# `ssh.service` (not `sshd.service` like Arch's).
#
# Debian's cloud image also ships with NO ssh host keys baked in (by
# design, for image hygiene) and relies on cloud-init's `ssh` module to
# generate them on first boot. Since cloud-init is masked off, `ssh-keygen
# -A` must be run here instead, or sshd exits immediately on every start
# with "no hostkeys available" and the VM is unreachable forever.
virt-customize -a "$VM_DISK" \
    --run-command 'useradd -m -G sudo -s /bin/bash pkgundo || true' \
    --password 'pkgundo:password:pkgundo' \
    --run-command 'echo "pkgundo ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/pkgundo && chmod 440 /etc/sudoers.d/pkgundo' \
    --ssh-inject "pkgundo:file:${SSH_KEY}.pub" \
    --run-command 'ssh-keygen -A' \
    --run-command 'systemctl enable ssh || systemctl enable sshd || true' \
    --run-command 'systemctl mask cloud-init.service cloud-init-local.service cloud-config.service cloud-final.service systemd-networkd-wait-online.service NetworkManager-wait-online.service 2>/dev/null || true' \
    --run-command 'printf "[Match]\nName=en* eth*\n\n[Network]\nDHCP=yes\n" > /etc/systemd/network/20-wired.network' \
    --run-command 'systemctl enable systemd-networkd systemd-resolved || true' \
    --hostname pkgundo-test-debian

echo "== Creating the VM (this boots it for the first time) =="
virt-install \
    --name "$VM_NAME" \
    --memory "$VM_RAM_MB" \
    --vcpus "$VM_VCPUS" \
    --disk "path=$VM_DISK,format=qcow2" \
    --os-variant debian12 \
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
# Debian's genericcloud image uses a single GPT root partition (vda1, ext4)
# rather than Arch's multi-partition/btrfs layout — growpart + resize2fs,
# not btrfs filesystem resize.
ssh_vm "$IP" "sudo growpart /dev/vda 1 && sudo resize2fs /dev/vda1"

echo "== Installing build tooling + Rust over SSH (has real network access, unlike offline image prep) =="
ssh_vm "$IP" "sudo apt-get update && sudo apt-get install -y --no-install-recommends build-essential git rsync curl ca-certificates"
# rustup rather than Debian's repo rustc/cargo: avoids depending on however
# old/new Debian stable's packaged toolchain happens to be at test time.
ssh_vm "$IP" "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable"
# Non-interactive `ssh host cmd` invocations (exactly what ssh_vm/apt-hook-test.sh
# use for every subsequent `cargo build`) don't source ~/.bashrc, so rustup's
# PATH-via-profile setup alone wouldn't be picked up — symlink the toolchain
# into /usr/local/bin, already on PATH for every shell type, instead.
ssh_vm "$IP" "sudo ln -sf /home/pkgundo/.cargo/bin/* /usr/local/bin/"

echo "== Taking the 'clean' snapshot to revert to before every test =="
virsh snapshot-create-as "$VM_NAME" "$SNAPSHOT_NAME" \
    "Freshly provisioned, before any pkgundo apt-hook test run"

echo
echo "Done. VM '$VM_NAME' is ready at $IP, snapshot '$SNAPSHOT_NAME' saved."
echo "Login: pkgundo / pkgundo (console) or SSH with the key at $SSH_KEY"
echo "Run ./apt-hook-test.sh next."

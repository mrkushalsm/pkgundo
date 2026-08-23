#!/usr/bin/env bash
# One-time setup: creates a disposable Fedora VM for testing pkgundo's
# dnf5 integration, and takes a "clean" snapshot to revert to before every
# test run. Sibling to setup-vm.sh (Arch) and setup-vm-debian.sh — same
# libvirt/SSH plumbing via lib.sh, but Fedora's cloud image needs its own
# image-prep/package-install/disk-resize sequence: `wheel` admin group
# (same as Arch, unlike Debian's `sudo`), `sshd.service` (same as Arch,
# unlike Debian's `ssh.service`), dnf instead of pacman/apt, and (based on
# the Debian phase's discovery that its cloud image ships no SSH host
# keys) `ssh-keygen -A` is included proactively from the start here rather
# than discovered via a failed VM run. Unlike Arch/Debian (which don't run
# NetworkManager by default), Fedora's cloud image ships NetworkManager as
# its actual default network stack, normally handed a DHCP connection
# profile by cloud-init at first boot — masking cloud-init here means that
# profile never gets written, so this script writes one directly instead
# of introducing systemd-networkd (which would just fight NetworkManager
# for the interface). This was discovered live: the first VM boot attempt
# got no IP at all despite reaching a working login prompt.
#
# Deliberately does NOT use cloud-init, for the same reason as the other
# two scripts: cloud-init's "Network Stage" can hang indefinitely in a
# plain libvirt NAT network. The VM is configured directly on the disk
# image with virt-customize before it's ever booted, and cloud-init is
# masked off.
#
# Prerequisites (Arch host): same as setup-vm.sh.
#
# Usage:
#   ./setup-vm-fedora.sh

cd "$(dirname "${BASH_SOURCE[0]}")"

# Isolate this VM from the Arch/Debian ones entirely — separate name
# (separate WORKDIR under /var/lib/libvirt/images/), separate base-image
# filename.
export VM_NAME="${VM_NAME:-pkgundo-test-fedora}"
export BASE_IMAGE_FILENAME="${BASE_IMAGE_FILENAME:-fedora-base.qcow2}"
source ./lib.sh

# Verified against the actual mirror redirect chain: download.fedoraproject.org
# 302s to a rotating set of mirrors, some of which 404 on a given file at a
# given moment — --retry handles that transient flakiness.
FEDORA_CLOUD_IMAGE_URL="https://download.fedoraproject.org/pub/fedora/linux/releases/42/Cloud/x86_64/images/Fedora-Cloud-Base-Generic-42-1.1.x86_64.qcow2"

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

echo "== Downloading Fedora cloud image (skipped if already present) =="
if [ ! -f "$BASE_IMAGE" ]; then
    curl -L --fail --retry 5 --retry-delay 3 -o "$BASE_IMAGE.tmp" "$FEDORA_CLOUD_IMAGE_URL"
    mv "$BASE_IMAGE.tmp" "$BASE_IMAGE"
fi

echo "== Creating VM disk (standalone copy, not a backing-file chain) =="
qemu-img convert -O qcow2 "$BASE_IMAGE" "$VM_DISK"
qemu-img resize "$VM_DISK" "${VM_DISK_GB}G"

echo "== Generating SSH keypair for the test VM =="
if [ ! -f "$SSH_KEY" ]; then
    ssh-keygen -t ed25519 -N "" -f "$SSH_KEY" -C "pkgundo-vm-test-fedora"
fi

echo "== Configuring the VM offline (no cloud-init involved) =="
# Fedora's cloud image ships cloud-init by default; mask it off the same
# way as the other two scripts, for the same NAT-hang reason. `wheel` is
# the admin group on Fedora (same as Arch, not Debian's `sudo`); the SSH
# service unit is `sshd.service` (same as Arch, not Debian's `ssh.service`).
# ssh-keygen -A included from the start (see header comment) since the
# Debian phase already discovered cloud images commonly ship with no host
# keys baked in.
#
# Two more issues discovered live (via a systemd-oneshot-diagnostic-service
# + offline virt-cat loop, since interactive console login proved too
# fragile to script reliably):
#   1. Fedora's cloud image runs NetworkManager (not systemd-networkd like
#      the other two scripts), and normally gets its DHCP connection
#      profile from cloud-init at first boot — masking cloud-init means
#      that profile never gets written, so one is written directly here.
#   2. `dbus-broker-launch` failed with a fatal -13 (EACCES) in this
#      environment (nested KVM under this session's sandboxed host),
#      cascading into NetworkManager ("Dependency failed"), sshd, logind,
#      and homed all failing too — a network-layer symptom with a
#      dbus-layer root cause. SELinux enforcing (Fedora's default, unlike
#      Arch/Debian which don't ship it) turned out to be the actual
#      culprit; setting it permissive fixed dbus-broker outright. The
#      `--audit`-flag override is kept too as harmless defense-in-depth,
#      since it was the first (insufficient on its own) fix attempted.
virt-customize -a "$VM_DISK" \
    --run-command 'useradd -m -G wheel -s /bin/bash pkgundo || true' \
    --password 'pkgundo:password:pkgundo' \
    --run-command 'echo "pkgundo ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/pkgundo && chmod 440 /etc/sudoers.d/pkgundo' \
    --ssh-inject "pkgundo:file:${SSH_KEY}.pub" \
    --run-command 'ssh-keygen -A' \
    --run-command 'systemctl enable sshd || true' \
    --run-command 'systemctl mask cloud-init.service cloud-init-local.service cloud-config.service cloud-final.service NetworkManager-wait-online.service 2>/dev/null || true' \
    --run-command 'mkdir -p /etc/NetworkManager/system-connections && printf "[connection]\nid=dhcp-any\ntype=ethernet\nautoconnect=true\n\n[ipv4]\nmethod=auto\n\n[ipv6]\nmethod=auto\n" > /etc/NetworkManager/system-connections/dhcp-any.nmconnection && chmod 600 /etc/NetworkManager/system-connections/dhcp-any.nmconnection' \
    --run-command 'systemctl enable NetworkManager || true' \
    --run-command 'sed -i "s/^SELINUX=.*/SELINUX=permissive/" /etc/selinux/config' \
    --run-command 'grubby --update-kernel=ALL --args="enforcing=0" 2>/dev/null || true' \
    --run-command 'mkdir -p /etc/systemd/system/dbus-broker.service.d && printf "[Service]\nExecStart=\nExecStart=/usr/bin/dbus-broker-launch --scope system\n" > /etc/systemd/system/dbus-broker.service.d/override.conf' \
    --hostname pkgundo-test-fedora

echo "== Creating the VM (this boots it for the first time) =="
virt-install \
    --name "$VM_NAME" \
    --memory "$VM_RAM_MB" \
    --vcpus "$VM_VCPUS" \
    --disk "path=$VM_DISK,format=qcow2" \
    --os-variant fedora42 \
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
# Verified directly via virt-filesystems (not guessed): Fedora 42's Cloud
# Base Generic image is GPT with 4 partitions (1=BIOS boot, 2=EFI, 3=ext4
# /boot, 4=btrfs root+home+var subvolumes) — no LVM at all, and btrfs
# rather than xfs. vda4 is the last partition, same growable-tail situation
# as Arch's own vda3 in setup-vm.sh, and the same `btrfs filesystem resize
# max` approach applies unchanged.
ssh_vm "$IP" "sudo growpart /dev/vda 4 && sudo btrfs filesystem resize max /"

echo "== Installing build tooling + Rust over SSH (has real network access, unlike offline image prep) =="
ssh_vm "$IP" "sudo dnf install -y --setopt=install_weak_deps=False gcc gcc-c++ make git rsync curl ca-certificates sqlite"
# rustup rather than Fedora's repo rustc/cargo: avoids depending on however
# old/new Fedora's packaged toolchain happens to be at test time — same
# reasoning as the Debian script.
ssh_vm "$IP" "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable"
# Non-interactive `ssh host cmd` invocations don't source ~/.bashrc, so
# rustup's own PATH setup alone wouldn't be picked up — symlink the
# toolchain into /usr/local/bin, already on PATH for every shell type.
ssh_vm "$IP" "sudo ln -sf /home/pkgundo/.cargo/bin/* /usr/local/bin/"

echo "== Taking the 'clean' snapshot to revert to before every test =="
virsh snapshot-create-as "$VM_NAME" "$SNAPSHOT_NAME" \
    "Freshly provisioned, before any pkgundo dnf-hook test run"

echo
echo "Done. VM '$VM_NAME' is ready at $IP, snapshot '$SNAPSHOT_NAME' saved."
echo "Login: pkgundo / pkgundo (console) or SSH with the key at $SSH_KEY"
echo "Run ./dnf-hook-test.sh next."

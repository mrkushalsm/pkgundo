#!/usr/bin/env bash
# Shared config/helpers for the pkgundo VM test scripts.
# Sourced by setup-vm.sh and smoke-test.sh — not meant to be run directly.

set -euo pipefail

# Force system libvirt (qemu:///system) rather than the per-user session
# instance. Session mode doesn't ship the pre-defined "default" NAT network
# that system mode does, which otherwise fails with:
#   ERROR  Network not found: no network with matching name 'default'
export LIBVIRT_DEFAULT_URI="${LIBVIRT_DEFAULT_URI:-qemu:///system}"

VM_NAME="${VM_NAME:-pkgundo-test}"
VM_RAM_MB="${VM_RAM_MB:-2048}"
VM_VCPUS="${VM_VCPUS:-2}"
VM_DISK_GB="${VM_DISK_GB:-15}"
SNAPSHOT_NAME="clean"

# Lives under libvirt's own image directory, not the repo/home dir: the
# system QEMU process runs as the unprivileged 'libvirt-qemu' user, which
# cannot traverse into a regular user's $HOME (permission denied on the
# parent directories, regardless of the disk file's own permissions).
# /var/lib/libvirt/images is already set up for it to reach.
WORKDIR="/var/lib/libvirt/images/${VM_NAME}"
BASE_IMAGE="$WORKDIR/arch-base.qcow2"
VM_DISK="$WORKDIR/${VM_NAME}.qcow2"
SSH_KEY="$WORKDIR/id_ed25519"
ARCH_CLOUD_IMAGE_URL="https://geo.mirror.pkgbuild.com/images/latest/Arch-Linux-x86_64-cloudimg.qcow2"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

mkdir -p "$WORKDIR" 2>/dev/null || {
    echo "One-time setup needed: $WORKDIR must exist and be owned by you." >&2
    echo "Run this once, then re-run this script:" >&2
    echo "  sudo mkdir -p $WORKDIR && sudo chown \"\$(id -u):\$(id -g)\" $WORKDIR" >&2
    exit 1
}

require_tools() {
    local missing=()
    for tool in "$@"; do
        command -v "$tool" >/dev/null 2>&1 || missing+=("$tool")
    done
    if [ "${#missing[@]}" -gt 0 ]; then
        echo "Missing required tools: ${missing[*]}" >&2
        echo "On Arch, install with:" >&2
        echo "  sudo pacman -S virt-manager qemu-full libvirt dnsmasq guestfs-tools" >&2
        echo "Then: sudo systemctl enable --now libvirtd" >&2
        exit 1
    fi
}

vm_ip() {
    # The default (lease-file) source goes stale after resuming a suspended
    # snapshot: the guest keeps its in-memory IP config across the resume
    # without a fresh DHCP handshake, so dnsmasq never re-logs a lease once
    # the original one has expired/been purged. Fall back to the live ARP
    # table, which reflects the guest's actual current address.
    local ip
    ip="$(virsh domifaddr "$VM_NAME" 2>/dev/null | awk '/ipv4/ {print $4}' | cut -d/ -f1)"
    if [ -z "$ip" ]; then
        ip="$(virsh domifaddr "$VM_NAME" --source arp 2>/dev/null | awk '/ipv4/ {print $4}' | cut -d/ -f1)"
    fi
    echo "$ip"
}

wait_for_ssh() {
    local ip="$1"
    echo "Waiting for SSH on $ip ..."
    echo "(if this takes a while, watch the boot live in another terminal with:"
    echo " virsh --connect qemu:///system console $VM_NAME  -- Ctrl+] to exit without killing the VM)"
    for i in $(seq 1 180); do
        if ssh -i "$SSH_KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
               -o ConnectTimeout=3 -o BatchMode=yes "pkgundo@$ip" true 2>/dev/null; then
            echo "SSH is up."
            return 0
        fi
        if [ $((i % 12)) -eq 0 ]; then
            echo "  ...still waiting ($((i * 5))s elapsed)"
        fi
        sleep 5
    done
    echo "Timed out waiting for SSH on $ip" >&2
    return 1
}

ssh_vm() {
    local ip="$1"
    shift
    ssh -i "$SSH_KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
        "pkgundo@$ip" "$@"
}

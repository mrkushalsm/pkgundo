# pkgundo VM smoke test

Runs a real `pkgundo run` -> `inspect` -> `rollback` cycle against a real
package install, inside a disposable VM you can revert instantly. This is
the only way to validate the parts of pkgundo that unit/integration tests
can't reach: live fanotify/inotify monitoring, live process tracking, and
rollback against a real system, without risking your actual machine.

## One-time setup

```bash
sudo pacman -S virt-manager qemu-full libvirt dnsmasq guestfs-tools
sudo systemctl enable --now libvirtd
sudo usermod -aG libvirt "$USER"   # log out/in for this to take effect

cd scripts/vm-test
./setup-vm.sh
```

This downloads an Arch cloud image, configures it offline with
`virt-customize` (user, SSH key, sudo, sshd enabled — cloud-init is
deliberately masked off, it hangs indefinitely in a plain libvirt NAT
network and isn't used here at all), boots it, installs rust/base-devel/git
over SSH, and takes a "clean" snapshot to revert to before every test.
Only needs to be run once (or again if you delete the VM).

Login for manual poking around: `pkgundo` / `pkgundo` (console), or
`ssh -i /var/lib/libvirt/images/pkgundo-test/id_ed25519 pkgundo@<ip>`.

## Running a test

```bash
./smoke-test.sh                       # default: pacman -S htop
./smoke-test.sh pacman -S fastfetch    # test a different package
```

Each run: reverts the VM to the clean snapshot, copies the current repo in,
builds it, runs the command under `pkgundo run`, inspects it, dry-run
rolls back, then really rolls back, and checks the package is actually gone.
Reverts the snapshot again at the end so the VM is ready for the next run.

## Testing the harder rollback paths

The default htop install only exercises "create file -> remove on rollback".
To validate the paths nothing else covers, edit the command passed to
`smoke-test.sh` (or SSH in manually with `ssh -i
/var/lib/libvirt/images/pkgundo-test/id_ed25519 pkgundo@<ip>`) to try:

- A package that overwrites an *existing* config file, to exercise the
  archive-then-restore path.
- Installing something and then enabling a systemd service as part of the
  same monitored command, to exercise service rollback.
- Adding a system user, to exercise `--mode clean`/`--mode nuclear` user
  rollback.

## Tracked-app / exec-watch tests

```bash
./exec-watch-test.sh            # main regression suite: exec detection, live mutation
                                 # capture, overlapping launches, restart re-arming,
                                 # unresolvable-uid fallback, untrack --rollback, and a
                                 # daemonizing app (fork, parent exits almost immediately,
                                 # child does the real work)
./exec-watch-multifs-test.sh    # separate, heavier: creates a throwaway user + a
                                 # loop-mounted filesystem to prove per-filesystem mark
                                 # scoping (st_dev) actually works across two distinct
                                 # filesystems, not just same-partition /home. More
                                 # invasive than the main suite, so kept separate rather
                                 # than run on every pass.
```

## Cleaning up

```bash
virsh destroy pkgundo-test
virsh undefine pkgundo-test --remove-all-storage
sudo rm -rf /var/lib/libvirt/images/pkgundo-test
```

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

## Uninstall-aware cleanup: pacman hook + review UI

```bash
sudo pkgundo install-hook            # once, requires root — installs the
                                      # pacman removal hook
sudo pkgundo install-hook --remove   # uninstalls it again
```

After this, removing a tracked app through pacman (`pacman -R weechat`,
`pacman -Rs ...`, an AUR helper, etc.) prints a reminder right in that same
terminal, naming the app and how many mutations were recorded, e.g.:

```
→ pkgundo was tracking removed package 'weechat' (23 mutation(s) recorded under $HOME).
  Review and roll back: pkgundo untrack weechat --rollback
  Preview first:         pkgundo untrack weechat --rollback --dry-run
```

The hook only ever detects and prints — it never touches `$HOME` itself.
Running `pkgundo untrack <app> --rollback` (without `--dry-run`) now walks
you through the recorded mutations in groups (home-relative path, capped at
2 components — e.g. `~/.config/weechat` and `~/.local/share/weechat/logs`
end up as two separate groups) instead of archiving/removing everything
unconditionally. Each group shows a suggested default (cache/log/state/tmp
directories default to remove; everything else defaults to keep) and takes:

- **Enter** — accept the suggested default
- **r** — remove this group
- **k** — keep this group
- **a** — remove this and every remaining group, no further prompts
- **s** — keep this and every remaining group, no further prompts
- **l** — list every file in this group, then re-ask about it

`--dry-run` is untouched by any of this — it stays a full, non-interactive
preview, exactly as before.

`pkgundo install-hook` is a manual, one-time step for now — there's no
packaging for pkgundo itself yet (no PKGBUILD), so nothing can trigger this
automatically on install. Once packaging exists, the hook file belongs in
`package()`'s output straight into `/usr/share/libalpm/hooks/`, which
pacman auto-scans — making this genuinely automatic and the CLI command
optional.

## Cleaning up

```bash
virsh destroy pkgundo-test
virsh undefine pkgundo-test --remove-all-storage
sudo rm -rf /var/lib/libvirt/images/pkgundo-test
```

# pkgundo

A universal Linux transaction monitor and intelligent rollback system: pkgundo watches what a package or binary actually does to your `$HOME` across its whole life, then lets you review and reverse it — archived, never blindly deleted.

## Uninstall-aware cleanup

`pkgundo track <name>` starts watching an app (a package name or a binary path) for as long as it's tracked, recording every file it creates/modifies/deletes under `$HOME`. When you're done with it:

```
pkgundo untrack <name> --rollback
```

reviews and reverses those accumulated mutations — archiving before removing, never a direct delete.

### Auto-detect on install/removal

Running `pkgundo track` manually every time is easy to forget. `pkgundo install-hook` (requires root) wires pkgundo into your package manager so it notices on its own:

```
sudo pkgundo install-hook
```

This detects which package manager(s) are present (pacman, apt/dpkg, and/or dnf5) and installs the matching hook(s):

- **On explicit install** (`pacman -S <pkg>` / `apt install <pkg>` / `dnf install <pkg>`): pkgundo starts tracking the package automatically — packages pulled in only as a dependency are left alone.
- **On removal** (`pacman -R <pkg>` / `apt remove <pkg>` / `dnf remove <pkg>`): if the removed package was being tracked, pkgundo prints a reminder in the same terminal, naming the review commands to run:
  ```
  → pkgundo was tracking removed package 'weechat' (23 mutation(s) recorded under $HOME).
    Review and roll back: pkgundo untrack weechat --rollback
    Preview first:         pkgundo untrack weechat --rollback --dry-run
  ```

The hook only ever detects and reminds — it never touches your files on its own. Run `sudo pkgundo install-hook --remove` to undo it.

Supported today: **pacman** (Arch/derivatives), **apt/dpkg** (Debian/Ubuntu/derivatives), and **dnf5** (Fedora 41+). dnf5's hook mechanism needs a separate, optional plugin package that isn't installed by default — if `install-hook` is run on a dnf5 system without it, it'll tell you to run `sudo dnf install libdnf5-plugin-actions` first. Note that dnf5 hands pkgundo one package name per transaction event rather than a batched list, so removing several tracked packages in one `dnf remove` prints one reminder block per package instead of a single combined summary (unlike pacman/apt). dnf4/RHEL/CentOS (an older, incompatible dnf generation) isn't supported yet.

### Reviewing what gets removed

`untrack --rollback` groups the recorded mutations (e.g. `~/.config/weechat`, `~/.local/share/weechat/logs`) and asks per group rather than all-or-nothing:

```
/home/you/.cache/weechat [Cache] 12 files — suggested: remove (Enter=accept, r=remove, k=keep, a=remove all remaining, s=keep all remaining, l=list files)
```

- **Enter** — accept the suggested default
- **`r`** — remove this group
- **`k`** — keep this group
- **`a`** — remove this and every remaining group
- **`s`** — keep this and every remaining group
- **`l`** — list every path in the group, then re-prompt

Groups tagged `Cache`/`Log`/`State`/`Tmp` default to remove; `Data` (config-looking) defaults to keep. Every removal still goes through the same archive-then-remove path as before, so a wrong call is exactly as recoverable via `pkgundo recover <txid>` as an unconditional rollback always was.

`untrack --rollback --dry-run` is unaffected by any of this — it stays a full, non-interactive preview.

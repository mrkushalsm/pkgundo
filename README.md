# pkgundo

A universal Linux transaction monitor and intelligent rollback system: pkgundo watches what a package or command actually does to your system — especially your `$HOME` — and lets you review and reverse it later. Nothing is ever silently deleted; everything removable is archived first.

Two ways to use it:
- **Automatically** — install the package-manager hooks once, and pkgundo starts tracking every app you explicitly install from then on, reminding you to review its footprint the moment you remove it.
- **Manually** — `pkgundo track <app>` / `pkgundo run <command>` to watch something on demand, on your own terms.

## Features

- **Uninstall-aware cleanup** — tracks a package's or binary's entire `$HOME` footprint (config, cache, logs, state) across its whole life, not just at install time.
- **Auto-detect on install/removal** — native hooks for pacman, apt/dpkg, and dnf5 auto-track new installs and remind you to review on removal, with zero manual steps.
- **Interactive, grouped review** — rollback groups mutations by directory and suggests keep/remove per group (config vs. cache vs. logs), instead of one all-or-nothing decision.
- **Archive, never delete** — every removed file is archived first via `pkgundo recover <txid>`, so a wrong call is always recoverable.
- **Whole-command monitoring** — `pkgundo run <command>` tracks a single command's system-wide mutations (not just `$HOME`), for one-off installs/scripts you want a rollback safety net around.
- **Works on real filesystems** — including btrfs (Fedora's default), via a transparent workaround for a kernel fanotify/btrfs limitation (see [How it works](#how-it-works)).

## Installation

### Quick install (recommended)

```sh
curl -fsSL https://raw.githubusercontent.com/mrkushalsm/pkgundo/main/install.sh | sh
```

This builds pkgundo from source (no prebuilt binaries yet — it'll install a Rust toolchain via `rustup` first if you don't have one), installs the binary to `/usr/local/bin`, and runs `pkgundo setup` (below) to wire up the daemon and package-manager hooks. Requires `git`, `curl`, and `sudo`.

### Manual build

```sh
git clone https://github.com/mrkushalsm/pkgundo.git
cd pkgundo
cargo build --release
sudo install -m 755 target/release/pkgundo /usr/local/bin/pkgundo
sudo pkgundo setup
```

### Native packages (Arch, Debian, Fedora)

Native packaging lives in [`packaging/`](packaging/) — an Arch `PKGBUILD`, and `cargo-deb`/`cargo-generate-rpm` metadata for `.deb`/`.rpm`. Each installs/upgrades/removes via the same `pkgundo setup`/`setup --remove` the other install paths use, so the daemon and package-manager hooks are wired up automatically as part of a normal `pacman -U`/`dpkg -i`/`dnf install` — no separate `pkgundo setup` step needed. GitHub Actions (`.github/workflows/release.yml`) builds a prebuilt binary tarball, `.deb`, and `.rpm`, and attaches a ready-to-use `PKGBUILD`, to every tagged release.

### `pkgundo setup`

Whichever way you install the binary, `sudo pkgundo setup` is the one command that wires everything up: installs and starts the daemon's systemd unit (enabled on boot), then installs the package-manager hooks for whichever of pacman/apt/dnf5 it detects. It's idempotent — safe to re-run.

To undo everything:

```sh
sudo pkgundo setup --remove && sudo rm /usr/local/bin/pkgundo
```

## How it works

A background daemon (`pkgundo-daemon`, installed by `setup`) watches for the launch of any tracked app's binary and captures its filesystem writes under `$HOME` via `fanotify`, attributing each mutation to the app's own tracking record as it happens — not by diffing snapshots afterward. Removed files are archived (not deleted) before rollback acts on them, so any decision is reversible.

**btrfs note:** the kernel's `fanotify_mark(FAN_MARK_FILESYSTEM | FAN_REPORT_FID)` call this relies on fails with `EXDEV` on any btrfs subvolume that isn't subvolume id 5 — a documented, unresolved kernel/btrfs limitation (Fedora, Debian's `@`-subvolume convention, and any manually-partitioned btrfs layout with a separate `/home` subvolume all hit this). pkgundo works around it transparently: the daemon mounts the owning device's subvolume id 5 read-only under `/run/pkgundo/btrfs-root/` and marks that instead, which — since fanotify's filesystem-scope mark attaches to the shared underlying superblock — still correctly captures events from every subvolume on that device. No configuration needed; the proxy mount is created lazily on first use and torn down on daemon shutdown.

## Usage

### Automatic (recommended)

Once `pkgundo setup` (or `install-hook`, see below) has run once, no further manual steps are needed:

- **On explicit install** (`pacman -S <pkg>` / `apt install <pkg>` / `dnf install <pkg>`): pkgundo starts tracking the package automatically — packages pulled in only as a dependency are left alone.
- **On removal** (`pacman -R <pkg>` / `apt remove <pkg>` / `dnf remove <pkg>`): if the removed package was being tracked, pkgundo prints a reminder in the same terminal, naming the review commands to run:
  ```
  → pkgundo was tracking removed package 'weechat' (23 mutation(s) recorded under $HOME).
    Review and roll back: pkgundo untrack weechat --rollback
    Preview first:         pkgundo untrack weechat --rollback --dry-run
  ```

The hook only ever detects and reminds — it never touches your files on its own.

If you only want the hooks (without the one-shot `setup` also managing the daemon unit for you), install them directly:

```sh
sudo pkgundo install-hook            # detects pacman/apt/dnf5 and installs the matching hook(s)
sudo pkgundo install-hook --remove   # undo
```

Supported today: **pacman** (Arch/derivatives), **apt/dpkg** (Debian/Ubuntu/derivatives), and **dnf5** (Fedora 41+). dnf5's hook mechanism needs a separate, optional plugin package that isn't installed by default — if `install-hook`/`setup` is run on a dnf5 system without it, it'll tell you to run `sudo dnf install libdnf5-plugin-actions` first, then re-run. Note that dnf5 hands pkgundo one package name per transaction event rather than a batched list, so removing several tracked packages in one `dnf remove` prints one reminder block per package instead of a single combined summary (unlike pacman/apt). dnf4/RHEL/CentOS (an older, incompatible dnf generation) isn't supported yet.

### Manual tracking

Prefer to choose what gets watched yourself, rather than everything you install?

```sh
pkgundo track firefox              # starts watching (package name or a binary path/name)
pkgundo tracked                     # list what's currently tracked
pkgundo untrack firefox --rollback  # review + archive-then-remove its accumulated $HOME mutations
pkgundo untrack firefox --rollback --dry-run   # preview only, no changes
```

`pkgundo untrack <app>` (no `--rollback`) just stops watching — it leaves everything on disk untouched.

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

### Whole-command monitoring

For a one-off command or install you want a rollback safety net around (not the app-lifecycle tracking above), wrap it directly:

```sh
sudo pkgundo run pacman -S steam   # runs the command, recording every system mutation it causes
pkgundo timeline                   # list all recorded transactions
pkgundo status                     # recent transactions, active monitors, etc.
pkgundo inspect <txid>             # mutations/files/services recorded for one transaction
sudo pkgundo rollback <txid>       # revert it (--dry-run to preview, --mode conservative|clean|nuclear)
pkgundo recover <txid>             # restore files archived by a previous rollback
```

Rollback modes: `conservative` (default — archive aggressively, minimal risk), `clean` (deeper cleanup of runtime leftovers), `nuclear` (aggressive removal — advanced users only).

### Other commands

```sh
pkgundo simulate <command>            # dry-run capability report for a command, no changes made
pkgundo scan-leftovers <app> --dry-run  # heuristic scan for an app's leftover files under $HOME (pacman, apt/dpkg, and dnf5/rpm)
```

## Supported platforms

Linux only (relies on `fanotify`). Package-manager hooks support pacman, apt/dpkg, and dnf5 (Fedora 41+) — see [Automatic](#automatic-recommended) above for exact coverage and caveats.

## Known limitations

- **dnf4 / RHEL / CentOS / older Fedora** isn't supported yet — dnf5's hook mechanism is a different, incompatible plugin from dnf4's.
- **`scan-leftovers`'s already-uninstalled fallback** (matching against a cached package archive rather than a currently-installed one) is best-effort on rpm/dnf5 systems — `dnf`'s `keepcache` setting defaults to off on many installs, so a downloaded `.rpm` is often not still around post-install to match against. Works reliably on pacman and apt/dpkg, which both keep their package cache by default.
- A tracked app registered by binary path (not a package) has no removal signal — nothing hooks into "the user deleted this binary by hand."

## License

[MIT](LICENSE)

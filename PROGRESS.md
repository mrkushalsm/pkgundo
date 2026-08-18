# pkgundo — Build Progress Tracker

> **Purpose:** Continuation context for AI agents resuming this work.
> Last updated: 2026-05-16 (all phases complete)

---

## 🎉 ALL PHASES COMPLETE

```
cargo build --release  → ✅ SUCCESS (warnings only, zero errors)
cargo test             → ✅ 26/26 tests pass
Binary:                → ✅ target/release/pkgundo (4.5MB)
```

---

## Development Phases

| Phase | Description | Status |
|-------|-------------|--------|
| 1 | Transaction wrapper, command execution, metadata storage | ✅ Done |
| 2 | Process tree attribution (PID → TXID mapping via /proc) | ✅ Done |
| 3 | Filesystem mutation monitoring (inotify via notify crate) | ✅ Done |
| 4 | Hashing and fingerprinting (SHA256 pre/post snapshots) | ✅ Done |
| 5 | Semantic file classification (12 categories) | ✅ Done |
| 6 | Rollback engine (Conservative/Clean/Nuclear, Steps A→G) | ✅ Done |
| 7 | Archive manager (preserve + recover) | ✅ Done |
| 8 | Explainability engine (inspect/timeline/status) | ✅ Done |
| 9 | Advanced reconciliation (service/user/blob tracking) | ✅ Done |
| 10 | eBPF/fanotify integration (PID attribution + capability detection) | ✅ Done |

---

## Complete File Structure

```
pkgundo/
├── Cargo.toml                      ✅ lib+bin targets, all deps
├── src/
│   ├── main.rs                     ✅ Phase 9+10 wired: blob pre-scan, user snapshot, fanotify
│   ├── lib.rs                      ✅ All 16 modules exposed
│   ├── cli/mod.rs                  ✅ clap Commands enum (7 subcommands)
│   ├── transaction/mod.rs          ✅ Transaction, PackageManager, create/load/update
│   ├── process_tracker/mod.rs      ✅ /proc polling, watch_process_tree task
│   ├── fs_monitor/mod.rs           ✅ inotify via notify crate
│   ├── journal/mod.rs              ✅ Append-only mutation journal with UNIQUE dedup
│   ├── fingerprint/mod.rs          ✅ SHA256 + metadata, compare_with_current
│   ├── classifier/mod.rs           ✅ 12-category semantic classifier
│   ├── rollback/mod.rs             ✅ Steps A→G incl. Step F (Phase 9)
│   ├── archive/mod.rs              ✅ archive_file, recover_archive, JSON metadata
│   ├── inspect/mod.rs              ✅ inspect_transaction, timeline, status
│   ├── db/mod.rs                   ✅ 9-table SQLite schema (WAL)
│   ├── service_tracker/mod.rs      ✅ Phase 9: systemctl detection + rollback
│   ├── blob_store/mod.rs           ✅ Phase 9: file content pre-snapshots + restore
│   ├── user_tracker/mod.rs         ✅ Phase 9: passwd/group diff + rollback
│   └── ebpf/mod.rs                 ✅ Phase 10: fanotify monitor + eBPF infrastructure
├── tests/
│   └── integration_test.rs         ✅ 26 tests (all pass)
└── target/release/pkgundo          ✅ 4.5MB binary
```

---

## Phase 9: Advanced Reconciliation

### `src/service_tracker/mod.rs`
- `parse_cmdline_for_systemctl()` — parses null-delimited /proc/pid/cmdline
- `ServiceAction` enum (Enable/Disable/Start/Stop/Restart/Reload/DaemonReload)
  - `inverse()` — returns the rollback action (Enable→Disable, Start→Stop)
- `ServiceEvent` struct with txid, service_name, action, pre_state, timestamp
- `get_service_state()` — calls systemctl is-enabled + is-active
- `record_service_event()` — INSERT OR IGNORE into service_events table
- `get_service_events()` — load events for rollback
- `detect_service_changes_from_pids()` — scans process_tree cmdlines post-tx
- `rollback_service_events()` — reverses service actions in reverse order; runs daemon-reload
- `detect_installed_units()` — finds .service/.timer/.socket in mutation paths

### `src/blob_store/mod.rs`
- `MAX_BLOB_SIZE = 1MB` — files larger than this are skipped
- `FileBlob` struct with content BLOB, sha256, uid/gid/mode, captured_at
- `store_file_blob()` — INSERT OR IGNORE into file_blobs table
- `get_blob_content()` — retrieve content bytes for a path+phase
- `restore_from_blob()` — writes file back to original path + restores permissions + chown
- `pre_scan_configs()` — pre-scans /etc and /usr/lib/systemd/system at tx start (Phase 9 key feature)
- `list_blobs()` — metadata-only listing (no content) for inspection

### `src/user_tracker/mod.rs`
- `UserGroupSnapshot` — captures /etc/passwd + /etc/group lines
  - `capture()` — reads from filesystem
  - `to_json()` / `from_json()` — serialize to DB
- `diff_snapshots()` — line-by-line diff of before/after snapshots
  - Detects: UserAdded, UserRemoved, GroupAdded, GroupRemoved
- `store_snapshot()` / `load_snapshot()` — persist in user_snapshots table
- `record_user_events()` — INSERT OR IGNORE into user_events
- `rollback_user_events()` — calls userdel/groupdel for added users/groups
  - Only in Clean/Nuclear mode; Conservative warns but doesn't act

### `src/db/mod.rs` — New Tables (Phase 9)
- `service_events` (txid, service_name, action, pre_state, timestamp; UNIQUE key)
- `file_blobs` (txid, path, phase, content BLOB, sha256, size, uid/gid/mode; UNIQUE key)
- `user_events` (txid, kind, name, pre_state, timestamp; UNIQUE key)
- `user_snapshots` (txid, phase, snapshot_json, captured_at; UNIQUE key)

### `src/rollback/mod.rs` — Step F Added
- Step F: Service & user reconciliation (new, between Step E and Step G)
  - Calls `service_tracker::rollback_service_events()`
  - Clean/Nuclear: calls `user_tracker::rollback_user_events()`
  - Conservative: warns about user changes, doesn't act
- `handle_modified_file()` now tries `blob_store::restore_from_blob()` before falling back
- `handle_deleted_file()` now tries `blob_store::restore_from_blob()` before warning
- `RollbackReport` gains `service_reversals` and `user_reversals` Vec fields

---

## Phase 10: eBPF / fanotify Integration

### `src/ebpf/mod.rs`
- `KernelCapabilities::detect()` — checks fanotify, eBPF, tracefs availability
  - `fanotify_available` — via fanotify_init() syscall probe
  - `ebpf_available` — checks /sys/fs/bpf + perf_event_paranoid
  - `tracefs_mounted` — checks /sys/kernel/tracing
  - `kernel_version` — from /proc/version
- `FanotifyMonitor` — fanotify-based fs monitor with PID attribution per event
  - Uses raw fanotify_init/fanotify_mark/read syscalls via libc
  - Watches /usr, /etc, /var, /opt, /lib, /lib64, /bin, /sbin
  - Each event includes the originating PID (unlike inotify)
  - Async `run()` method forwards `MutationRecord` with `pid: Some(pid)`
- `EbpfTracer` — eBPF infrastructure + reporting
  - `print_report()` — shows kernel capabilities table
  - Includes full eBPF C program source (embedded as comments)
  - Documents aya Rust loader API for future compilation
- `start_enhanced_monitor()` — tries fanotify first, falls back to inotify
  - Returns `true` if fanotify was started (PID attribution active)

### `src/main.rs` — Phase 10 Wiring
- Calls `KernelCapabilities::detect()` at startup
- Calls `start_enhanced_monitor()` — fanotify if available, inotify fallback
- `handle_simulate()` now shows full Phase 10 capability report

---

## Test Coverage: 26/26

| Test | Phase | Status |
|------|-------|--------|
| test_create_and_load_transaction | 1 | ✅ |
| test_transaction_status_update | 1 | ✅ |
| test_package_manager_detection | 1 | ✅ |
| test_journal_append_and_retrieve | 3 | ✅ |
| test_journal_deduplication | 3 | ✅ |
| test_classifier_user_data_never_touched | 5 | ✅ |
| test_classifier_config_archived | 5 | ✅ |
| test_classifier_cache_safe_delete | 5 | ✅ |
| test_classifier_binaries | 5 | ✅ |
| test_classifier_libraries | 5 | ✅ |
| test_sha256_hash | 4 | ✅ |
| test_fingerprint_comparison_unchanged | 4 | ✅ |
| test_mutation_summary | 3 | ✅ |
| test_parse_systemctl_enable | 9 | ✅ |
| test_parse_systemctl_disable_multiple | 9 | ✅ |
| test_parse_non_systemctl_returns_none | 9 | ✅ |
| test_service_action_inverse | 9 | ✅ |
| test_service_action_roundtrip | 9 | ✅ |
| test_service_event_record_and_load | 9 | ✅ |
| test_detect_installed_units | 9 | ✅ |
| test_blob_store_round_trip | 9 | ✅ |
| test_blob_store_skips_large_files | 9 | ✅ |
| test_user_snapshot_diff_detects_new_user | 9 | ✅ |
| test_user_snapshot_diff_detects_removed_user | 9 | ✅ |
| test_ebpf_tracer_creates_without_panic | 10 | ✅ |
| test_kernel_caps_detect_runs | 10 | ✅ |

---

## How to Build & Run

```bash
cd /home/mrkus/Work/personal/projects/pkgundo
source ~/.cargo/env
cargo build --release
cargo test

# Run commands (require root)
sudo ./target/release/pkgundo run pacman -S htop
sudo ./target/release/pkgundo inspect 1
sudo ./target/release/pkgundo timeline
sudo ./target/release/pkgundo rollback 1
sudo ./target/release/pkgundo rollback 1 --mode clean   # also reverses users/groups
sudo ./target/release/pkgundo rollback 1 --mode nuclear
sudo ./target/release/pkgundo rollback 1 --dry-run
sudo ./target/release/pkgundo recover 1

# No root needed
./target/release/pkgundo simulate pacman -S steam  # shows Phase 10 capabilities
./target/release/pkgundo status
./target/release/pkgundo timeline
```

---

## Architecture Decision Notes

- **fanotify over inotify (Phase 10):** fanotify provides the PID of every file mutator.
  pkgundo tries fanotify at startup; if unavailable (permissions/kernel), falls back to inotify.
  fanotify gives true PID attribution — the core Phase 10 improvement.
- **eBPF infrastructure:** Full eBPF C program source embedded as comments in ebpf/mod.rs.
  Requires `clang -target bpf` to compile; loadable via `aya`. Infrastructure is in place.
- **Blob store design:** Pre-scan /etc (up to 1MB per file) before command launches.
  This is the only way to get true "restore from before install" capability without eBPF.
  Files larger than 1MB are skipped (fingerprint-only tracking).
- **Service rollback is reversible:** Only Enable→Disable and Start→Stop are reversed.
  Restart/Reload/DaemonReload have no deterministic inverse and are skipped.
- **User rollback is mode-gated:** Conservative mode warns; Clean/Nuclear mode runs userdel.
  This is intentional — removing system users is dangerous and should require explicit opt-in.
- **Safety invariants:** /home → NeverTouch (enforced by classifier). Package-owned files → delegated
  to native PM (enforced by PackageDb category → Skip action). These are not overridable.

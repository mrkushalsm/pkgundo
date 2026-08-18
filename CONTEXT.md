# PROJECT SPECIFICATION

# pkgundo

## Universal Linux Transaction Monitoring, Mutation Provenance, and Intelligent Rollback System

==================================================

1. PROJECT OVERVIEW
   ==================================================

Build a production-grade Linux systems software project called `pkgundo`.

pkgundo is NOT:

* a package manager replacement
* a distro-specific feature
* a filesystem snapshot utility
* a wrapper around pacman/apt/dnf
* a backup application

pkgundo IS:

* a universal Linux transaction monitor
* a mutation provenance engine
* an intelligent rollback reconciliation system
* an archival recovery system
* a system state observability platform
* a transaction-aware rollback orchestrator

Core philosophy:

“Track, understand, explain, archive, and safely reverse Linux system mutations caused by package installations, uninstallations, scripts, installers, and arbitrary system-modifying commands.”

The project must work universally across Linux distributions without relying on:

* Btrfs
* ZFS
* overlayfs snapshots
* distro-specific hooks
* package-manager-specific internal APIs
* filesystem-specific snapshot capabilities

The project should rely primarily on:

* Linux kernel interfaces
* process tree tracking
* `/proc`
* fanotify/inotify
* optionally eBPF in advanced stages

The abstraction layer is:
THE LINUX KERNEL ITSELF.

==================================================
2. CORE PROBLEM THIS PROJECT SOLVES
===================================

Current package managers:

* pacman
* apt
* dnf
* rpm
* dpkg

already manage:

* package ownership
* package databases
* dependency graphs
* installation metadata

Examples:

* `pacman -Rcns`
* `apt purge && apt autoremove`
* `dnf remove && dnf autoremove`

These commands:

* remove package-owned files
* remove orphaned dependencies
* update package databases

BUT they do NOT fully:

* track side effects
* restore overwritten files
* preserve user-modified configs intelligently
* explain mutations
* reverse arbitrary runtime modifications
* rollback indirect system changes
* provide causal mutation provenance

pkgundo exists to solve:
“everything beyond package ownership.”

==================================================
3. DESIGN PHILOSOPHY
====================

Package managers remain:
SOURCE OF TRUTH for package ownership.

pkgundo becomes:
SOURCE OF TRUTH for mutation provenance and rollback reconciliation.

Separation of responsibility:

| Responsibility               | Owner                  |
| ---------------------------- | ---------------------- |
| install/remove package       | native package manager |
| dependency graph             | native package manager |
| package database consistency | native package manager |
| mutation tracking            | pkgundo                |
| side effect tracking         | pkgundo                |
| rollback reconciliation      | pkgundo                |
| archival preservation        | pkgundo                |
| explainability               | pkgundo                |

pkgundo must NEVER:

* corrupt package manager databases
* manually remove official package files without informing package manager
* reinvent package management
* assume filesystem snapshot support

==================================================
4. PRIMARY EXECUTION MODEL
==========================

All monitored commands are executed through:

```bash
sudo pkgundo run <command>
```

Examples:

```bash
sudo pkgundo run pacman -S steam
sudo pkgundo run apt install nginx
sudo pkgundo run dnf install vlc
sudo pkgundo run ./installer.sh
sudo pkgundo run make install
sudo pkgundo run pip install tensorflow
```

pkgundo acts as:

* supervisor
* monitor
* transaction orchestrator

NOT package installer.

==================================================
5. HIGH LEVEL ARCHITECTURE
==========================

Architecture:

```text
                 +-------------------+
                 |   User Command    |
                 | pkgundo run ...   |
                 +---------+---------+
                           |
                           v
               +-----------+-----------+
               | Transaction Manager   |
               +-----------+-----------+
                           |
         +----------------+----------------+
         |                                 |
         v                                 v
+------------------+         +-------------------------+
| Process Tracker  |         | Filesystem Monitor      |
| PID Attribution  |         | fanotify/inotify/eBPF   |
+------------------+         +-------------------------+
         |                                 |
         +---------------+-----------------+
                         |
                         v
             +-----------------------+
             | Mutation Journal      |
             | hashes, metadata,     |
             | ownership, diffs      |
             +-----------+-----------+
                         |
          +--------------+--------------+
          |                             |
          v                             v
+-------------------+       +----------------------+
| Rollback Engine   |       | Archive Manager      |
| reconciliation    |       | preserved user data  |
+-------------------+       +----------------------+
```

==================================================
6. TRANSACTION SYSTEM
=====================

Every monitored operation becomes:
A TRANSACTION.

Each transaction receives:

* transaction ID
* timestamp
* process tree
* mutation journal
* rollback metadata

Example:

```json
{
  "txid": 42,
  "command": "pacman -S steam",
  "package_manager": "pacman",
  "start_time": "...",
  "status": "running"
}
```

Transaction storage:

```text
/var/lib/pkgundo/
├── transactions/
├── journals/
├── archives/
├── metadata/
├── diffs/
└── logs/
```

==================================================
7. PROCESS ATTRIBUTION ENGINE
=============================

pkgundo must track:

* parent processes
* child processes
* detached subprocesses
* post-install scripts
* service invocations
* cache rebuilders

Examples:

```text
pacman
 ├── bash post-install.sh
 ├── ldconfig
 ├── systemctl
 ├── gtk-update-icon-cache
 └── update-desktop-database
```

Every mutation must map:

```text
PID -> TRANSACTION_ID
```

Implementation:

* `/proc`
* process groups
* PPID traversal
* PID lineage tracking

Potential advanced implementation:

* eBPF syscall attribution

==================================================
8. FILESYSTEM MUTATION JOURNAL
==============================

Track:

* create
* modify
* delete
* rename
* chmod
* chown
* symlink creation/removal

Use:

* fanotify
* inotify
* optionally eBPF

Mutation example:

```json
{
  "txid": 42,
  "pid": 4421,
  "operation": "modify",
  "path": "/etc/ld.so.cache",
  "timestamp": "..."
}
```

The mutation journal must be:

* append-only
* timestamped
* transaction-aware

==================================================
9. FILE FINGERPRINTING SYSTEM
=============================

Before modification:
store:

* SHA256 hash
* ownership
* permissions
* timestamps
* metadata

During rollback:
compare:

* pre-install hash
* post-install hash
* current hash

Cases:

| State                   | Action        |
| ----------------------- | ------------- |
| unchanged since install | remove safely |
| modified later by user  | archive       |
| missing already         | ignore        |
| ambiguous               | ask or skip   |

==================================================
10. SEMANTIC FILE CLASSIFICATION
================================

Every tracked file classified into categories:

* binaries
* configs
* caches
* runtime state
* temp files
* logs
* symlinks
* user data

Rules:

NEVER TOUCH:

```text
/home
```

SAFE TO DELETE:

```text
/var/cache
/tmp
/run
```

ARCHIVE CAREFULLY:

```text
/etc
/var/lib
```

This classification layer powers:

* intelligent rollback
* safe archival
* selective cleanup

==================================================
11. ARCHIVE MANAGER
===================

If user modified files after installation:
DO NOT:

* leave active
* delete permanently

Instead:
ARCHIVE externally.

Example:

```text
/var/lib/pkgundo/archive/steam/tx42/
```

Store:

* original path
* timestamps
* hashes
* optional diffs
* ownership metadata

Example metadata:

```json
{
  "original_path": "/etc/nginx/nginx.conf",
  "txid": 51,
  "modified_after_install": true
}
```

Recovery command:

```bash
pkgundo recover 51
```

Archive philosophy:

* clean rollback
* zero permanent data loss
* avoid stale config pollution

==================================================
12. PACKAGE MANAGER COOPERATION MODEL
=====================================

IMPORTANT:

pkgundo NEVER manually removes package-owned files first.

Instead:
ROLLBACK MUST COOPERATE WITH PACKAGE MANAGERS.

==================================================
13. ROLLBACK EXECUTION FLOW
===========================

Command:

```bash
pkgundo rollback <txid>
```

Example:

```bash
pkgundo rollback 42
```

==================================================
STEP A: LOAD TRANSACTION
========================

Load:

* mutation journal
* hashes
* process tree
* metadata
* archives

==================================================
STEP B: DETERMINE PACKAGE MANAGER
=================================

Example:

```json
{
  "manager": "pacman"
}
```

==================================================
STEP C: DELEGATE OFFICIAL REMOVAL
=================================

Arch:

```bash
pacman -Rcns <package>
```

Debian:

```bash
apt purge <package>
apt autoremove
```

Fedora:

```bash
dnf remove <package>
dnf autoremove
```

This ensures:

* package DB consistency
* dependency correctness
* ownership cleanup

==================================================
STEP D: RECONCILIATION PHASE
============================

pkgundo now:

* restores overwritten files
* archives modified configs
* removes generated caches
* disables services
* removes orphaned side effects
* restores previous state

This is pkgundo’s MAIN VALUE.

==================================================
STEP E: FILE ANALYSIS
=====================

For every mutation:

CASE 1: Created File

* unchanged → remove
* modified → archive
* owned elsewhere → skip

CASE 2: Modified File

* unchanged since install → restore original
* modified later → archive modified version, restore original

CASE 3: Deleted File

* restore from stored backup

CASE 4: Generated Cache

* remove safely

==================================================
STEP F: SERVICE RECONCILIATION
==============================

Track:

* enabled services
* cron jobs
* users/groups
* symlinks
* daemon reloads

Rollback:

* disable services
* restore previous state

==================================================
STEP G: FINAL INTEGRITY CHECK
=============================

Verify:

* broken symlinks
* missing restores
* hash mismatches
* failed rollbacks

Generate final report.

==================================================
14. EXPLAINABILITY ENGINE
=========================

Commands:

```bash
pkgundo inspect 42
pkgundo timeline
pkgundo status
```

Example output:

```text
Installed:
- 1842 files

Modified:
- icon cache
- MIME database

Enabled:
- steam-helper.service

Archived:
- modified configs
```

Explainability is a PRIMARY FEATURE.

==================================================
15. TRANSACTION TYPES
=====================

TYPE 1:
PACKAGE TRANSACTION

Examples:

```bash
pkgundo run pacman -S steam
pkgundo run apt install nginx
```

Rollback:

* native uninstall first
* reconciliation second

==================================================

TYPE 2:
SCRIPT TRANSACTION

Examples:

```bash
pkgundo run ./installer.sh
pkgundo run make install
```

No package manager exists.

Rollback:

* fully manual mutation reversal

This is where pkgundo becomes extremely powerful.

==================================================
16. SAFETY MODEL
================

Rollback must prioritize:
SAFETY OVER COMPLETENESS.

Rules:

* never blindly delete
* never overwrite ambiguous changes
* archive uncertain files
* never touch unrelated user data
* never assume ownership blindly

If uncertain:

* archive
* skip
* ask user

==================================================
17. ROLLBACK MODES
==================

Mode 1:
Conservative
(default)

* archive aggressively
* minimal risk
* preserve ambiguity

Mode 2:
Clean

* deeper cleanup
* removes more runtime leftovers

Mode 3:
Nuclear

* aggressive removal
* advanced users only
* strong warnings

==================================================
18. MASSIVE FULL EXAMPLE
========================

INSTALL:

```bash
sudo pkgundo run pacman -S steam
```

==================================================
TRANSACTION CREATED
===================

```json
{
  "txid": 42,
  "command": "pacman -S steam"
}
```

==================================================
PROCESS TREE TRACKED
====================

```text
pacman
 ├── post-install.sh
 ├── ldconfig
 ├── gtk-update-icon-cache
 ├── systemctl
 └── shader-cache-generator
```

==================================================
MUTATIONS TRACKED
=================

Installed:

```text
/usr/bin/steam
/usr/lib/steam/*
```

Modified:

```text
/etc/ld.so.cache
/usr/share/icons/hicolor/icon-theme.cache
```

Generated:

```text
/var/cache/fontconfig/*
```

Services:

```text
steam-helper.service
```

==================================================
USER MODIFIES CONFIGS
=====================

```text
/etc/steam/custom.conf
```

==================================================
ROLLBACK
========

```bash
pkgundo rollback 42
```

==================================================
ROLLBACK FLOW
=============

1. Run:

```bash
pacman -Rcns steam
```

2. Package DB updated correctly.

3. pkgundo reconciles mutations.

4. Modified config archived:

```text
/var/lib/pkgundo/archive/steam/tx42/custom.conf
```

5. Original overwritten files restored.

6. Generated caches removed.

7. Service disabled.

==================================================
FINAL RESULT
============

* package manager remains consistent
* user modifications preserved
* system cleaned safely
* no orphaned active configs
* no silent data loss

==================================================
19. ADVANCED FEATURES
=====================

Future features:

* eBPF syscall tracing
* TUI dashboard
* interactive rollback
* rollback dry-run
* dependency-aware rollback
* conflict detection
* transaction simulation
* mutation replay timeline
* AI-assisted semantic classification
* visual process tree explorer

==================================================
20. COMMANDS
============

Run transaction:

```bash
pkgundo run <command>
```

Rollback:

```bash
pkgundo rollback <txid>
```

Inspect:

```bash
pkgundo inspect <txid>
```

Timeline:

```bash
pkgundo timeline
```

Recover archives:

```bash
pkgundo recover <txid>
```

Simulation:

```bash
pkgundo simulate <command>
```

==================================================
21. SUGGESTED TECH STACK
========================

Language:
Rust

Reasons:

* systems programming
* memory safety
* concurrency
* daemon reliability
* async IO

Suggested crates:

* tokio
* nix
* notify
* serde
* rusqlite

Database:
SQLite

==================================================
22. PROJECT STRUCTURE
=====================

```text
pkgundo/
├── cmd/
├── daemon/
├── transaction/
├── process_tracker/
├── fs_monitor/
├── journal/
├── rollback/
├── archive/
├── classifier/
├── integrity/
├── inspect/
├── storage/
├── db/
├── cli/
├── tests/
└── docs/
```

==================================================
23. DEVELOPMENT PHASES
======================

PHASE 1

* transaction wrapper
* command execution
* metadata storage

PHASE 2

* process tree attribution

PHASE 3

* filesystem mutation monitoring

PHASE 4

* hashing and fingerprinting

PHASE 5

* semantic file classification

PHASE 6

* rollback engine

PHASE 7

* archive manager

PHASE 8

* explainability engine

PHASE 9

* advanced reconciliation

PHASE 10

* eBPF integration

==================================================
24. FINAL PROJECT GOAL
======================

The final project should feel like:

* Git for Linux system state
* transactional observability for Linux
* intelligent rollback infrastructure
* mutation provenance engine
* accountability layer for Linux system changes

The implementation must be:

* modular
* scalable
* event-driven
* distro-independent
* filesystem-independent
* production-grade
* cautious
* explainable
* safe
* deeply observable

The project should emphasize:
“understanding and reconciling system mutations”
rather than merely deleting files.

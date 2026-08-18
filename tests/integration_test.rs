/// Integration tests for pkgundo core modules.
/// These tests use in-memory SQLite and temp directories to avoid requiring root.

#[cfg(test)]
mod tests {
    use rusqlite::Connection;
    use std::path::Path;
    use std::fs;
    use tempfile::TempDir;

    fn setup_in_memory_db() -> Connection {
        let conn = Connection::open_in_memory().expect("failed to open in-memory db");
        conn.execute_batch("PRAGMA journal_mode=WAL;").ok();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS transactions (
                txid INTEGER PRIMARY KEY AUTOINCREMENT,
                command TEXT NOT NULL,
                package_manager TEXT NOT NULL DEFAULT 'unknown',
                start_time TEXT NOT NULL,
                end_time TEXT,
                status TEXT NOT NULL DEFAULT 'running',
                pid_root INTEGER,
                rollback_mode TEXT NOT NULL DEFAULT 'conservative',
                notes TEXT
            );
            CREATE TABLE IF NOT EXISTS mutations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                txid INTEGER NOT NULL,
                pid INTEGER,
                operation TEXT NOT NULL,
                path TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                file_category TEXT NOT NULL DEFAULT 'Unknown',
                pre_hash TEXT,
                post_hash TEXT,
                UNIQUE(txid, operation, path)
            );
            CREATE TABLE IF NOT EXISTS fingerprints (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                txid INTEGER NOT NULL,
                path TEXT NOT NULL,
                phase TEXT NOT NULL,
                sha256 TEXT,
                size INTEGER,
                uid INTEGER,
                gid INTEGER,
                mode INTEGER,
                mtime INTEGER,
                captured_at TEXT NOT NULL,
                is_symlink INTEGER NOT NULL DEFAULT 0,
                symlink_target TEXT,
                UNIQUE(txid, path, phase)
            );
            CREATE TABLE IF NOT EXISTS archives (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                txid INTEGER NOT NULL,
                original_path TEXT NOT NULL,
                archive_path TEXT NOT NULL,
                modified_after_install INTEGER NOT NULL DEFAULT 0,
                archived_at TEXT NOT NULL,
                UNIQUE(txid, original_path)
            );
            CREATE TABLE IF NOT EXISTS process_tree (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                txid INTEGER NOT NULL,
                pid INTEGER NOT NULL,
                ppid INTEGER,
                name TEXT,
                UNIQUE(txid, pid)
            );",
        ).expect("failed to create tables");
        conn
    }

    // ── Transaction tests ───────────────────────────────────────────────────

    #[test]
    fn test_create_and_load_transaction() {
        let conn = setup_in_memory_db();
        let args = vec!["pacman".to_string(), "-S".to_string(), "htop".to_string()];
        let txid = pkgundo::transaction::create_transaction(&conn, "pacman -S htop", &args)
            .expect("create transaction failed");

        assert!(txid > 0, "txid should be positive");

        let tx = pkgundo::transaction::load_transaction(&conn, txid)
            .expect("load transaction failed");

        assert_eq!(tx.command, "pacman -S htop");
        assert_eq!(tx.package_manager, pkgundo::transaction::PackageManager::Pacman);
        assert_eq!(tx.status, pkgundo::transaction::TransactionStatus::Running);
    }

    #[test]
    fn test_transaction_status_update() {
        let conn = setup_in_memory_db();
        let args = vec!["apt".to_string(), "install".to_string(), "nginx".to_string()];
        let txid = pkgundo::transaction::create_transaction(&conn, "apt install nginx", &args)
            .expect("create transaction failed");

        pkgundo::transaction::update_transaction_status(
            &conn,
            txid,
            pkgundo::transaction::TransactionStatus::Completed,
            Some(12345),
        ).expect("update status failed");

        let tx = pkgundo::transaction::load_transaction(&conn, txid).unwrap();
        assert_eq!(tx.status, pkgundo::transaction::TransactionStatus::Completed);
        assert_eq!(tx.pid_root, Some(12345));
    }

    #[test]
    fn test_package_manager_detection() {
        use pkgundo::transaction::{PackageManager};

        let cases = vec![
            (vec!["pacman", "-S", "steam"], PackageManager::Pacman),
            (vec!["apt", "install", "htop"], PackageManager::Apt),
            (vec!["apt-get", "install", "curl"], PackageManager::Apt),
            (vec!["dnf", "install", "vlc"], PackageManager::Dnf),
            (vec!["./installer.sh"], PackageManager::Script),
            (vec!["make", "install"], PackageManager::Script),
            (vec!["pip3", "install", "flask"], PackageManager::Pip),
        ];

        for (args, expected) in cases {
            let string_args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            let detected = PackageManager::detect_from_command(&string_args);
            assert_eq!(detected, expected, "Failed for args: {:?}", args);
        }
    }

    // ── Journal tests ───────────────────────────────────────────────────────

    #[test]
    fn test_journal_append_and_retrieve() {
        let conn = setup_in_memory_db();

        let record = pkgundo::journal::MutationRecord {
            id: None,
            txid: 1,
            pid: Some(1234),
            operation: "create".to_string(),
            path: "/usr/bin/htop".to_string(),
            timestamp: chrono::Utc::now(),
            file_category: "Binary".to_string(),
            pre_hash: None,
            post_hash: Some("abc123".to_string()),
        };

        pkgundo::journal::append_mutation(&conn, &record)
            .expect("append mutation failed");

        let mutations = pkgundo::journal::get_mutations(&conn, 1)
            .expect("get mutations failed");

        assert_eq!(mutations.len(), 1);
        assert_eq!(mutations[0].path, "/usr/bin/htop");
        assert_eq!(mutations[0].operation, "create");
    }

    #[test]
    fn test_journal_deduplication() {
        let conn = setup_in_memory_db();

        let record = pkgundo::journal::MutationRecord {
            id: None,
            txid: 1,
            pid: Some(1234),
            operation: "modify".to_string(),
            path: "/etc/ld.so.cache".to_string(),
            timestamp: chrono::Utc::now(),
            file_category: "Cache".to_string(),
            pre_hash: None,
            post_hash: None,
        };

        pkgundo::journal::append_mutation(&conn, &record).ok();
        pkgundo::journal::append_mutation(&conn, &record).ok(); // duplicate

        let mutations = pkgundo::journal::get_mutations(&conn, 1).unwrap();
        assert_eq!(mutations.len(), 1, "Duplicate mutation should be deduped");
    }

    // ── Classifier tests ────────────────────────────────────────────────────

    #[test]
    fn test_classifier_user_data_never_touched() {
        use pkgundo::classifier::{classify_path, FileCategory};
        let path = Path::new("/home/user/.bashrc");
        assert_eq!(classify_path(path), FileCategory::UserData);
    }

    #[test]
    fn test_classifier_config_archived() {
        use pkgundo::classifier::{classify_path, FileCategory};
        assert_eq!(classify_path(Path::new("/etc/nginx/nginx.conf")), FileCategory::Config);
        assert_eq!(classify_path(Path::new("/etc/systemd/system/myservice.service")), FileCategory::ServiceUnit);
    }

    #[test]
    fn test_classifier_cache_safe_delete() {
        use pkgundo::classifier::{classify_path, FileCategory};
        assert_eq!(classify_path(Path::new("/var/cache/pacman/pkg/htop.pkg.tar.zst")), FileCategory::Cache);
        assert_eq!(classify_path(Path::new("/tmp/some-temp-file")), FileCategory::TempFile);
    }

    #[test]
    fn test_classifier_binaries() {
        use pkgundo::classifier::{classify_path, FileCategory};
        assert_eq!(classify_path(Path::new("/usr/bin/htop")), FileCategory::Binary);
        assert_eq!(classify_path(Path::new("/usr/sbin/nginx")), FileCategory::Binary);
    }

    #[test]
    fn test_classifier_libraries() {
        use pkgundo::classifier::{classify_path, FileCategory};
        assert_eq!(classify_path(Path::new("/usr/lib/libssl.so.3")), FileCategory::Library);
        assert_eq!(classify_path(Path::new("/lib64/libc.so.6")), FileCategory::Library);
    }

    // ── Fingerprint tests ───────────────────────────────────────────────────

    #[test]
    fn test_sha256_hash() {
        use pkgundo::fingerprint::compute_sha256;
        use std::io::Write;

        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("test.txt");
        let mut f = fs::File::create(&file_path).unwrap();
        f.write_all(b"hello world").unwrap();

        let hash = compute_sha256(&file_path).expect("sha256 failed");
        // SHA256 of "hello world" is a specific known value
        assert_eq!(hash, "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9");
        assert_eq!(hash.len(), 64, "SHA256 hex should be 64 chars");
    }

    #[test]
    fn test_fingerprint_comparison_unchanged() {
        use pkgundo::fingerprint::{compute_sha256, compare_with_current, FileFingerprint, FingerprintDiff};
        use std::io::Write;

        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("stable.txt");
        let mut f = fs::File::create(&file_path).unwrap();
        f.write_all(b"stable content").unwrap();

        let hash = compute_sha256(&file_path).unwrap();

        let fp = FileFingerprint {
            id: None,
            txid: 1,
            path: file_path.to_string_lossy().to_string(),
            phase: "pre".to_string(),
            sha256: Some(hash),
            size: None,
            uid: None,
            gid: None,
            mode: None,
            mtime: None,
            captured_at: chrono::Utc::now(),
            is_symlink: false,
            symlink_target: None,
        };

        assert_eq!(compare_with_current(&fp), FingerprintDiff::Unchanged);
    }

    // ── Mutation summary tests ──────────────────────────────────────────────

    #[test]
    fn test_mutation_summary() {
        use pkgundo::journal::{MutationRecord, summarize_mutations};

        let records = vec![
            MutationRecord {
                id: None, txid: 1, pid: None,
                operation: "create".to_string(),
                path: "/usr/bin/a".to_string(),
                timestamp: chrono::Utc::now(),
                file_category: "Binary".to_string(),
                pre_hash: None, post_hash: None,
            },
            MutationRecord {
                id: None, txid: 1, pid: None,
                operation: "create".to_string(),
                path: "/usr/bin/b".to_string(),
                timestamp: chrono::Utc::now(),
                file_category: "Binary".to_string(),
                pre_hash: None, post_hash: None,
            },
            MutationRecord {
                id: None, txid: 1, pid: None,
                operation: "modify".to_string(),
                path: "/etc/ld.so.cache".to_string(),
                timestamp: chrono::Utc::now(),
                file_category: "Cache".to_string(),
                pre_hash: None, post_hash: None,
            },
        ];

        let summary = summarize_mutations(&records);
        assert_eq!(summary.created, 2);
        assert_eq!(summary.modified, 1);
        assert_eq!(summary.deleted, 0);
        assert_eq!(summary.total, 3);
    }

    // ── Phase 9: Service tracker tests ─────────────────────────────────────

    #[test]
    fn test_parse_systemctl_enable() {
        use pkgundo::service_tracker::{parse_cmdline_for_systemctl, ServiceAction};
        let cmdline = b"systemctl\0enable\0nginx.service\0";
        let result = parse_cmdline_for_systemctl(cmdline);
        assert!(result.is_some(), "Should detect systemctl enable");
        let (action, services) = result.unwrap();
        assert_eq!(action, ServiceAction::Enable);
        assert_eq!(services, vec!["nginx.service"]);
    }

    #[test]
    fn test_parse_systemctl_disable_multiple() {
        use pkgundo::service_tracker::{parse_cmdline_for_systemctl, ServiceAction};
        let cmdline = b"systemctl\0disable\0apache2.service\0php-fpm.service\0";
        let result = parse_cmdline_for_systemctl(cmdline);
        assert!(result.is_some());
        let (action, services) = result.unwrap();
        assert_eq!(action, ServiceAction::Disable);
        assert_eq!(services.len(), 2);
    }

    #[test]
    fn test_parse_non_systemctl_returns_none() {
        use pkgundo::service_tracker::parse_cmdline_for_systemctl;
        let cmdline = b"pacman\0-S\0htop\0";
        assert!(parse_cmdline_for_systemctl(cmdline).is_none());
    }

    #[test]
    fn test_service_action_inverse() {
        use pkgundo::service_tracker::ServiceAction;
        assert_eq!(ServiceAction::Enable.inverse(), Some(ServiceAction::Disable));
        assert_eq!(ServiceAction::Disable.inverse(), Some(ServiceAction::Enable));
        assert_eq!(ServiceAction::Start.inverse(), Some(ServiceAction::Stop));
        assert_eq!(ServiceAction::DaemonReload.inverse(), None);
    }

    #[test]
    fn test_service_event_record_and_load() {
        use pkgundo::service_tracker::{ServiceAction, ServiceEvent, record_service_event, get_service_events};
        let conn = setup_in_memory_db();
        // Create service_events table
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS service_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                txid INTEGER NOT NULL,
                service_name TEXT NOT NULL,
                action TEXT NOT NULL,
                pre_state TEXT,
                timestamp TEXT NOT NULL,
                UNIQUE(txid, service_name, action)
            );"
        ).unwrap();

        let event = ServiceEvent {
            id: None,
            txid: 1,
            service_name: "nginx.service".to_string(),
            action: ServiceAction::Enable,
            pre_state: Some("disabled".to_string()),
            timestamp: chrono::Utc::now(),
        };

        record_service_event(&conn, &event).expect("record failed");

        let events = get_service_events(&conn, 1).expect("get failed");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].service_name, "nginx.service");
        assert_eq!(events[0].action, ServiceAction::Enable);
    }

    // ── Phase 9: Blob store tests ───────────────────────────────────────────

    #[test]
    fn test_blob_store_round_trip() {
        use pkgundo::blob_store::{store_file_blob, get_blob_content};
        use std::io::Write;

        let conn = setup_in_memory_db();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS file_blobs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                txid INTEGER NOT NULL,
                path TEXT NOT NULL,
                phase TEXT NOT NULL,
                content BLOB NOT NULL,
                sha256 TEXT NOT NULL,
                size INTEGER NOT NULL,
                uid INTEGER,
                gid INTEGER,
                mode INTEGER,
                captured_at TEXT NOT NULL,
                UNIQUE(txid, path, phase)
            );"
        ).unwrap();

        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("config.conf");
        let mut f = fs::File::create(&file_path).unwrap();
        f.write_all(b"server_name localhost;\nlisten 80;\n").unwrap();
        drop(f);

        let stored = store_file_blob(&conn, 1, &file_path, "pre").expect("store failed");
        assert!(stored, "Should have stored the file");

        let content = get_blob_content(&conn, 1, &file_path.to_string_lossy(), "pre")
            .expect("get failed");
        assert!(content.is_some(), "Content should be retrievable");
        assert_eq!(content.unwrap(), b"server_name localhost;\nlisten 80;\n");
    }

    #[test]
    fn test_blob_store_skips_large_files() {
        use pkgundo::blob_store::{store_file_blob, MAX_BLOB_SIZE};
        use std::io::Write;

        let conn = setup_in_memory_db();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS file_blobs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                txid INTEGER NOT NULL,
                path TEXT NOT NULL,
                phase TEXT NOT NULL,
                content BLOB NOT NULL,
                sha256 TEXT NOT NULL,
                size INTEGER NOT NULL,
                uid INTEGER, gid INTEGER, mode INTEGER,
                captured_at TEXT NOT NULL,
                UNIQUE(txid, path, phase)
            );"
        ).unwrap();

        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("large.bin");
        let mut f = fs::File::create(&file_path).unwrap();
        // Write slightly over MAX_BLOB_SIZE
        let data = vec![0u8; (MAX_BLOB_SIZE + 1) as usize];
        f.write_all(&data).unwrap();

        let stored = store_file_blob(&conn, 1, &file_path, "pre").expect("no error");
        assert!(!stored, "Should have skipped large file");
    }

    // ── Phase 9: User tracker tests ─────────────────────────────────────────

    #[test]
    fn test_user_snapshot_diff_detects_new_user() {
        use pkgundo::user_tracker::{UserGroupSnapshot, UserEventKind, diff_snapshots};

        let before = UserGroupSnapshot {
            passwd_lines: vec![
                "root:x:0:0:root:/root:/bin/bash".to_string(),
                "nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin".to_string(),
            ],
            group_lines: vec!["root:x:0:".to_string()],
        };

        let after = UserGroupSnapshot {
            passwd_lines: vec![
                "root:x:0:0:root:/root:/bin/bash".to_string(),
                "nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin".to_string(),
                "nginx:x:999:999:nginx user:/var/lib/nginx:/usr/sbin/nologin".to_string(),
            ],
            group_lines: vec![
                "root:x:0:".to_string(),
                "nginx:x:999:".to_string(),
            ],
        };

        let events = diff_snapshots(1, &before, &after);
        let user_added: Vec<_> = events.iter().filter(|e| e.kind == UserEventKind::UserAdded).collect();
        let group_added: Vec<_> = events.iter().filter(|e| e.kind == UserEventKind::GroupAdded).collect();

        assert_eq!(user_added.len(), 1, "Should detect 1 new user");
        assert_eq!(user_added[0].name, "nginx");
        assert_eq!(group_added.len(), 1, "Should detect 1 new group");
        assert_eq!(group_added[0].name, "nginx");
    }

    #[test]
    fn test_user_snapshot_diff_detects_removed_user() {
        use pkgundo::user_tracker::{UserGroupSnapshot, UserEventKind, diff_snapshots};

        let before = UserGroupSnapshot {
            passwd_lines: vec![
                "root:x:0:0:root:/root:/bin/bash".to_string(),
                "appuser:x:1001:1001::/home/appuser:/bin/bash".to_string(),
            ],
            group_lines: vec!["root:x:0:".to_string()],
        };
        let after = UserGroupSnapshot {
            passwd_lines: vec!["root:x:0:0:root:/root:/bin/bash".to_string()],
            group_lines: vec!["root:x:0:".to_string()],
        };

        let events = diff_snapshots(1, &before, &after);
        let removed: Vec<_> = events.iter().filter(|e| e.kind == UserEventKind::UserRemoved).collect();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].name, "appuser");
    }

    // ── Phase 10: eBPF / kernel capability tests ────────────────────────────

    #[test]
    fn test_ebpf_tracer_creates_without_panic() {
        use pkgundo::ebpf::EbpfTracer;
        // Should not panic regardless of eBPF availability
        let tracer = EbpfTracer::new();
        // available is a valid boolean
        let _ = tracer.available;
    }

    #[test]
    fn test_kernel_caps_detect_runs() {
        use pkgundo::ebpf::KernelCapabilities;
        let caps = KernelCapabilities::detect();
        // Kernel version should be non-empty
        assert!(!caps.kernel_version.is_empty(), "Kernel version should be readable");
        println!("Detected kernel: {}", caps.kernel_version);
        println!("fanotify: {}, eBPF: {}, tracefs: {}",
            caps.fanotify_available, caps.ebpf_available, caps.tracefs_mounted);
    }

    #[test]
    fn test_service_action_roundtrip() {
        use pkgundo::service_tracker::ServiceAction;
        let actions = ["enable", "disable", "start", "stop", "restart", "reload", "daemon-reload"];
        for action_str in &actions {
            let action = ServiceAction::from_str(action_str).expect("Should parse");
            assert_eq!(action.as_str(), *action_str);
        }
    }

    // ── Rollback engine end-to-end flow tests ───────────────────────────────
    //
    // These drive RollbackEngine::execute() against a real on-disk SQLite file
    // (RollbackEngine opens its own connection from a path, so :memory: won't
    // work here) and real temp files, without needing root or a real package
    // manager: the "run" phase is faked by inserting transaction/mutation/blob
    // rows directly instead of actually spawning and monitoring a command.
    // Paths live under the OS temp dir so they classify as FileCategory::TempFile
    // (RollbackAction::RemoveCache), which isn't gated by NeverTouch/Skip.
    // Transactions use an unrecognized binary name so PackageManager::Unknown
    // is detected and Step C (real package-manager removal) is skipped.

    fn setup_rollback_test_db() -> (TempDir, String) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("pkgundo.db").to_string_lossy().to_string();
        pkgundo::db::init_db(&db_path).expect("init_db failed");
        (dir, db_path)
    }

    #[test]
    fn test_rollback_removes_created_file_with_no_baseline() {
        use pkgundo::journal::{append_mutation, MutationRecord};
        use pkgundo::rollback::{RollbackEngine, RollbackMode};
        use pkgundo::transaction::create_transaction;

        let (_db_dir, db_path) = setup_rollback_test_db();
        let conn = Connection::open(&db_path).unwrap();

        let args = vec!["faketool".to_string(), "install".to_string(), "foo".to_string()];
        let txid = create_transaction(&conn, "faketool install foo", &args).unwrap();

        let files_dir = TempDir::new().unwrap();
        let created_file = files_dir.path().join("created.txt");
        fs::write(&created_file, b"created content").unwrap();
        let created_path_str = created_file.to_string_lossy().to_string();

        append_mutation(&conn, &MutationRecord {
            id: None,
            txid,
            pid: None,
            operation: "create".to_string(),
            path: created_path_str.clone(),
            timestamp: chrono::Utc::now(),
            file_category: "TempFile".to_string(),
            pre_hash: None,
            post_hash: None,
        }).unwrap();

        let engine = RollbackEngine::new(txid, RollbackMode::Conservative, false, &db_path);
        let report = engine.execute().expect("rollback should succeed");

        assert!(report.success);
        assert!(!created_file.exists(), "created file with no baseline should be removed");
        assert!(report.removed.contains(&created_path_str));
        assert!(report.failed.is_empty());
    }

    #[test]
    fn test_rollback_restores_deleted_file_from_blob() {
        use pkgundo::blob_store::store_file_blob;
        use pkgundo::journal::{append_mutation, MutationRecord};
        use pkgundo::rollback::{RollbackEngine, RollbackMode};
        use pkgundo::transaction::create_transaction;

        let (_db_dir, db_path) = setup_rollback_test_db();
        let conn = Connection::open(&db_path).unwrap();

        let args = vec!["faketool".to_string(), "install".to_string(), "foo".to_string()];
        let txid = create_transaction(&conn, "faketool install foo", &args).unwrap();

        let files_dir = TempDir::new().unwrap();
        let config_file = files_dir.path().join("deleted.conf");
        fs::write(&config_file, b"original-config-content").unwrap();
        let config_path_str = config_file.to_string_lossy().to_string();

        // Simulate the pre-run blob snapshot, then the transaction deleting the file.
        store_file_blob(&conn, txid, &config_file, "pre").unwrap();
        fs::remove_file(&config_file).unwrap();

        append_mutation(&conn, &MutationRecord {
            id: None,
            txid,
            pid: None,
            operation: "delete".to_string(),
            path: config_path_str.clone(),
            timestamp: chrono::Utc::now(),
            file_category: "TempFile".to_string(),
            pre_hash: None,
            post_hash: None,
        }).unwrap();

        let engine = RollbackEngine::new(txid, RollbackMode::Conservative, false, &db_path);
        let report = engine.execute().expect("rollback should succeed");

        assert!(config_file.exists(), "deleted file should be restored from its pre-blob");
        assert_eq!(fs::read_to_string(&config_file).unwrap(), "original-config-content");
        assert!(report.restored.contains(&config_path_str));
    }

    #[test]
    fn test_rollback_archives_modified_file_and_restores_original() {
        use pkgundo::blob_store::store_file_blob;
        use pkgundo::fingerprint::{capture_fingerprint, store_fingerprint};
        use pkgundo::journal::{append_mutation, MutationRecord};
        use pkgundo::rollback::{RollbackEngine, RollbackMode};
        use pkgundo::transaction::create_transaction;

        let (_db_dir, db_path) = setup_rollback_test_db();
        let conn = Connection::open(&db_path).unwrap();

        let args = vec!["faketool".to_string(), "install".to_string(), "foo".to_string()];
        let txid = create_transaction(&conn, "faketool install foo", &args).unwrap();

        let files_dir = TempDir::new().unwrap();
        let config_file = files_dir.path().join("modified.conf");
        fs::write(&config_file, b"original-config-content").unwrap();

        // Snapshot the file as it was before the transaction ran (what
        // blob_store::pre_scan_configs would have done).
        store_file_blob(&conn, txid, &config_file, "pre").unwrap();
        let pre_fp = capture_fingerprint(txid, &config_file, "pre").unwrap();
        store_fingerprint(&conn, &pre_fp).unwrap();

        // Simulate the transaction actually changing the file's content.
        fs::write(&config_file, b"modified-by-install").unwrap();
        let config_path_str = config_file.to_string_lossy().to_string();

        append_mutation(&conn, &MutationRecord {
            id: None,
            txid,
            pid: None,
            operation: "modify".to_string(),
            path: config_path_str.clone(),
            timestamp: chrono::Utc::now(),
            file_category: "TempFile".to_string(),
            pre_hash: None,
            post_hash: None,
        }).unwrap();

        // Archive into a temp dir instead of the real /var/lib/pkgundo/archives,
        // which requires root.
        let archive_dir = TempDir::new().unwrap();
        let engine = RollbackEngine::new(txid, RollbackMode::Conservative, false, &db_path)
            .with_archive_root(archive_dir.path().to_string_lossy().to_string());
        let report = engine.execute().expect("rollback should succeed");

        assert!(report.archived.contains(&config_path_str));
        assert_eq!(
            fs::read_to_string(&config_file).unwrap(),
            "original-config-content",
            "original content should be restored from blob after archiving the modified version"
        );

        let archived_path = archive_dir.path().join(txid.to_string()).join(
            config_path_str.trim_start_matches('/'),
        );
        assert!(archived_path.exists(), "modified version should be copied into the archive");
        assert_eq!(fs::read_to_string(&archived_path).unwrap(), "modified-by-install");
    }

    #[test]
    fn test_rollback_twice_is_rejected() {
        use pkgundo::rollback::{RollbackEngine, RollbackMode};
        use pkgundo::transaction::{create_transaction, load_transaction, TransactionStatus};

        let (_db_dir, db_path) = setup_rollback_test_db();
        let conn = Connection::open(&db_path).unwrap();

        let args = vec!["faketool".to_string(), "install".to_string(), "foo".to_string()];
        let txid = create_transaction(&conn, "faketool install foo", &args).unwrap();

        let engine = RollbackEngine::new(txid, RollbackMode::Conservative, false, &db_path);
        engine.execute().expect("first rollback should succeed");

        let tx = load_transaction(&conn, txid).unwrap();
        assert_eq!(tx.status, TransactionStatus::RolledBack);

        let engine_again = RollbackEngine::new(txid, RollbackMode::Conservative, false, &db_path);
        let result = engine_again.execute();
        assert!(result.is_err(), "rolling back an already-rolled-back transaction should error");
    }
}

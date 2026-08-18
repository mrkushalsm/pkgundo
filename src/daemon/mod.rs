//! The pkgundo background daemon. Runs as a systemd service (see
//! `systemd/pkgundo-daemon.service`), exposing IPC over a Unix socket so the
//! CLI's `track`/`untrack`/`tracked` commands can durably record which apps
//! are being watched.
//!
//! Beyond the DB-backed tracking itself, the daemon detects every execution
//! of a tracked binary (`exec_watch`, a `FAN_OPEN_EXEC` fanotify group) and
//! captures that launch's `$HOME` mutations into the app's bucket
//! transaction via a lazily-started/stopped, per-filesystem-refcounted
//! shared mutation-capture group (`mutation_capture`). If `FAN_OPEN_EXEC`
//! isn't supported by the running kernel (pre-5.0), exec-watching is
//! disabled for the daemon's life but every DB-backed feature still works.

pub mod client;
mod exec_watch;
pub mod ipc;
mod mutation_capture;

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, Mutex};

use crate::ebpf::ActiveLaunch;
use exec_watch::ExecWatch;
use ipc::{Request, Response, TrackedAppView};
use mutation_capture::MutationCapture;

pub const RUN_DIR: &str = "/run/pkgundo";
pub const SOCKET_PATH: &str = "/run/pkgundo/daemon.sock";
pub const PID_PATH: &str = "/run/pkgundo/daemon.pid";

/// Run the daemon in the foreground. Returns once a shutdown signal is received.
pub async fn run_daemon(db_path: &str) -> Result<()> {
    std::fs::create_dir_all(RUN_DIR)
        .with_context(|| format!("Failed to create runtime dir {}", RUN_DIR))?;

    // The daemon is the sole owner of this socket path — clear any stale
    // file left behind by a previous unclean shutdown before binding.
    let _ = std::fs::remove_file(SOCKET_PATH);

    let listener = UnixListener::bind(SOCKET_PATH)
        .with_context(|| format!("Failed to bind daemon socket at {}", SOCKET_PATH))?;
    std::fs::set_permissions(SOCKET_PATH, std::fs::Permissions::from_mode(0o666))
        .context("Failed to set daemon socket permissions")?;
    std::fs::write(PID_PATH, std::process::id().to_string())
        .with_context(|| format!("Failed to write pid file {}", PID_PATH))?;

    let conn = crate::db::init_db(db_path).context("Failed to initialize database")?;
    let conn = Arc::new(Mutex::new(conn));

    // Exec-watching setup. `ExecWatch::load_from_db` can fail (e.g. a
    // pre-5.0 kernel without FAN_OPEN_EXEC) — that's graceful degradation,
    // not a startup failure: every DB-backed feature keeps working, only
    // live mutation capture is unavailable.
    let exec_watch: Option<Arc<ExecWatch>> = {
        let guard = conn.lock().await;
        match ExecWatch::load_from_db(&guard) {
            Ok(ew) => Some(Arc::new(ew)),
            Err(e) => {
                log::warn!(
                    "pkgundo daemon: exec-watching disabled ({}). Tracking still works; \
                     live mutation capture for launches will not.",
                    e
                );
                None
            }
        }
    };

    // One long-lived mutation channel + journal-writing collector task for
    // the daemon's whole life, decoupled from individual launches/groups
    // starting and stopping — mirrors `commands/run.rs`'s journal task.
    let (mutation_tx, mut mutation_rx) = mpsc::channel::<crate::journal::MutationRecord>(4096);
    let db_path_for_journal = db_path.to_string();
    tokio::spawn(async move {
        let conn = match crate::db::open_db(&db_path_for_journal) {
            Ok(c) => c,
            Err(e) => {
                log::error!("daemon journal task: DB open failed: {}", e);
                return;
            }
        };
        while let Some(record) = mutation_rx.recv().await {
            if let Err(e) = crate::journal::append_mutation(&conn, &record) {
                log::debug!("daemon journal task: dedup/error: {}", e);
            }
        }
    });

    let active_launches: Arc<StdMutex<HashMap<i32, ActiveLaunch>>> =
        Arc::new(StdMutex::new(HashMap::new()));
    let mutation_capture = Arc::new(MutationCapture::new(mutation_tx, Arc::clone(&active_launches)));

    if let Some(ew) = &exec_watch {
        let ew = Arc::clone(ew);
        let db_path = db_path.to_string();
        let active_launches = Arc::clone(&active_launches);
        let mutation_capture = Arc::clone(&mutation_capture);
        tokio::spawn(async move {
            ew.run(db_path, active_launches, mutation_capture).await;
        });
    }

    log::info!("pkgundo daemon listening on {}", SOCKET_PATH);

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("Failed to install SIGTERM handler")?;

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let conn = Arc::clone(&conn);
                        let exec_watch = exec_watch.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(stream, conn, exec_watch).await {
                                log::debug!("daemon: connection handler error: {}", e);
                            }
                        });
                    }
                    Err(e) => log::warn!("daemon: accept() failed: {}", e),
                }
            }
            _ = sigterm.recv() => {
                log::info!("pkgundo daemon received SIGTERM, shutting down");
                break;
            }
            _ = tokio::signal::ctrl_c() => {
                log::info!("pkgundo daemon received Ctrl-C, shutting down");
                break;
            }
        }
    }

    // Best-effort cleanup for the non-systemd (manual) run case; under
    // systemd, RuntimeDirectory=pkgundo already removes /run/pkgundo on stop.
    let _ = std::fs::remove_file(SOCKET_PATH);
    let _ = std::fs::remove_file(PID_PATH);
    Ok(())
}

async fn handle_connection(
    stream: UnixStream,
    conn: Arc<Mutex<Connection>>,
    exec_watch: Option<Arc<ExecWatch>>,
) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.context("read from client failed")?;
        if n == 0 {
            return Ok(()); // client disconnected
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<Request>(line) {
            Ok(req) => handle_request(&conn, &exec_watch, req).await,
            Err(e) => Response::Error { message: format!("Malformed request: {}", e) },
        };

        let mut out = serde_json::to_string(&response).unwrap_or_else(|_| {
            "{\"status\":\"error\",\"message\":\"failed to encode response\"}".to_string()
        });
        out.push('\n');
        write_half.write_all(out.as_bytes()).await.context("write to client failed")?;
    }
}

async fn handle_request(
    conn: &Arc<Mutex<Connection>>,
    exec_watch: &Option<Arc<ExecWatch>>,
    req: Request,
) -> Response {
    match req {
        Request::Ping => Response::Pong,
        Request::Track { name } => {
            let conn = conn.lock().await;
            match crate::tracked_apps::track_app(&conn, &name) {
                Ok(app) => {
                    if let Some(ew) = exec_watch {
                        if let Err(e) = ew.watch_app(&app.name, app.txid, &app.resolved_paths) {
                            log::warn!("daemon: failed to arm exec-watch marks for '{}': {}", app.name, e);
                        }
                    }
                    Response::Ok {
                        message: format!(
                            "Now tracking '{}' ({}: {}, {} path(s) resolved)",
                            app.name,
                            app.kind,
                            app.package_name.as_deref().unwrap_or("n/a"),
                            app.resolved_paths.len()
                        ),
                    }
                }
                Err(e) => Response::Error { message: format!("{:#}", e) },
            }
        }
        Request::Untrack { name } => {
            let conn = conn.lock().await;
            match crate::tracked_apps::untrack_app(&conn, &name) {
                Ok(()) => {
                    if let Some(ew) = exec_watch {
                        ew.unwatch_app(&name);
                    }
                    Response::Ok { message: format!("Stopped tracking '{}'", name) }
                }
                Err(e) => Response::Error { message: format!("{:#}", e) },
            }
        }
        Request::ListTracked { all } => {
            let conn = conn.lock().await;
            match crate::tracked_apps::list_tracked_apps(&conn, all) {
                Ok(apps) => Response::TrackedList {
                    apps: apps.iter().map(TrackedAppView::from).collect(),
                },
                Err(e) => Response::Error { message: format!("{:#}", e) },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Can't simulate a real pre-5.0 kernel to test `ExecWatch::try_new()`
    /// actually failing, but this verifies the thing that actually matters:
    /// every request the daemon serves keeps working correctly when
    /// exec-watching is disabled (`exec_watch: None`), not just "doesn't panic".
    #[tokio::test]
    async fn handle_request_works_with_exec_watch_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("pkgundo.db");
        let conn = Arc::new(Mutex::new(crate::db::init_db(db_path.to_str().unwrap()).unwrap()));
        let exec_watch: Option<Arc<ExecWatch>> = None;

        assert!(matches!(handle_request(&conn, &exec_watch, Request::Ping).await, Response::Pong));

        let resp =
            handle_request(&conn, &exec_watch, Request::Track { name: "/bin/ls".to_string() }).await;
        assert!(matches!(resp, Response::Ok { .. }), "expected Ok, got {:?}", resp);

        let resp = handle_request(&conn, &exec_watch, Request::ListTracked { all: false }).await;
        match resp {
            Response::TrackedList { apps } => assert_eq!(apps.len(), 1),
            other => panic!("expected TrackedList, got {:?}", other),
        }

        let resp =
            handle_request(&conn, &exec_watch, Request::Untrack { name: "/bin/ls".to_string() }).await;
        assert!(matches!(resp, Response::Ok { .. }), "expected Ok, got {:?}", resp);
    }
}

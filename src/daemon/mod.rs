//! The pkgundo background daemon. Runs as a systemd service (see
//! `systemd/pkgundo-daemon.service`), exposing IPC over a Unix socket so the
//! CLI's `track`/`untrack`/`tracked` commands can durably record which apps
//! are being watched.
//!
//! Foundational slice only: the daemon accepts and durably records
//! track/untrack requests, but does not yet watch anything via fanotify
//! (FAN_OPEN_EXEC-based exec watching is future work layered on top of this).

pub mod client;
pub mod ipc;

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

use ipc::{Request, Response, TrackedAppView};

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

    log::info!("pkgundo daemon listening on {}", SOCKET_PATH);

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("Failed to install SIGTERM handler")?;

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let conn = Arc::clone(&conn);
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(stream, conn).await {
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

async fn handle_connection(stream: UnixStream, conn: Arc<Mutex<Connection>>) -> Result<()> {
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
            Ok(req) => handle_request(&conn, req).await,
            Err(e) => Response::Error { message: format!("Malformed request: {}", e) },
        };

        let mut out = serde_json::to_string(&response).unwrap_or_else(|_| {
            "{\"status\":\"error\",\"message\":\"failed to encode response\"}".to_string()
        });
        out.push('\n');
        write_half.write_all(out.as_bytes()).await.context("write to client failed")?;
    }
}

async fn handle_request(conn: &Arc<Mutex<Connection>>, req: Request) -> Response {
    match req {
        Request::Ping => Response::Pong,
        Request::Track { name } => {
            let conn = conn.lock().await;
            match crate::tracked_apps::track_app(&conn, &name) {
                Ok(app) => Response::Ok {
                    message: format!(
                        "Now tracking '{}' ({}: {}, {} path(s) resolved)",
                        app.name,
                        app.kind,
                        app.package_name.as_deref().unwrap_or("n/a"),
                        app.resolved_paths.len()
                    ),
                },
                Err(e) => Response::Error { message: format!("{:#}", e) },
            }
        }
        Request::Untrack { name } => {
            let conn = conn.lock().await;
            match crate::tracked_apps::untrack_app(&conn, &name) {
                Ok(()) => Response::Ok { message: format!("Stopped tracking '{}'", name) },
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

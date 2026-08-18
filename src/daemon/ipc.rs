//! Wire protocol between the daemon and CLI clients: newline-delimited JSON
//! over a Unix domain socket. No framing library needed — one JSON value per
//! line, using serde_json (already a dependency) and tokio's line-buffered
//! reader/writer.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Track { name: String },
    Untrack { name: String },
    ListTracked { all: bool },
    Ping,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Ok { message: String },
    TrackedList { apps: Vec<TrackedAppView> },
    Error { message: String },
    Pong,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TrackedAppView {
    pub name: String,
    pub kind: String,
    pub package_name: Option<String>,
    pub resolved_paths: Vec<String>,
    pub status: String,
    pub created_at: String,
    pub txid: i64,
}

impl From<&crate::tracked_apps::TrackedApp> for TrackedAppView {
    fn from(app: &crate::tracked_apps::TrackedApp) -> Self {
        Self {
            name: app.name.clone(),
            kind: app.kind.clone(),
            package_name: app.package_name.clone(),
            resolved_paths: app.resolved_paths.clone(),
            status: app.status.clone(),
            created_at: app.created_at.clone(),
            txid: app.txid,
        }
    }
}

//! Thin IPC client used by the CLI's track/untrack/tracked commands. The CLI
//! never touches the tracked_apps DB or pacman directly — the daemon owns all
//! of that; the CLI just forwards a request and prints the response.

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use super::ipc::{Request, Response};
use super::SOCKET_PATH;

pub async fn send_request(req: Request) -> Result<Response> {
    let stream = UnixStream::connect(SOCKET_PATH).await.with_context(|| {
        format!(
            "pkgundo daemon is not running (could not connect to {}). Start it with: sudo systemctl start pkgundo-daemon",
            SOCKET_PATH
        )
    })?;

    let (read_half, mut write_half) = stream.into_split();
    let mut line = serde_json::to_string(&req).context("Failed to encode request")?;
    line.push('\n');
    write_half.write_all(line.as_bytes()).await.context("Failed to send request to daemon")?;

    let mut reader = BufReader::new(read_half);
    let mut response_line = String::new();
    let n = reader
        .read_line(&mut response_line)
        .await
        .context("Failed to read response from daemon")?;
    if n == 0 {
        bail!("Daemon closed the connection without responding");
    }

    serde_json::from_str(response_line.trim()).context("Failed to decode daemon response")
}

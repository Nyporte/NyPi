//! Local control socket of the daemon's Telegram channel. Only one process may poll a bot, so
//! other NyPi processes (a terminal with a session of its own, a scheduled job) send their
//! approval requests here, and the channel asks the user in Telegram. The socket lives in
//! `~/.nypi/run/` with owner-only permissions; a connecting process can ask, but only a paired
//! chat can answer.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use nypi_core::inspector::ApprovalRequest;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

/// Requests are one JSON line; anything longer is refused.
const MAX_LINE: u64 = 64 * 1024;
/// `sun_path` is 104 bytes on macOS (108 on Linux), including the terminating NUL.
const MAX_SOCKET_PATH: usize = 103;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum BridgeRequest {
    Approval { origin: String, tool: String, summary: String, reason: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BridgeDecision {
    pub approved: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// An approval request received from another process.
pub struct RemoteApproval {
    pub origin: String,
    pub tool: String,
    pub summary: String,
    pub reason: String,
    respond: oneshot::Sender<BridgeDecision>,
}

impl RemoteApproval {
    /// False once the asking process gave up (answered locally, timed out, or exited).
    pub fn is_waiting(&self) -> bool {
        !self.respond.is_closed()
    }

    /// Returns false when the asker is no longer waiting.
    pub fn answer(self, decision: BridgeDecision) -> bool {
        self.respond.send(decision).is_ok()
    }
}

/// Bind an owner-only Unix socket in `~/.nypi/run/`, for `what`. Fails when one is already
/// listening there; a stale file is replaced.
pub fn bind_owner_socket(path: &Path, what: &str) -> anyhow::Result<UnixListener> {
    let length = path.as_os_str().len();
    if length > MAX_SOCKET_PATH {
        bail!(
            "the {what} socket path {} is {length} bytes; Unix sockets allow {MAX_SOCKET_PATH}. Use a shorter NYPI_HOME.",
            path.display()
        );
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            bail!("another {what} is already running ({} is in use)", path.display());
        }
        std::fs::remove_file(path).with_context(|| format!("cannot remove stale socket {}", path.display()))?;
    }
    let listener = UnixListener::bind(path).with_context(|| format!("cannot listen on {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Bind the bridge's socket and take approval requests from it.
pub fn listen(path: &Path) -> anyhow::Result<mpsc::UnboundedReceiver<RemoteApproval>> {
    let listener = bind_owner_socket(path, "Telegram bridge")?;
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                // A question put here reaches the owner as Lucia's: a program she started may
                // not put one, as it may not speak on the daemon's socket.
                if crate::daemon::started_by_this_process(&stream).await {
                    return crate::daemon::log("refused a bridge connection from a program Lucia started");
                }
                serve_connection(stream, tx).await;
            });
        }
    });
    Ok(rx)
}

async fn serve_connection(stream: UnixStream, requests: mpsc::UnboundedSender<RemoteApproval>) {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read.take(MAX_LINE));
    let mut line = String::new();
    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
        return;
    }
    let Ok(BridgeRequest::Approval { origin, tool, summary, reason }) = serde_json::from_str(&line) else {
        let _ = write.write_all(b"{\"approved\":false,\"reason\":\"invalid request\"}\n").await;
        return;
    };
    let (respond, answer) = oneshot::channel();
    if requests.send(RemoteApproval { origin, tool, summary, reason, respond }).is_err() {
        return;
    }
    // The asker closing the connection means it stopped waiting; dropping `answer` lets the
    // bridge see that through `is_waiting`.
    let mut probe = [0u8; 1];
    let decision = tokio::select! {
        decision = answer => decision,
        _ = reader.read(&mut probe) => return,
    };
    if let Ok(decision) = decision {
        let mut reply = serde_json::to_vec(&decision).unwrap_or_default();
        reply.push(b'\n');
        let _ = write.write_all(&reply).await;
    }
}

/// Ask the bridge. None when no bridge is listening or it went away before answering.
pub async fn ask(path: &Path, request: &BridgeRequest) -> Option<BridgeDecision> {
    let stream = UnixStream::connect(path).await.ok()?;
    let (read, mut write) = stream.into_split();
    let mut line = serde_json::to_vec(request).ok()?;
    line.push(b'\n');
    write.write_all(&line).await.ok()?;
    let mut reply = String::new();
    BufReader::new(read.take(MAX_LINE)).read_line(&mut reply).await.ok()?;
    serde_json::from_str(&reply).ok()
}

/// Whether a bridge appears to be running (the socket exists).
pub fn available(path: &Path) -> bool {
    path.exists()
}

/// Forward a local approval request to the bridge and apply its answer. Returns the request
/// unanswered when no bridge could take it.
pub async fn forward(path: PathBuf, origin: String, request: ApprovalRequest) -> Option<ApprovalRequest> {
    let ask_for = BridgeRequest::Approval {
        origin,
        tool: request.tool.clone(),
        summary: request.summary.clone(),
        reason: request.reason.clone(),
    };
    match ask(&path, &ask_for).await {
        Some(decision) if decision.approved => {
            request.approve();
            None
        }
        Some(decision) => {
            request.deny(decision.reason);
            None
        }
        None => Some(request),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trip_and_abandoned_requests() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run/bridge.sock");
        let mut requests = listen(&path).unwrap();
        assert!(listen(&path).is_err(), "a second bridge is refused");

        let request = BridgeRequest::Approval {
            origin: "test".into(),
            tool: "bash".into(),
            summary: "git push".into(),
            reason: "pushing".into(),
        };
        let asker = tokio::spawn({
            let path = path.clone();
            let request = request.clone();
            async move { ask(&path, &request).await }
        });
        let remote = requests.recv().await.unwrap();
        assert_eq!(remote.summary, "git push");
        assert!(remote.is_waiting());
        assert!(remote.answer(BridgeDecision { approved: false, reason: Some("no".into()) }));
        let decision = asker.await.unwrap().unwrap();
        assert!(!decision.approved);
        assert_eq!(decision.reason.as_deref(), Some("no"));

        let asker = tokio::spawn({
            let path = path.clone();
            async move { ask(&path, &request).await }
        });
        let remote = requests.recv().await.unwrap();
        asker.abort();
        let _ = asker.await;
        for _ in 0..50 {
            if !remote.is_waiting() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(!remote.is_waiting(), "an asker that hung up is no longer waiting");
        assert!(
            ask(
                &dir.path().join("none.sock"),
                &BridgeRequest::Approval {
                    origin: String::new(),
                    tool: String::new(),
                    summary: String::new(),
                    reason: String::new(),
                }
            )
            .await
            .is_none()
        );
    }
}

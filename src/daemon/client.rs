//! Client actor for the `serve`↔`daemon` protocol (`protocol.rs`), mirroring
//! `LspClient`'s shape (`src/lsp/client.rs`): a cheap-to-clone handle backed
//! by an `mpsc`-driven actor that owns the connection and pairs
//! request/response by id, so multiple concurrent tool calls from `serve`'s
//! rmcp dispatcher can share one physical `UnixStream`.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Result, anyhow};
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};

use super::protocol::{DaemonEnvelope, DaemonRequest, DaemonResponseEnvelope, read_line, write_line};

type Reply = oneshot::Sender<Result<serde_json::Value, String>>;

enum Cmd {
    Call { request: DaemonRequest, reply: Reply },
}

/// Inbound events produced by the reader task.
enum Inbound {
    Response(DaemonResponseEnvelope),
    /// Clean EOF between messages.
    Eof,
    /// Unparseable message or transport error.
    Failed(String),
}

/// Handle to the daemon connection. Cheap to clone: the `mpsc::Sender` is
/// shared across every clone, all funneling into the same actor/connection.
#[derive(Clone)]
pub struct DaemonClient {
    tx: mpsc::Sender<Cmd>,
}

impl DaemonClient {
    /// Connect to the daemon's socket and spawn the client actor over it.
    pub async fn connect(sock_path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(sock_path)
            .await
            .map_err(|e| anyhow!("cannot connect to daemon at {}: {e}", sock_path.display()))?;
        let (read_half, write_half) = stream.into_split();
        Ok(Self::spawn(read_half, write_half))
    }

    /// Spawn the actor over the given halves. Generic so tests can drive it
    /// over `tokio::io::duplex` without a real socket.
    pub fn spawn<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>(64);
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel::<Inbound>();
        tokio::spawn(reader_loop(reader, inbound_tx));
        tokio::spawn(actor_loop(cmd_rx, inbound_rx, writer));
        Self { tx: cmd_tx }
    }

    /// Send `request` and await the daemon's response, reduced to a plain
    /// `anyhow::Error` on either a protocol-level failure or a tool-level
    /// error the daemon reported.
    pub async fn call(&self, request: DaemonRequest) -> Result<serde_json::Value> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Cmd::Call { request, reply: tx })
            .await
            .map_err(|_| anyhow!("daemon client closed"))?;
        rx.await
            .map_err(|_| anyhow!("daemon client dropped reply"))?
            .map_err(|msg| anyhow!("daemon error: {msg}"))
    }
}

async fn reader_loop<R>(reader: R, inbound_tx: mpsc::UnboundedSender<Inbound>)
where
    R: AsyncRead + Send + Unpin + 'static,
{
    let mut reader = BufReader::new(reader);
    loop {
        match read_line::<_, DaemonResponseEnvelope>(&mut reader).await {
            Ok(Some(response)) => {
                if inbound_tx.send(Inbound::Response(response)).is_err() {
                    return;
                }
            }
            Ok(None) => {
                let _ = inbound_tx.send(Inbound::Eof);
                return;
            }
            Err(err) => {
                let _ = inbound_tx.send(Inbound::Failed(err.to_string()));
                return;
            }
        }
    }
}

async fn actor_loop<W>(
    mut cmd_rx: mpsc::Receiver<Cmd>,
    mut inbound_rx: mpsc::UnboundedReceiver<Inbound>,
    mut writer: W,
) where
    W: AsyncWrite + Send + Unpin + 'static,
{
    let mut pending: HashMap<u64, Reply> = HashMap::new();
    let mut next_id: u64 = 0;
    let mut alive = true;
    let mut close_reason = None;

    while alive {
        tokio::select! {
            biased;
            inbound = inbound_rx.recv() => match inbound {
                Some(Inbound::Response(resp)) => {
                    if let Some(reply) = pending.remove(&resp.id) {
                        let _ = reply.send(resp.result);
                    }
                }
                Some(Inbound::Eof) | None => alive = false,
                Some(Inbound::Failed(msg)) => {
                    close_reason = Some(msg);
                    alive = false;
                }
            },
            cmd = cmd_rx.recv() => match cmd {
                Some(Cmd::Call { request, reply }) => {
                    let id = next_id;
                    next_id += 1;
                    let envelope = DaemonEnvelope { id, request };
                    match write_line(&mut writer, &envelope).await {
                        Ok(()) => {
                            pending.insert(id, reply);
                        }
                        Err(e) => {
                            let _ = reply.send(Err(format!("write failed: {e}")));
                        }
                    }
                }
                None => alive = false,
            },
        }
    }

    let reason = close_reason.unwrap_or_else(|| "daemon connection closed".to_string());
    for (_, reply) in pending.drain() {
        let _ = reply.send(Err(reason.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::dto::RestartLspInput;
    use tokio::io::duplex;

    /// A minimal fake daemon: echoes back `restarted: []` for any
    /// `RestartLsp` request, exercising the client's request/response
    /// pairing without a real `UnixListener`/`SemnavServer`.
    async fn fake_daemon<R, W>(reader: R, mut writer: W)
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut reader = BufReader::new(reader);
        loop {
            match read_line::<_, DaemonEnvelope>(&mut reader).await {
                Ok(Some(envelope)) => {
                    let response = DaemonResponseEnvelope {
                        id: envelope.id,
                        result: Ok(serde_json::json!({"restarted": []})),
                    };
                    if write_line(&mut writer, &response).await.is_err() {
                        return;
                    }
                }
                _ => return,
            }
        }
    }

    #[tokio::test]
    async fn call_round_trips_through_a_fake_daemon() {
        let (client_reader, server_writer) = duplex(4096);
        let (server_reader, client_writer) = duplex(4096);
        tokio::spawn(fake_daemon(server_reader, server_writer));

        let client = DaemonClient::spawn(client_reader, client_writer);
        let value = client
            .call(DaemonRequest::RestartLsp(RestartLspInput { language: None }))
            .await
            .unwrap();
        assert_eq!(value["restarted"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn concurrent_calls_on_one_connection_get_matched_replies() {
        let (client_reader, server_writer) = duplex(8192);
        let (server_reader, client_writer) = duplex(8192);
        tokio::spawn(fake_daemon(server_reader, server_writer));

        let client = DaemonClient::spawn(client_reader, client_writer);
        let (a, b, c) = tokio::join!(
            client.call(DaemonRequest::RestartLsp(RestartLspInput { language: None })),
            client.call(DaemonRequest::RestartLsp(RestartLspInput { language: None })),
            client.call(DaemonRequest::RestartLsp(RestartLspInput { language: None })),
        );
        assert!(a.is_ok() && b.is_ok() && c.is_ok());
    }

    #[tokio::test]
    async fn pending_calls_fail_when_the_connection_closes() {
        let (client_reader, server_writer) = duplex(64);
        let (_server_reader, client_writer) = duplex(64);
        drop(server_writer);

        let client = DaemonClient::spawn(client_reader, client_writer);
        let result = client
            .call(DaemonRequest::RestartLsp(RestartLspInput { language: None }))
            .await;
        assert!(result.is_err());
    }
}

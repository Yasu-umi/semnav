//! LSP client actor — owns the server's stdio streams, pairs request/response
//! by id via oneshot channels. Thin layer (`crate-structure.md` Decision Point 2).
//!
//! The health state machine, backoff restart, timeouts, and child-process
//! supervision arrive in a later step. This module is the transport-level
//! request/response core, parameterized over generic `AsyncRead`/`AsyncWrite`
//! so tests can drive it over `tokio::io::duplex` without spawning a real
//! language server.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::{mpsc, oneshot};
use tracing::Instrument;

use super::proto::{Id, JsonRpcVersion, Message, RpcError};
use super::transport::{read_message, write_message};

/// A pending reply channel: inner `Ok` carries a successful result, inner `Err`
/// a JSON-RPC error object.
type Reply = oneshot::Sender<std::result::Result<Value, RpcError>>;

enum Cmd {
    Request {
        method: String,
        params: Option<Value>,
        reply: Reply,
    },
    Notify {
        method: String,
        params: Option<Value>,
    },
}

/// Inbound events produced by the reader task.
enum Inbound {
    Message(Message),
    /// Clean EOF between messages.
    Eof,
    /// Unparseable message or transport error.
    Failed(String),
}

/// Handle to the LSP client. Cheap to clone: the `mpsc::Sender` and the
/// open-document map are both shared (`Arc`-backed) across every clone.
#[derive(Clone, Debug)]
pub struct LspClient {
    tx: mpsc::Sender<Cmd>,
    // uri -> (version, last-sent text). Shared across every clone of this
    // client so the FS watcher (`indexer::reconcile`) and query-time
    // `ensure_open` (`query::resolver`) — which acquire the *same* live
    // connection per `QueryRuntime::acquire_for_watcher` — agree on whether a
    // document is already open and at what version, instead of each
    // independently firing `didOpen` at a hardcoded version 1. A stray
    // `didOpen` for an already-open uri (at a version the server has already
    // moved past) silently desyncs the server's semantic analysis —
    // `documentSymbol` still reparses fine since it's purely syntactic, but
    // `references`/call-hierarchy queries start returning empty.
    open_docs: Arc<Mutex<HashMap<String, (i32, String)>>>,
}

impl LspClient {
    /// Spawn the actor over the given server stdio streams. The reader task and
    /// the actor task run until every `LspClient` clone is dropped (and pending
    /// work drains) or the stream ends.
    pub fn spawn<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>(64);
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel::<Inbound>();
        tokio::spawn(reader_loop(reader, inbound_tx));
        tokio::spawn(actor_loop(cmd_rx, inbound_rx, writer));
        Self {
            tx: cmd_tx,
            open_docs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Send a request and await its result. Spans this as `lsp_request`
    /// (`docs/design/observability.md`) — its `time.busy` on span close is the
    /// actual LSP round-trip (including the actor's own queue wait), the
    /// figure to diff against an enclosing `tool` span to tell "LSP was slow"
    /// from "semnav was slow".
    pub async fn request(&self, method: &str, params: Option<Value>) -> Result<Value> {
        let span = tracing::debug_span!("lsp_request", method = %method);
        async move {
            let (tx, rx) = oneshot::channel();
            self.tx
                .send(Cmd::Request {
                    method: method.to_string(),
                    params,
                    reply: tx,
                })
                .await
                .map_err(|_| anyhow!("lsp client closed"))?;
            match rx.await.map_err(|_| anyhow!("lsp client dropped reply"))? {
                Ok(value) => Ok(value),
                Err(err) => Err(anyhow!(
                    "lsp rpc error (code {}): {}",
                    err.code,
                    err.message
                )),
            }
        }
        .instrument(span)
        .await
    }

    /// Send a fire-and-forget notification.
    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<()> {
        self.tx
            .send(Cmd::Notify {
                method: method.to_string(),
                params,
            })
            .await
            .map_err(|_| anyhow!("lsp client closed"))?;
        Ok(())
    }

    /// Ensure `uri` is open on the server with `text` as its current content:
    /// `textDocument/didOpen` on first touch, a whole-document `didChange`
    /// (rangeless replace) on a later touch whose text actually changed, or a
    /// no-op if `text` matches what's already open. Every clone of this
    /// `LspClient` shares `open_docs`, so callers that acquire the same live
    /// connection (the FS watcher and query-time `ensure_open`) never race
    /// each other into sending a redundant/version-regressed `didOpen`.
    pub async fn ensure_document(&self, uri: &str, language_id: &str, text: &str) -> Result<()> {
        enum Action {
            Skip,
            Open,
            Change(i32),
        }
        let action = {
            let mut docs = self.open_docs.lock().unwrap();
            match docs.get(uri) {
                Some((_, existing)) if existing == text => Action::Skip,
                Some((version, _)) => {
                    let next = version + 1;
                    docs.insert(uri.to_string(), (next, text.to_string()));
                    Action::Change(next)
                }
                None => {
                    docs.insert(uri.to_string(), (1, text.to_string()));
                    Action::Open
                }
            }
        };
        match action {
            Action::Skip => Ok(()),
            Action::Open => {
                self.notify(
                    "textDocument/didOpen",
                    Some(json!({
                        "textDocument": {
                            "uri": uri,
                            "languageId": language_id,
                            "version": 1,
                            "text": text,
                        }
                    })),
                )
                .await
            }
            Action::Change(version) => {
                self.notify(
                    "textDocument/didChange",
                    Some(json!({
                        "textDocument": { "uri": uri, "version": version },
                        "contentChanges": [{ "text": text }],
                    })),
                )
                .await
            }
        }
    }
}

/// Read framed messages from the server until EOF/error, forwarding each to the
/// actor. Owns the `BufReader` so prefetched bytes survive between messages.
async fn reader_loop<R>(reader: R, tx: mpsc::UnboundedSender<Inbound>)
where
    R: AsyncRead + Unpin,
{
    let mut br = BufReader::new(reader);
    loop {
        match read_message(&mut br).await {
            Ok(Some(value)) => match Message::from_value(value) {
                Some(msg) => {
                    if tx.send(Inbound::Message(msg)).is_err() {
                        return; // actor gone
                    }
                }
                None => {
                    let _ = tx.send(Inbound::Failed("unparseable message".into()));
                    return;
                }
            },
            Ok(None) => {
                let _ = tx.send(Inbound::Eof);
                return;
            }
            Err(e) => {
                let _ = tx.send(Inbound::Failed(e.to_string()));
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
    W: AsyncWrite + Unpin,
{
    let mut next_id: Id = 1;
    let mut pending: HashMap<Id, Reply> = HashMap::new();
    let mut close_reason: Option<String> = None;
    let mut alive = true;

    while alive {
        tokio::select! {
            biased;
            // Prefer inbound so a dead stream fails pending requests promptly.
            inbound = inbound_rx.recv() => match inbound {
                Some(Inbound::Message(msg)) => dispatch_inbound(&mut pending, msg),
                Some(Inbound::Eof) | None => alive = false,
                Some(Inbound::Failed(msg)) => {
                    close_reason = Some(msg);
                    alive = false;
                }
            },
            cmd = cmd_rx.recv() => match cmd {
                Some(Cmd::Request { method, params, reply }) => {
                    let id = next_id;
                    next_id += 1;
                    let req = Message::Request {
                        jsonrpc: JsonRpcVersion,
                        id,
                        method,
                        params,
                    };
                    match write_message(&mut writer, &req).await {
                        Ok(()) => {
                            pending.insert(id, reply);
                        }
                        Err(e) => {
                            let _ = reply.send(Err(RpcError {
                                code: -32000,
                                message: format!("write failed: {e}"),
                                data: None,
                            }));
                        }
                    }
                }
                Some(Cmd::Notify { method, params }) => {
                    let notif = Message::Notification {
                        jsonrpc: JsonRpcVersion,
                        method,
                        params,
                    };
                    let _ = write_message(&mut writer, &notif).await;
                }
                None => alive = false,
            },
        }
    }

    // Stream ended or last handle dropped: fail anything still waiting,
    // surfacing the close reason (transport error) where we have one.
    let reason = close_reason.as_deref().unwrap_or("client stream closed");
    for (_, tx) in pending.drain() {
        let _ = tx.send(Err(RpcError {
            code: -32001,
            message: reason.to_string(),
            data: None,
        }));
    }
}

/// Route a decoded inbound message to the matching pending reply (if any).
fn dispatch_inbound(pending: &mut HashMap<Id, Reply>, msg: Message) {
    match msg {
        Message::Response { id, result, .. } => {
            if let Some(tx) = pending.remove(&id) {
                let _ = tx.send(Ok(result));
            }
        }
        Message::Error { id, error, .. } => {
            if let Some(tx) = pending.remove(&id) {
                let _ = tx.send(Err(error));
            }
        }
        // server→client requests/notifications are not handled in this layer.
        Message::Request { .. } | Message::Notification { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::proto::{JsonRpcVersion, Message};
    use crate::lsp::transport::{read_message, write_message};
    use tokio::io::{BufReader, duplex};

    /// A mock server: echo each request back as `{"echoed": <method>}`.
    async fn mock_server_echo(sr: tokio::io::DuplexStream, sw: tokio::io::DuplexStream) {
        let mut sr = BufReader::new(sr);
        let mut sw = sw;
        loop {
            let req = match read_message::<_>(&mut sr).await {
                Ok(Some(v)) => v,
                _ => return,
            };
            if let Some(id) = req.get("id").and_then(|i| i.as_i64()) {
                let resp = Message::Response {
                    jsonrpc: JsonRpcVersion,
                    id,
                    result: serde_json::json!({"echoed": req.get("method").cloned().unwrap_or_default()}),
                };
                if write_message(&mut sw, &resp).await.is_err() {
                    return;
                }
            }
            // notifications get no reply
        }
    }

    fn spawn_client_and_server() -> (LspClient, tokio::task::JoinHandle<()>) {
        // client writes → server reads ; server writes → client reads
        let (client_writer, server_reader) = duplex(8192);
        let (server_writer, client_reader) = duplex(8192);
        let handle = tokio::spawn(mock_server_echo(server_reader, server_writer));
        (LspClient::spawn(client_reader, client_writer), handle)
    }

    #[tokio::test]
    async fn request_response_roundtrip() {
        let (client, _server) = spawn_client_and_server();
        let v = client
            .request("ping", Some(serde_json::json!({"x": 1})))
            .await
            .expect("request ok");
        assert_eq!(v["echoed"], "ping");
    }

    #[tokio::test]
    async fn multiple_requests_pair_by_id() {
        let (client, _server) = spawn_client_and_server();
        // Concurrent requests must each get their own (correct) response.
        let r1 = tokio::spawn({
            let c = client.clone();
            async move { c.request("one", None).await }
        });
        let r2 = tokio::spawn({
            let c = client.clone();
            async move { c.request("two", None).await }
        });
        let v1 = r1.await.unwrap().expect("req1 ok");
        let v2 = r2.await.unwrap().expect("req2 ok");
        assert_eq!(v1["echoed"], "one");
        assert_eq!(v2["echoed"], "two");
    }

    #[tokio::test]
    async fn error_response_propagates() {
        let (client_writer, server_reader) = duplex(8192);
        let (server_writer, client_reader) = duplex(8192);
        tokio::spawn(async move {
            let mut sr = BufReader::new(server_reader);
            let mut sw = server_writer;
            let req = read_message::<_>(&mut sr).await.unwrap().unwrap();
            let id = req["id"].as_i64().unwrap();
            let err = Message::Error {
                jsonrpc: JsonRpcVersion,
                id,
                error: RpcError {
                    code: -32601,
                    message: "method not found".into(),
                    data: None,
                },
            };
            write_message(&mut sw, &err).await.unwrap();
        });

        let client = LspClient::spawn(client_reader, client_writer);
        let err = client.request("bad", None).await;
        assert!(err.is_err());
    }

    /// Real pyright's exact `-32601` envelope for `textDocument/implementation`
    /// (`docs/design/lsp-integration.md`: "pyright: unsupported (-32601
    /// Unhandled method)" — Python's duck typing makes "implementation" a weak
    /// concept there), observed live. Must surface as a typed `Err` carrying
    /// the code and message, not panic or hang, so a caller (once the
    /// `implements` edge is implemented) can treat it as "not supported here"
    /// rather than a crash.
    #[tokio::test]
    async fn unhandled_method_error_surfaces_pyrights_exact_envelope() {
        let (client_writer, server_reader) = duplex(8192);
        let (server_writer, client_reader) = duplex(8192);
        tokio::spawn(async move {
            let mut sr = BufReader::new(server_reader);
            let mut sw = server_writer;
            let req = read_message::<_>(&mut sr).await.unwrap().unwrap();
            let id = req["id"].as_i64().unwrap();
            let err = Message::Error {
                jsonrpc: JsonRpcVersion,
                id,
                error: RpcError {
                    code: -32601,
                    message: "Unhandled method textDocument/implementation".into(),
                    data: None,
                },
            };
            write_message(&mut sw, &err).await.unwrap();
        });

        let client = LspClient::spawn(client_reader, client_writer);
        let err = client
            .request("textDocument/implementation", None)
            .await
            .expect_err("unhandled method must surface as an error, not panic");
        let msg = err.to_string();
        assert!(msg.contains("-32601"));
        assert!(msg.contains("Unhandled method textDocument/implementation"));
    }

    #[tokio::test]
    async fn stream_close_fails_pending_request() {
        // Server half closes immediately; the client request must surface an error.
        let (client_writer, server_reader) = duplex(8192);
        let (server_writer, client_reader) = duplex(8192);
        drop(server_reader);
        drop(server_writer);

        let client = LspClient::spawn(client_reader, client_writer);
        let err = client.request("hang", None).await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn notification_needs_no_response() {
        let (client, _server) = spawn_client_and_server();
        // A notification returns immediately without waiting on the server.
        client
            .notify(
                "textDocument/didOpen",
                Some(serde_json::json!({"uri": "file:///a"})),
            )
            .await
            .expect("notify ok");
        // Follow-up request still works (actor didn't block on the notification).
        let v = client.request("after", None).await.expect("request ok");
        assert_eq!(v["echoed"], "after");
    }
}

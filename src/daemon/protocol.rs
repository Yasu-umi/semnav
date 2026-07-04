//! The `serve` ↔ `daemon` wire protocol: newline-delimited JSON over a raw
//! `UnixStream`, one request/response pair per line, multiplexed by `id` so a
//! single connection can carry multiple in-flight calls
//! (`docs/design/daemon-lifecycle.md`). Deliberately not rmcp/MCP — the two
//! processes are always the same binary, so there's no need for HTTP framing,
//! sessions, or SSE just to shuttle a handful of fixed operations one hop.
//!
//! Reuses the existing tool DTOs (`crate::mcp::dto`) verbatim as request
//! payloads rather than inventing a parallel schema.

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::mcp::dto::{
    CallGraphQueryInput, FindDefinitionInput, FindSymbolInput, ReadRangeInput, RestartLspInput,
    SymbolQueryInput,
};

/// One of the 7 tools, or the daemon-only `Shutdown` control message. Mirrors
/// `SemnavServer`'s tool set (`src/mcp/server.rs`) so the daemon's accept loop
/// can dispatch each variant to the matching inherent method.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", content = "params")]
pub enum DaemonRequest {
    FindSymbol(FindSymbolInput),
    FindDefinition(FindDefinitionInput),
    FindReferences(SymbolQueryInput),
    FindCallers(CallGraphQueryInput),
    FindCallees(CallGraphQueryInput),
    ReadRange(ReadRangeInput),
    RestartLsp(RestartLspInput),
    /// Not a query op: tells the daemon to run its graceful-shutdown path
    /// immediately, regardless of connection count (`daemon stop`).
    Shutdown,
}

/// A framed request: `id` lets one connection carry multiple concurrent
/// in-flight calls, matched back up by [`DaemonResponseEnvelope::id`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonEnvelope {
    pub id: u64,
    pub request: DaemonRequest,
}

/// A framed response. `result` is `Err(message)` for both tool-level errors
/// (e.g. `ErrorData` from a `SemnavServer` method, reduced to its message —
/// `serve`'s proxy re-wraps it into a fresh `ErrorData` on its side) and
/// protocol-level failures.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonResponseEnvelope {
    pub id: u64,
    pub result: Result<serde_json::Value, String>,
}

/// Write one envelope as a single line of JSON (serde_json escapes embedded
/// newlines, so the line boundary is unambiguous) followed by `\n`.
pub async fn write_line<W, T>(writer: &mut W, value: &T) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
    T: Serialize,
{
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

/// Read one line and parse it as JSON. `Ok(None)` on clean EOF (the peer
/// closed the connection between messages, not mid-message).
pub async fn read_line<R, T>(reader: &mut BufReader<R>) -> Result<Option<T>>
where
    R: tokio::io::AsyncRead + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Ok(None);
    }
    let value = serde_json::from_str(line.trim_end_matches('\n'))
        .map_err(|e| anyhow!("malformed daemon protocol line: {e}"))?;
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::dto::{AtRef, FilterInput, PageInput, SymbolRefInput};
    use tokio::io::duplex;

    fn sample_requests() -> Vec<DaemonRequest> {
        vec![
            DaemonRequest::FindSymbol(FindSymbolInput {
                pattern: "foo".into(),
                match_mode: Default::default(),
                ignore_case: false,
                brief: false,
                with_signature: false,
                filter: FilterInput::default(),
                page: PageInput::default(),
            }),
            DaemonRequest::FindDefinition(FindDefinitionInput {
                at: AtRef {
                    uri: "file:///repo/mod.py".into(),
                    line: 0,
                    character: 4,
                },
            }),
            DaemonRequest::FindReferences(SymbolQueryInput {
                symbol: SymbolRefInput {
                    fqn: Some("repo.foo".into()),
                    at: None,
                },
                filter: FilterInput::default(),
                page: PageInput::default(),
            }),
            DaemonRequest::ReadRange(ReadRangeInput {
                uri: "file:///repo/mod.py".into(),
                range: None,
            }),
            DaemonRequest::RestartLsp(RestartLspInput { language: None }),
            DaemonRequest::Shutdown,
        ]
    }

    #[tokio::test]
    async fn envelope_round_trips_over_a_stream_for_every_op() {
        let (mut client, mut server) = duplex(64 * 1024);

        for (id, request) in sample_requests().into_iter().enumerate() {
            let envelope = DaemonEnvelope {
                id: id as u64,
                request,
            };
            write_line(&mut client, &envelope).await.unwrap();

            let mut reader = BufReader::new(&mut server);
            let received: DaemonEnvelope = read_line(&mut reader).await.unwrap().unwrap();
            assert_eq!(received.id, envelope.id);
            // Debug-format comparison is enough here — the point is that
            // every variant round-trips through JSON without loss, not that
            // DaemonRequest implements PartialEq.
            assert_eq!(
                format!("{:?}", received.request),
                format!("{:?}", envelope.request)
            );
        }
    }

    #[tokio::test]
    async fn response_round_trips_ok_and_err() {
        let (mut client, mut server) = duplex(4096);

        let ok = DaemonResponseEnvelope {
            id: 1,
            result: Ok(serde_json::json!({"nodes": []})),
        };
        write_line(&mut client, &ok).await.unwrap();
        let mut reader = BufReader::new(&mut server);
        let received: DaemonResponseEnvelope = read_line(&mut reader).await.unwrap().unwrap();
        assert_eq!(received.id, 1);
        assert!(received.result.is_ok());

        let err = DaemonResponseEnvelope {
            id: 2,
            result: Err("boom".into()),
        };
        write_line(&mut client, &err).await.unwrap();
        let received: DaemonResponseEnvelope = read_line(&mut reader).await.unwrap().unwrap();
        assert_eq!(received.result.unwrap_err(), "boom");
    }

    #[tokio::test]
    async fn read_line_returns_none_on_clean_eof() {
        let (client, mut server) = duplex(64);
        drop(client);
        let mut reader = BufReader::new(&mut server);
        let received: Option<DaemonEnvelope> = read_line(&mut reader).await.unwrap();
        assert!(received.is_none());
    }
}

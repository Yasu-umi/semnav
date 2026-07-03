//! LSP Content-Length framing over an async byte stream.
//!
//! Each message is encoded as ASCII headers terminated by a blank line,
//! followed by a UTF-8 JSON body of the declared byte length:
//!
//! ```text
//! Content-Length: <n>\r\n
//! \r\n
//! <n bytes of JSON>
//! ```
//!
//! Other headers (`Content-Type`, etc.) are parsed and ignored.
//!
//! The reader side operates on `AsyncBufRead`: a `BufReader` owns the read
//! buffer so that bytes prefetched while scanning headers (which may include
//! part of the body or the next message) are preserved across calls.

use std::io;

use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Upper bound on a single frame's declared `Content-Length`, guarding against
/// a malformed/hostile header driving an unbounded allocation. Generous for any
/// real `documentSymbol`/hover payload.
const MAX_FRAME_SIZE: usize = 64 * 1024 * 1024;

/// Serialize `msg` and write it with Content-Length framing, then flush.
pub async fn write_message<W, M>(w: &mut W, msg: &M) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    M: Serialize,
{
    let body = serde_json::to_vec(msg).map_err(io::Error::other)?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    w.write_all(header.as_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

/// Read one framed message. Returns `Ok(None)` at clean EOF (no header bytes),
/// `Ok(Some(value))` for a complete message, or `Err` on a malformed/truncated
/// frame. Requires a buffered reader so prefetched bytes survive between calls.
pub async fn read_message<R>(r: &mut R) -> io::Result<Option<Value>>
where
    R: AsyncBufRead + Unpin,
{
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = Vec::new();
        let n = r.read_until(b'\n', &mut line).await?;
        if n == 0 {
            // EOF before any header byte → clean end of stream between messages.
            return Ok(None);
        }
        let trimmed = line.strip_suffix(b"\r\n").unwrap_or(&line);
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix(b"Content-Length:") {
            let s = std::str::from_utf8(rest).unwrap_or("").trim();
            if let Ok(len) = s.parse::<usize>() {
                content_length = Some(len);
            }
        }
        // Other headers are ignored.
    }

    let len = content_length.ok_or_else(|| io::Error::other("missing Content-Length header"))?;
    if len > MAX_FRAME_SIZE {
        return Err(io::Error::other(format!("frame too large: {len} bytes")));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    let value = serde_json::from_slice(&buf).map_err(io::Error::other)?;
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::proto::{JsonRpcVersion, Message};
    use tokio::io::{BufReader, duplex};

    async fn roundtrip(msg: Message) -> Message {
        let (mut a, b) = duplex(8192);
        let writer = tokio::spawn(async move { write_message(&mut a, &msg).await });
        let read = tokio::spawn(async move {
            let mut br = BufReader::new(b);
            read_message::<_>(&mut br).await
        });
        writer.await.unwrap().unwrap();
        let value = read.await.unwrap().unwrap().unwrap();
        Message::from_value(value).expect("decoded to Message")
    }

    #[tokio::test]
    async fn request_roundtrips() {
        let msg = Message::Request {
            jsonrpc: JsonRpcVersion,
            id: 7,
            method: "ping".to_string(),
            params: Some(serde_json::json!({"q": "something"})),
        };
        assert_eq!(roundtrip(msg.clone()).await, msg);
    }

    #[tokio::test]
    async fn notification_roundtrips_without_id() {
        let msg = Message::Notification {
            jsonrpc: JsonRpcVersion,
            method: "initialized".to_string(),
            params: None,
        };
        assert_eq!(roundtrip(msg.clone()).await, msg);
    }

    #[tokio::test]
    async fn response_and_error_distinguish() {
        let ok = Message::Response {
            jsonrpc: JsonRpcVersion,
            id: 1,
            result: serde_json::json!([{"name": "f"}]),
        };
        assert_eq!(roundtrip(ok.clone()).await, ok);

        let err = Message::Error {
            jsonrpc: JsonRpcVersion,
            id: 2,
            error: crate::lsp::proto::RpcError {
                code: -32601,
                message: "Unhandled method".to_string(),
                data: None,
            },
        };
        assert_eq!(roundtrip(err.clone()).await, err);
    }

    #[tokio::test]
    async fn two_messages_back_to_back() {
        let (mut a, b) = duplex(8192);
        let mut br = BufReader::new(b);
        let msgs = [
            Message::Request {
                jsonrpc: JsonRpcVersion,
                id: 1,
                method: "one".to_string(),
                params: None,
            },
            Message::Request {
                jsonrpc: JsonRpcVersion,
                id: 2,
                method: "two".to_string(),
                params: None,
            },
        ];
        for m in &msgs {
            write_message(&mut a, m).await.unwrap();
        }
        for m in &msgs {
            let v = read_message::<_>(&mut br).await.unwrap().unwrap();
            assert_eq!(Message::from_value(v).as_ref(), Some(m));
        }
    }

    #[tokio::test]
    async fn clean_eof_returns_none() {
        let (_a, b) = duplex(8192);
        drop(_a);
        let mut br = BufReader::new(b);
        let got = read_message::<_>(&mut br).await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn oversized_content_length_is_rejected_without_allocating() {
        let (mut a, b) = duplex(8192);
        let header = format!("Content-Length: {}\r\n\r\n", MAX_FRAME_SIZE + 1);
        let writer = tokio::spawn(async move { a.write_all(header.as_bytes()).await });
        let mut br = BufReader::new(b);
        let err = read_message::<_>(&mut br).await.unwrap_err();
        assert!(err.to_string().contains("frame too large"));
        writer.await.unwrap().unwrap();
    }
}

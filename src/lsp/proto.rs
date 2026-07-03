//! JSON-RPC 2.0 message envelopes for LSP.
//!
//! Every message on the wire is wrapped in `{"jsonrpc":"2.0", ...}`. The
//! `jsonrpc` field is pinned to `"2.0"` at serialization and accepted (but not
//! strictly validated) on read — it doubles as a presence marker that lets
//! `#[serde(untagged)]` distinguish the four variants.

use serde::de::IgnoredAny;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// JSON-RPC id (LSP servers accept integers).
pub type Id = i64;

/// Marker for the `jsonrpc: "2.0"` field. Serializes as `"2.0"`; deserializes
/// by consuming (and discarding) any value, so a missing/wrong version simply
/// fails variant matching rather than hard-erroring the whole stream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JsonRpcVersion;

impl Serialize for JsonRpcVersion {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("2.0")
    }
}

impl<'de> Deserialize<'de> for JsonRpcVersion {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let _ = IgnoredAny::deserialize(d)?;
        Ok(JsonRpcVersion)
    }
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// An outbound or inbound JSON-RPC 2.0 message.
///
/// Variant order matters for `untagged`: `Request` (id + method), `Response`
/// (id + result), `Error` (id + error), then `Notification` (method, no id).
/// Required (non-`Option`) fields drive the match, so a notification never
/// matches the id-bearing variants and vice-versa.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Message {
    Request {
        #[serde(default)]
        jsonrpc: JsonRpcVersion,
        id: Id,
        method: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        params: Option<Value>,
    },
    Response {
        #[serde(default)]
        jsonrpc: JsonRpcVersion,
        id: Id,
        result: Value,
    },
    Error {
        #[serde(default)]
        jsonrpc: JsonRpcVersion,
        id: Id,
        error: RpcError,
    },
    Notification {
        #[serde(default)]
        jsonrpc: JsonRpcVersion,
        method: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        params: Option<Value>,
    },
}

impl Message {
    /// Try to interpret a raw JSON value as a message. Returns `None` if the
    /// shape matches none of the four variants.
    pub fn from_value(value: Value) -> Option<Self> {
        serde_json::from_value(value).ok()
    }
}

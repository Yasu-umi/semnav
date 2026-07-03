//! LSP query-time abstraction: definition / references / call hierarchy /
//! hover, in the RPITIT style of `SymbolFetcher` (native async-in-trait, no
//! `async-trait` crate, not `dyn`-safe). Operations are generic over
//! `impl LspQueryClient`, so a real [`ClientLspQueryClient`] over a live
//! [`LspClient`] and a [`MockLspQueryClient`] share one code path
//! (`docs/design/lsp-integration.md`).
//!
//! Parse helpers normalize the two server dialects:
//! - definition returns `Location[]` (pyright) **or** `LocationLink[]`
//!   (tsserver) — collapsed to `Location` here;
//! - call hierarchy items round-trip their raw JSON in `incomingCalls` /
//!   `outgoingCalls` (the server's `data` token must be passed back verbatim);
//! - hover's `contents` (`MarkupContent` / `MarkedString` / `MarkedString[]`)
//!   is flattened to a single text blob.

#[cfg(test)]
use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::timeout;

use crate::indexer::LspRange;
use crate::lsp::{LspClient, QUERY_TIMEOUT};

/// A normalized LSP `Location` (`uri` + `range`).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Location {
    pub uri: String,
    pub range: LspRange,
}

/// Flattened hover contents — the combined markup text, regardless of which
/// `contents` shape the server used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hover {
    pub value: String,
}

/// A `CallHierarchyItem`. `raw` preserves the server's full JSON (including the
/// opaque `data` token) so it can be echoed back in `incomingCalls`/
/// `outgoingCalls`.
#[derive(Debug, Clone)]
pub struct CallHierarchyItem {
    pub name: String,
    pub kind: u32,
    pub uri: String,
    pub range: LspRange,
    pub selection_range: LspRange,
    pub raw: Value,
}

impl CallHierarchyItem {
    /// Parse an item from its raw JSON, keeping the original for round-tripping.
    fn from_value(v: Value) -> Result<Self> {
        let raw: RawItem = serde_json::from_value(v.clone())?;
        Ok(Self {
            name: raw.name,
            kind: raw.kind,
            uri: raw.uri,
            range: raw.range,
            selection_range: raw.selection_range,
            raw: v,
        })
    }

    /// Stable lookup key for canned call-hierarchy responses in tests.
    pub fn key(&self) -> ItemKey {
        (
            self.uri.clone(),
            self.selection_range.start.line,
            self.selection_range.start.character,
        )
    }
}

#[derive(Deserialize)]
struct RawItem {
    name: String,
    kind: u32,
    uri: String,
    range: LspRange,
    #[serde(rename = "selectionRange")]
    selection_range: LspRange,
}

/// An incoming call edge: `from` calls the prepared item, at `from_ranges`.
#[derive(Debug, Clone)]
pub struct IncomingCall {
    pub from: CallHierarchyItem,
    pub from_ranges: Vec<LspRange>,
}

/// An outgoing call edge: the prepared item calls `to`, at `from_ranges`.
#[derive(Debug, Clone)]
pub struct OutgoingCall {
    pub to: CallHierarchyItem,
    pub from_ranges: Vec<LspRange>,
}

/// The query-time LSP surface. Methods mirror the LSP requests the five find_*
/// tools need (`docs/design/lsp-integration.md`). `open_document` must precede
/// any positional request on a fresh server (query uses a separate server from
/// the indexer, so documents are not yet open).
pub trait LspQueryClient {
    /// `textDocument/didOpen` for `uri`. Best-effort: callers ignore failures
    /// for unreadable (external / deleted) files.
    fn open_document(&self, uri: &str, text: &str) -> impl Future<Output = Result<()>> + Send;
    /// `textDocument/definition` collapsed to `Location[]`.
    fn definition(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> impl Future<Output = Result<Vec<Location>>> + Send;
    /// `textDocument/references` (`includeDeclaration` passed through).
    fn references(
        &self,
        uri: &str,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> impl Future<Output = Result<Vec<Location>>> + Send;
    /// `textDocument/prepareCallHierarchy`.
    fn prepare_call_hierarchy(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> impl Future<Output = Result<Vec<CallHierarchyItem>>> + Send;
    /// `callHierarchy/incomingCalls` for a prepared item.
    fn incoming_calls(
        &self,
        item: &CallHierarchyItem,
    ) -> impl Future<Output = Result<Vec<IncomingCall>>> + Send;
    /// `callHierarchy/outgoingCalls` for a prepared item.
    fn outgoing_calls(
        &self,
        item: &CallHierarchyItem,
    ) -> impl Future<Output = Result<Vec<OutgoingCall>>> + Send;
    /// `textDocument/hover` (flattened text), or `None` if the server has none.
    fn hover(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> impl Future<Output = Result<Option<Hover>>> + Send;
}

/// Real client over a live [`LspClient`], enforcing [`QUERY_TIMEOUT`] per
/// round-trip. `language_id` is the didOpen language id; one client serves one
/// language's server (mirrors `LspSymbolFetcher`).
pub struct ClientLspQueryClient<'a> {
    client: &'a LspClient,
    timeout: Duration,
    language_id: &'a str,
}

impl<'a> ClientLspQueryClient<'a> {
    pub fn new(client: &'a LspClient, timeout: Duration, language_id: &'a str) -> Self {
        Self {
            client,
            timeout,
            language_id,
        }
    }
}

impl LspQueryClient for ClientLspQueryClient<'_> {
    fn open_document(&self, uri: &str, text: &str) -> impl Future<Output = Result<()>> + Send {
        let client = self.client.clone();
        let language_id = self.language_id.to_string();
        let uri = uri.to_string();
        let text = text.to_string();
        async move {
            client
                .ensure_document(&uri, &language_id, &text)
                .await
                .map_err(|e| anyhow!("didOpen/didChange failed: {e}"))
        }
    }

    fn definition(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> impl Future<Output = Result<Vec<Location>>> + Send {
        let client = self.client.clone();
        let to = self.timeout;
        let uri = uri.to_string();
        async move {
            let raw = timed(
                to,
                client.request(
                    "textDocument/definition",
                    Some(pos_params(&uri, line, character)),
                ),
                "definition",
            )
            .await?;
            parse_locations(raw)
        }
    }

    fn references(
        &self,
        uri: &str,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> impl Future<Output = Result<Vec<Location>>> + Send {
        let client = self.client.clone();
        let to = self.timeout;
        let uri = uri.to_string();
        async move {
            let params = json!({
                "textDocument": {"uri": uri},
                "position": {"line": line, "character": character},
                "context": {"includeDeclaration": include_declaration},
            });
            let raw = timed(
                to,
                client.request("textDocument/references", Some(params)),
                "references",
            )
            .await?;
            if raw.is_null() {
                return Ok(Vec::new());
            }
            let locs: Vec<Location> = serde_json::from_value(raw)?;
            Ok(locs)
        }
    }

    fn prepare_call_hierarchy(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> impl Future<Output = Result<Vec<CallHierarchyItem>>> + Send {
        let client = self.client.clone();
        let to = self.timeout;
        let uri = uri.to_string();
        async move {
            let raw = timed(
                to,
                client.request(
                    "textDocument/prepareCallHierarchy",
                    Some(pos_params(&uri, line, character)),
                ),
                "prepareCallHierarchy",
            )
            .await?;
            parse_items(raw)
        }
    }

    fn incoming_calls(
        &self,
        item: &CallHierarchyItem,
    ) -> impl Future<Output = Result<Vec<IncomingCall>>> + Send {
        let client = self.client.clone();
        let to = self.timeout;
        let raw = item.raw.clone();
        async move {
            let params = json!({ "item": raw });
            let res = timed(
                to,
                client.request("callHierarchy/incomingCalls", Some(params)),
                "incomingCalls",
            )
            .await?;
            parse_incoming(res)
        }
    }

    fn outgoing_calls(
        &self,
        item: &CallHierarchyItem,
    ) -> impl Future<Output = Result<Vec<OutgoingCall>>> + Send {
        let client = self.client.clone();
        let to = self.timeout;
        let raw = item.raw.clone();
        async move {
            let params = json!({ "item": raw });
            let res = timed(
                to,
                client.request("callHierarchy/outgoingCalls", Some(params)),
                "outgoingCalls",
            )
            .await?;
            parse_outgoing(res)
        }
    }

    fn hover(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> impl Future<Output = Result<Option<Hover>>> + Send {
        let client = self.client.clone();
        let to = self.timeout;
        let uri = uri.to_string();
        async move {
            let raw = timed(
                to,
                client.request(
                    "textDocument/hover",
                    Some(pos_params(&uri, line, character)),
                ),
                "hover",
            )
            .await?;
            parse_hover(raw)
        }
    }
}

/// Default query-time client bound to [`QUERY_TIMEOUT`].
impl<'a> ClientLspQueryClient<'a> {
    pub fn with_default_timeout(client: &'a LspClient, language_id: &'a str) -> Self {
        Self::new(client, QUERY_TIMEOUT, language_id)
    }
}

/// `textDocument` + `position` request params.
fn pos_params(uri: &str, line: u32, character: u32) -> Value {
    json!({
        "textDocument": {"uri": uri},
        "position": {"line": line, "character": character},
    })
}

/// Run an LSP request with a deadline, mapping a timeout to an error string the
/// supervisor classifies as [`FailureKind::Timeout`] (see `lsp/supervisor.rs`).
async fn timed<F>(deadline: Duration, fut: F, op: &str) -> Result<Value>
where
    F: std::future::Future<Output = Result<Value>>,
{
    timeout(deadline, fut)
        .await
        .map_err(|_| anyhow!("{op} timed out after {deadline:?}"))?
}

#[derive(Deserialize)]
struct LocationLink {
    #[serde(rename = "targetUri")]
    target_uri: String,
    #[serde(rename = "targetRange")]
    target_range: LspRange,
    #[serde(rename = "targetSelectionRange", default)]
    target_selection_range: Option<LspRange>,
}

/// Collapse a `Location[]` or `LocationLink[]` definition result to `Location`.
/// tsserver returns `LocationLink[]`; pyright returns `Location[]`. A
/// `LocationLink`'s target identifier span is `targetSelectionRange` (falling
/// back to `targetRange` if the server omits it).
fn parse_locations(raw: Value) -> Result<Vec<Location>> {
    if raw.is_null() {
        return Ok(Vec::new());
    }
    let is_link = raw
        .as_array()
        .and_then(|a| a.first())
        .map(|f| f.get("targetUri").is_some())
        .unwrap_or(false);
    if is_link {
        let links: Vec<LocationLink> = serde_json::from_value(raw)?;
        Ok(links
            .into_iter()
            .map(|l| Location {
                uri: l.target_uri,
                range: l.target_selection_range.unwrap_or(l.target_range),
            })
            .collect())
    } else {
        let locs: Vec<Location> = serde_json::from_value(raw)?;
        Ok(locs)
    }
}

/// Flatten hover `contents` (`string` | `MarkupContent` | `MarkedString[]`) to a
/// single text blob.
fn markup_text(c: Value) -> String {
    match c {
        Value::String(s) => s,
        Value::Array(a) => a
            .into_iter()
            .map(markup_text)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        Value::Object(o) => o
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

fn parse_hover(raw: Value) -> Result<Option<Hover>> {
    if raw.is_null() {
        return Ok(None);
    }
    let contents = raw.get("contents").cloned().unwrap_or(Value::Null);
    Ok(Some(Hover {
        value: markup_text(contents),
    }))
}

fn parse_items(raw: Value) -> Result<Vec<CallHierarchyItem>> {
    if raw.is_null() {
        return Ok(Vec::new());
    }
    let arr = raw
        .as_array()
        .ok_or_else(|| anyhow!("prepareCallHierarchy: expected array"))?;
    arr.iter()
        .map(|v| CallHierarchyItem::from_value(v.clone()))
        .collect()
}

#[derive(Deserialize)]
struct RawIncoming {
    from: Value,
    #[serde(rename = "fromRanges", default)]
    from_ranges: Vec<LspRange>,
}

fn parse_incoming(raw: Value) -> Result<Vec<IncomingCall>> {
    if raw.is_null() {
        return Ok(Vec::new());
    }
    let rows: Vec<RawIncoming> = serde_json::from_value(raw)?;
    rows.into_iter()
        .map(|r| {
            Ok(IncomingCall {
                from: CallHierarchyItem::from_value(r.from)?,
                from_ranges: r.from_ranges,
            })
        })
        .collect()
}

#[derive(Deserialize)]
struct RawOutgoing {
    to: Value,
    #[serde(rename = "fromRanges", default)]
    from_ranges: Vec<LspRange>,
}

fn parse_outgoing(raw: Value) -> Result<Vec<OutgoingCall>> {
    if raw.is_null() {
        return Ok(Vec::new());
    }
    let rows: Vec<RawOutgoing> = serde_json::from_value(raw)?;
    rows.into_iter()
        .map(|r| {
            Ok(OutgoingCall {
                to: CallHierarchyItem::from_value(r.to)?,
                from_ranges: r.from_ranges,
            })
        })
        .collect()
}

/// (uri, line, character) — the canned-response key for positional queries.
#[cfg(test)]
type PosKey = (String, u32, u32);
/// (uri, line, character) of a call-hierarchy item's selection range.
type ItemKey = (String, u32, u32);

/// In-memory client for operation unit tests. Each method looks up its canned
/// response by position (or by item key for call hierarchy) and returns an
/// empty/`None` result when none is programmed.
#[cfg(test)]
#[derive(Default)]
pub struct MockLspQueryClient {
    pub definitions: HashMap<PosKey, Vec<Location>>,
    pub references: HashMap<PosKey, Vec<Location>>,
    pub prepare: HashMap<PosKey, Vec<CallHierarchyItem>>,
    pub incoming: HashMap<ItemKey, Vec<IncomingCall>>,
    pub outgoing: HashMap<ItemKey, Vec<OutgoingCall>>,
    pub hovers: HashMap<PosKey, Option<Hover>>,
}

#[cfg(test)]
impl MockLspQueryClient {
    pub fn new() -> Self {
        Self::default()
    }
}

// Mirrors the crate-wide RPITIT style (`-> impl Future + Send`, not `async fn`;
// see `lsp::supervisor::MetaStore`), so the trivial mock bodies stay explicit.
#[cfg(test)]
#[allow(clippy::manual_async_fn)]
impl LspQueryClient for MockLspQueryClient {
    fn open_document(&self, _uri: &str, _text: &str) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }

    fn definition(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> impl Future<Output = Result<Vec<Location>>> + Send {
        let out = self
            .definitions
            .get(&(uri.to_string(), line, character))
            .cloned()
            .unwrap_or_default();
        async move { Ok(out) }
    }

    fn references(
        &self,
        uri: &str,
        line: u32,
        character: u32,
        _include_declaration: bool,
    ) -> impl Future<Output = Result<Vec<Location>>> + Send {
        let out = self
            .references
            .get(&(uri.to_string(), line, character))
            .cloned()
            .unwrap_or_default();
        async move { Ok(out) }
    }

    fn prepare_call_hierarchy(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> impl Future<Output = Result<Vec<CallHierarchyItem>>> + Send {
        let out = self
            .prepare
            .get(&(uri.to_string(), line, character))
            .cloned()
            .unwrap_or_default();
        async move { Ok(out) }
    }

    fn incoming_calls(
        &self,
        item: &CallHierarchyItem,
    ) -> impl Future<Output = Result<Vec<IncomingCall>>> + Send {
        let out = self.incoming.get(&item.key()).cloned().unwrap_or_default();
        async move { Ok(out) }
    }

    fn outgoing_calls(
        &self,
        item: &CallHierarchyItem,
    ) -> impl Future<Output = Result<Vec<OutgoingCall>>> + Send {
        let out = self.outgoing.get(&item.key()).cloned().unwrap_or_default();
        async move { Ok(out) }
    }

    fn hover(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> impl Future<Output = Result<Option<Hover>>> + Send {
        let out = self
            .hovers
            .get(&(uri.to_string(), line, character))
            .cloned()
            .unwrap_or(None);
        async move { Ok(out) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::LspPosition;

    fn rng(sl: u32, sc: u32, el: u32, ec: u32) -> LspRange {
        LspRange {
            start: LspPosition {
                line: sl,
                character: sc,
            },
            end: LspPosition {
                line: el,
                character: ec,
            },
        }
    }

    #[test]
    fn parse_locations_handles_plain_locations() {
        let raw = serde_json::json!([
            {"uri": "file:///a.py", "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 4}}},
        ]);
        let locs = parse_locations(raw).unwrap();
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].uri, "file:///a.py");
        assert_eq!(locs[0].range.start.line, 1);
    }

    #[test]
    fn parse_locations_collapses_location_links() {
        let raw = serde_json::json!([
            {
                "targetUri": "file:///b.ts",
                "targetRange": {"start": {"line": 0, "character": 0}, "end": {"line": 5, "character": 0}},
                "targetSelectionRange": {"start": {"line": 0, "character": 6}, "end": {"line": 0, "character": 10}}
            }
        ]);
        let locs = parse_locations(raw).unwrap();
        assert_eq!(locs[0].uri, "file:///b.ts");
        // Selection range wins over the broader target range.
        assert_eq!(locs[0].range.start.character, 6);
    }

    #[test]
    fn parse_locations_null_is_empty() {
        assert!(parse_locations(Value::Null).unwrap().is_empty());
    }

    #[test]
    fn parse_hover_flattens_markup_and_string_contents() {
        let markup = serde_json::json!({"contents": {"kind": "markdown", "value": "doc body"}});
        assert_eq!(parse_hover(markup).unwrap().unwrap().value, "doc body");

        let s = serde_json::json!({"contents": "plain string doc"});
        assert_eq!(parse_hover(s).unwrap().unwrap().value, "plain string doc");

        let arr = serde_json::json!({"contents": [{"language": "python", "value": "sig"}, "note"]});
        let h = parse_hover(arr).unwrap().unwrap();
        assert!(h.value.contains("sig") && h.value.contains("note"));
    }

    #[test]
    fn parse_hover_null_is_none() {
        assert!(parse_hover(Value::Null).unwrap().is_none());
    }

    #[test]
    fn parse_items_round_trips_data_via_raw() {
        let raw = serde_json::json!([{
            "name": "foo", "kind": 12,
            "uri": "file:///a.py",
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}},
            "selectionRange": {"start": {"line": 0, "character": 4}, "end": {"line": 0, "character": 7}},
            "data": {"token": 42}
        }]);
        let items = parse_items(raw).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "foo");
        // The opaque `data` survives on `raw` for echoing back.
        assert_eq!(items[0].raw["data"]["token"], 42);
    }

    #[test]
    fn parse_incoming_attaches_from_ranges() {
        let raw = serde_json::json!([{
            "from": {
                "name": "caller", "kind": 12,
                "uri": "file:///c.py",
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}},
                "selectionRange": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 6}}
            },
            "fromRanges": [{"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 6}}]
        }]);
        let inc = parse_incoming(raw).unwrap();
        assert_eq!(inc[0].from.name, "caller");
        assert_eq!(inc[0].from_ranges.len(), 1);
        assert_eq!(inc[0].from_ranges[0].start.line, 2);
    }

    #[test]
    fn item_key_uses_selection_range_start() {
        let item = CallHierarchyItem {
            name: "x".into(),
            kind: 12,
            uri: "file:///a.py".into(),
            range: rng(0, 0, 5, 0),
            selection_range: rng(0, 4, 0, 5),
            raw: Value::Null,
        };
        assert_eq!(item.key(), ("file:///a.py".to_string(), 0, 4));
    }

    #[tokio::test]
    async fn mock_returns_canned_definition_or_empty() {
        let mut mock = MockLspQueryClient::new();
        mock.definitions.insert(
            ("file:///a.py".to_string(), 1, 2),
            vec![Location {
                uri: "file:///b.py".to_string(),
                range: rng(0, 0, 0, 3),
            }],
        );
        let hit = mock.definition("file:///a.py", 1, 2).await.unwrap();
        assert_eq!(hit.len(), 1);
        let miss = mock.definition("file:///a.py", 9, 9).await.unwrap();
        assert!(miss.is_empty());
    }
}

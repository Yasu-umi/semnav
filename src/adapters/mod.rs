//! Language adapters — `LanguageAdapter` trait, pyright/tsserver implementations,
//! provisioning (isolated npm install), `map_symbol_kind` / `NodeKind`, and
//! hover-based refine (`construct` extraction).
//!
//! See `docs/design/language-adapters.md`.

mod kind;
mod provision;
mod python;
mod rust;
mod typescript;

pub use kind::{NodeKind, SymbolKind};
pub use provision::{ProvisionContext, provision};
pub use python::PythonAdapter;
pub use rust::RustAdapter;
pub use typescript::TypeScriptAdapter;

/// How to launch the language server: the binary name plus its fixed args
/// (e.g. `pyright-langserver --stdio`). `&'static` because adapters are unit
/// structs with no runtime state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    /// Bare binary name resolvable on `PATH`, or an absolute path after provision.
    pub program: &'static str,
    /// Fixed trailing args (e.g. `["--stdio"]`).
    pub args: &'static [&'static str],
}

/// How to (re)install the language server via npm when it is not on `PATH`
/// (`docs/design/language-adapters.md` "LSP Server Provisioning").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerPackage {
    /// npm package name providing [`CommandSpec::program`] (e.g. `"pyright"`).
    pub npm_package: &'static str,
    /// Pinned version (`docs/design/lsp-integration.md` verification env).
    pub version: &'static str,
    /// Peer packages to install alongside, `name@version` form
    /// (e.g. `["typescript@6.0.3"]` for tsserver; empty for pyright).
    pub peers: &'static [&'static str],
}

/// Per-language differences extracted behind a small adapter.
///
/// The indexer path uses only classification + URI/extension matching +
/// `is_external`; the lifecycle path additionally uses `server_command` +
/// `server_package` to provision and spawn the real LSP process
/// (`docs/design/language-adapters.md`).
///
/// `Send + Sync` so `dyn LanguageAdapter` (as returned by
/// [`adapter_for_language`]) can be held across an await in the spawned LSP
/// supervisor task. Built-in adapters are unit structs, so the bound is free.
pub trait LanguageAdapter: Send + Sync {
    /// Lowercase language tag stored on `nodes.language` (e.g. `"python"`).
    fn language_name(&self) -> &'static str;

    /// File extensions this server indexes, each including the leading dot
    /// (e.g. `".py"`), lowercase.
    fn file_extensions(&self) -> &'static [&'static str];

    /// Path substrings marking dependencies/stdlib treated as external
    /// (`docs/design/graph-model.md` `is_external`). Each is a path fragment
    /// with surrounding separators (e.g. `"/node_modules/"`).
    fn external_path_markers(&self) -> &'static [&'static str];

    /// How to launch this server once provisioned
    /// (`docs/design/language-adapters.md`). pyright → `pyright-langserver
    /// --stdio`; tsserver → `typescript-language-server --stdio`.
    fn server_command(&self) -> CommandSpec;

    /// npm package metadata for isolated install when the server is not on
    /// `PATH` (`docs/design/language-adapters.md`). `None` when this
    /// language's server isn't distributed via npm (e.g. rust-analyzer via
    /// `rustup`) — provisioning then requires the server already be on
    /// `PATH`, failing with a clear error otherwise.
    fn server_package(&self) -> Option<ServerPackage>;

    /// Map an LSP `SymbolKind` number to a [`NodeKind`]. The default pass-through
    /// works for both pyright and tsserver (both emit standard values); the TS
    /// `type`-alias trap is refined later via hover `construct`, not here.
    fn map_symbol_kind(&self, lsp: u32) -> NodeKind {
        match SymbolKind::from_u32(lsp) {
            Some(kind) => NodeKind::Standard(kind),
            None => NodeKind::Custom(format!("Unknown({lsp})")),
        }
    }

    /// Whether `uri` is a source file this adapter owns (case-insensitive ext).
    fn matches_uri(&self, uri: &str) -> bool {
        let Some((_, raw)) = uri.rsplit_once('.') else {
            return false;
        };
        let ext = format!(".{}", raw.to_ascii_lowercase());
        self.file_extensions()
            .iter()
            .any(|e| e.to_ascii_lowercase() == ext)
    }

    /// `is_external` detection: not under `root_uri`, or inside a known
    /// dependency path marker (`docs/design/graph-model.md`).
    fn is_external(&self, uri: &str, root_uri: &str) -> bool {
        if !uri.starts_with(root_uri) {
            return true;
        }
        self.external_path_markers()
            .iter()
            .any(|marker| uri.contains(marker))
    }
}

/// The built-in adapters shipped with 0.0.1 (Python, TypeScript, Rust).
/// References are `'static` (unit-struct ZST promotion), so the registry is
/// cheap to build.
pub fn builtin_adapters() -> Vec<&'static dyn LanguageAdapter> {
    vec![&PythonAdapter, &TypeScriptAdapter, &RustAdapter]
}

/// Pick the built-in adapter that owns `uri`, if any.
pub fn select_for_uri(uri: &str) -> Option<&'static dyn LanguageAdapter> {
    builtin_adapters()
        .into_iter()
        .find(|adapter| adapter.matches_uri(uri))
}

/// Pick the built-in adapter for a `language_name()` tag (e.g. `"python"`).
/// Used by the lifecycle runner to provision + index one language at a time.
pub fn adapter_for_language(language: &str) -> Option<&'static dyn LanguageAdapter> {
    builtin_adapters()
        .into_iter()
        .find(|adapter| adapter.language_name() == language)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_symbol_kind_passes_through_standard() {
        let a = PythonAdapter;
        assert_eq!(
            a.map_symbol_kind(12),
            NodeKind::Standard(SymbolKind::Function)
        );
        assert_eq!(a.map_symbol_kind(5), NodeKind::Standard(SymbolKind::Class));
    }

    #[test]
    fn map_symbol_kind_custom_for_unknown() {
        let a = TypeScriptAdapter;
        assert_eq!(
            a.map_symbol_kind(99),
            NodeKind::Custom("Unknown(99)".to_string())
        );
    }

    #[test]
    fn builtin_adapters_cover_python_typescript_and_rust() {
        let adapters = builtin_adapters();
        assert_eq!(adapters.len(), 3);
        assert_eq!(adapters[0].language_name(), "python");
        assert_eq!(adapters[1].language_name(), "typescript");
        assert_eq!(adapters[2].language_name(), "rust");
    }

    #[test]
    fn select_for_uri_routes_by_extension() {
        assert_eq!(
            select_for_uri("file:///app/mod.py").map(|a| a.language_name()),
            Some("python")
        );
        assert_eq!(
            select_for_uri("file:///app/mod.tsx").map(|a| a.language_name()),
            Some("typescript")
        );
        assert_eq!(
            select_for_uri("file:///app/main.rs").map(|a| a.language_name()),
            Some("rust")
        );
        assert!(select_for_uri("file:///app/Cargo.toml").is_none());
    }

    #[test]
    fn server_command_specs_match_design() {
        assert_eq!(
            PythonAdapter.server_command(),
            CommandSpec {
                program: "pyright-langserver",
                args: &["--stdio"],
            }
        );
        assert_eq!(
            TypeScriptAdapter.server_command(),
            CommandSpec {
                program: "typescript-language-server",
                args: &["--stdio"],
            }
        );
        assert_eq!(
            RustAdapter.server_command(),
            CommandSpec {
                program: "rust-analyzer",
                args: &[],
            }
        );
    }

    #[test]
    fn server_package_pins_versions() {
        assert_eq!(
            PythonAdapter.server_package(),
            Some(ServerPackage {
                npm_package: "pyright",
                version: "1.1.409",
                peers: &[],
            })
        );
        // tsserver needs the typescript peer that the language-server drives.
        assert_eq!(
            TypeScriptAdapter.server_package(),
            Some(ServerPackage {
                npm_package: "typescript-language-server",
                version: "5.1.3",
                peers: &["typescript@6.0.3"],
            })
        );
        // rust-analyzer isn't npm-distributed; must already be on PATH.
        assert_eq!(RustAdapter.server_package(), None);
    }

    #[test]
    fn adapter_for_language_round_trips() {
        assert_eq!(
            adapter_for_language("python").map(|a| a.language_name()),
            Some("python")
        );
        assert_eq!(
            adapter_for_language("typescript").map(|a| a.language_name()),
            Some("typescript")
        );
        assert_eq!(
            adapter_for_language("rust").map(|a| a.language_name()),
            Some("rust")
        );
        assert!(adapter_for_language("go").is_none());
    }
}

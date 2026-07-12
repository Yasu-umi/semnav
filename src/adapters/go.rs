//! Go adapter — gopls.

use crate::adapters::{CommandSpec, LanguageAdapter, ServerPackage};

/// Go via gopls. Emits standard `SymbolKind` values (struct→Struct,
/// interface→Interface, func→Function, method→Method), so no hover-based
/// refinement is needed. Unlike pyright/tsserver, gopls isn't distributed via
/// npm — installed through `go install golang.org/x/tools/gopls@latest`, so
/// it must already be on `PATH` (`server_package` returns `None`; see
/// `docs/design/language-adapters.md`). External = vendored deps and the
/// module cache (the Go stdlib itself resolves outside the workspace root,
/// so it's already excluded by the `is_external` "not under root" check).
///
/// Quirk: unlike Python/TS, a method's `documentSymbol` entry does **not**
/// nest under its receiver struct/interface — it comes back as a top-level
/// sibling named `"(*Type).Method"` (`docs/design/lsp-integration.md`).
pub struct GoAdapter;

impl LanguageAdapter for GoAdapter {
    fn language_name(&self) -> &'static str {
        "go"
    }

    fn file_extensions(&self) -> &'static [&'static str] {
        &[".go"]
    }

    fn external_path_markers(&self) -> &'static [&'static str] {
        &["/vendor/", "/pkg/mod/"]
    }

    fn server_command(&self) -> CommandSpec {
        CommandSpec {
            program: "gopls",
            args: &[],
        }
    }

    fn server_package(&self) -> Option<ServerPackage> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_go_uris_only() {
        let a = GoAdapter;
        assert!(a.matches_uri("file:///app/main.go"));
        assert!(a.matches_uri("file:///app/sub/Mod.GO"));
        assert!(!a.matches_uri("file:///app/mod.py"));
        assert!(!a.matches_uri("file:///app/go.mod"));
    }

    #[test]
    fn is_external_for_vendor_and_module_cache() {
        let a = GoAdapter;
        assert!(!a.is_external("file:///repo/pkg/greeter.go", "file:///repo/"));
        assert!(a.is_external("file:///repo/vendor/foo/foo.go", "file:///repo/"));
        assert!(a.is_external(
            "file:///home/u/go/pkg/mod/foo@v1.0.0/foo.go",
            "file:///repo/"
        ));
        // Not under the workspace root at all (e.g. GOROOT stdlib).
        assert!(a.is_external("file:///elsewhere/foo.go", "file:///repo/"));
    }

    #[test]
    fn server_package_is_none_not_npm_distributed() {
        assert_eq!(GoAdapter.server_package(), None);
    }
}

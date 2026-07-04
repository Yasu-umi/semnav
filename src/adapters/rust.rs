//! Rust adapter — rust-analyzer.

use crate::adapters::{CommandSpec, LanguageAdapter, ServerPackage};

/// Rust via rust-analyzer. Emits standard `SymbolKind` values. Unlike
/// pyright/tsserver, rust-analyzer isn't distributed via npm — installed
/// through `rustup component add rust-analyzer`, so it must already be on
/// `PATH` (`server_package` returns `None`; see `docs/design/language-adapters.md`).
/// External = build output (`target/`) and the cargo registry/git dep caches.
pub struct RustAdapter;

impl LanguageAdapter for RustAdapter {
    fn language_name(&self) -> &'static str {
        "rust"
    }

    fn file_extensions(&self) -> &'static [&'static str] {
        &[".rs"]
    }

    fn external_path_markers(&self) -> &'static [&'static str] {
        &["/target/", "/.cargo/registry/", "/.cargo/git/"]
    }

    fn server_command(&self) -> CommandSpec {
        CommandSpec {
            program: "rust-analyzer",
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
    fn matches_rust_uris_only() {
        let a = RustAdapter;
        assert!(a.matches_uri("file:///app/main.rs"));
        assert!(a.matches_uri("file:///app/sub/Mod.RS"));
        assert!(!a.matches_uri("file:///app/mod.py"));
        assert!(!a.matches_uri("file:///app/Cargo.toml"));
    }

    #[test]
    fn is_external_for_target_and_registry() {
        let a = RustAdapter;
        assert!(!a.is_external("file:///repo/src/lib.rs", "file:///repo/"));
        assert!(a.is_external(
            "file:///repo/target/debug/build/foo/out.rs",
            "file:///repo/"
        ));
        assert!(a.is_external(
            "file:///home/u/.cargo/registry/src/foo-1.0.0/lib.rs",
            "file:///repo/"
        ));
        assert!(a.is_external(
            "file:///home/u/.cargo/git/checkouts/foo/lib.rs",
            "file:///repo/"
        ));
        // Not under the workspace root at all.
        assert!(a.is_external("file:///elsewhere/lib.rs", "file:///repo/"));
    }

    #[test]
    fn server_package_is_none_not_npm_distributed() {
        assert_eq!(RustAdapter.server_package(), None);
    }
}

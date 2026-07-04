//! TypeScript adapter — tsserver.

use crate::adapters::{CommandSpec, LanguageAdapter, ServerPackage};

/// TypeScript via tsserver. Overloads normalize to an arity-suffixed FQN
/// (`app.repo.load#1`); the `type`-alias `SymbolKind=13` trap is refined to
/// `Custom("TypeAlias")` later via hover `construct`. External = `node_modules`.
pub struct TypeScriptAdapter;

impl LanguageAdapter for TypeScriptAdapter {
    fn language_name(&self) -> &'static str {
        "typescript"
    }

    fn file_extensions(&self) -> &'static [&'static str] {
        &[".ts", ".tsx"]
    }

    fn external_path_markers(&self) -> &'static [&'static str] {
        &["/node_modules/"]
    }

    fn server_command(&self) -> CommandSpec {
        CommandSpec {
            program: "typescript-language-server",
            args: &["--stdio"],
        }
    }

    fn server_package(&self) -> Option<ServerPackage> {
        Some(ServerPackage {
            npm_package: "typescript-language-server",
            version: "5.1.3",
            peers: &["typescript@6.0.3"],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_typescript_uris_only() {
        let a = TypeScriptAdapter;
        assert!(a.matches_uri("file:///app/mod.ts"));
        assert!(a.matches_uri("file:///app/mod.tsx"));
        assert!(a.matches_uri("file:///app/Component.TSX"));
        assert!(!a.matches_uri("file:///app/mod.py"));
        // `.tsx` must not be mistaken for `.ts`.
        assert!(!a.matches_uri("file:///app/mod.ts.py"));
    }

    #[test]
    fn is_external_for_node_modules() {
        let a = TypeScriptAdapter;
        assert!(!a.is_external("file:///repo/src/a.ts", "file:///repo/"));
        assert!(a.is_external("file:///repo/node_modules/lib/index.ts", "file:///repo/"));
        assert!(a.is_external("file:///elsewhere/a.ts", "file:///repo/"));
    }
}

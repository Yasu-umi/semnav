//! Python adapter — pyright-langserver (or basedpyright).

use crate::adapters::{CommandSpec, LanguageAdapter, ServerPackage};

/// Python via pyright. Emits standard `SymbolKind` values; no overloads, so FQN
/// needs no arity suffix. External = site-packages / venv / typeshed.
pub struct PythonAdapter;

impl LanguageAdapter for PythonAdapter {
    fn language_name(&self) -> &'static str {
        "python"
    }

    fn file_extensions(&self) -> &'static [&'static str] {
        &[".py"]
    }

    fn external_path_markers(&self) -> &'static [&'static str] {
        &["/site-packages/", "/.venv/lib/", "/typeshed-fallback/"]
    }

    fn server_command(&self) -> CommandSpec {
        CommandSpec {
            program: "pyright-langserver",
            args: &["--stdio"],
        }
    }

    fn server_package(&self) -> ServerPackage {
        ServerPackage {
            npm_package: "pyright",
            version: "1.1.409",
            peers: &[],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_python_uris_only() {
        let a = PythonAdapter;
        assert!(a.matches_uri("file:///app/mod.py"));
        assert!(a.matches_uri("file:///app/sub/Class.PY"));
        assert!(!a.matches_uri("file:///app/mod.ts"));
        assert!(!a.matches_uri("file:///app/README"));
    }

    #[test]
    fn is_external_for_dependency_paths() {
        let a = PythonAdapter;
        assert!(!a.is_external("file:///repo/app/repo.py", "file:///repo/"));
        assert!(a.is_external(
            "file:///repo/.venv/lib/python3.11/site-packages/foo/__init__.py",
            "file:///repo/"
        ));
        assert!(a.is_external("file:///repo/site-packages/foo.py", "file:///repo/"));
        assert!(a.is_external(
            "file:///usr/lib/typeshed-fallback/stdlib/os.pyi",
            "file:///repo/"
        ));
        // Not under the workspace root at all.
        assert!(a.is_external("file:///elsewhere/mod.py", "file:///repo/"));
    }
}

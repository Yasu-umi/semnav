//! Custom/generic adapter — lets a user point semnav at an LSP server for a
//! language semnav has no built-in adapter for, purely via environment
//! variables (`docs/design/language-adapters.md` "Custom/Generic Adapter").
//! No npm auto-install, no per-language `construct`/interface-dispatch
//! special-casing — a "just make it run" escape hatch, not a plugin system.

use std::sync::OnceLock;

use crate::adapters::{CommandSpec, LanguageAdapter, ServerPackage};

/// A single user-configured language, entirely driven by
/// `SEMNAV_CUSTOM_LANGUAGES` + `SEMNAV_LSP_<TAG>_*` env vars read once at
/// process startup (see [`custom_adapters`]). Fields are `&'static` (leaked
/// once during parsing) so this fits the existing `&'static dyn
/// LanguageAdapter` registry (`docs/design/language-adapters.md`:
/// "Built-in adapters are unit structs, so the bound is free") without
/// changing the trait signature.
pub struct CustomAdapter {
    tag: &'static str,
    extensions: &'static [&'static str],
    external_markers: &'static [&'static str],
    /// Never resolved in practice: `SEMNAV_LSP_<TAG>_COMMAND` is required for
    /// a custom language to actually run, and its env-override lookup
    /// (`src/adapters/provision.rs::command_override_from_env`) short-circuits
    /// before this is ever read. Left unconfigured, it deliberately fails to
    /// resolve on `PATH`, falling into the existing `server_package() ==
    /// None` bail message — which names this exact env var.
    placeholder_program: &'static str,
}

impl LanguageAdapter for CustomAdapter {
    fn language_name(&self) -> &'static str {
        self.tag
    }

    fn file_extensions(&self) -> &'static [&'static str] {
        self.extensions
    }

    fn external_path_markers(&self) -> &'static [&'static str] {
        self.external_markers
    }

    fn server_command(&self) -> CommandSpec {
        CommandSpec {
            program: self.placeholder_program,
            args: &[],
        }
    }

    fn server_package(&self) -> Option<ServerPackage> {
        None
    }
}

/// Split a comma-separated env value into trimmed, non-empty entries.
fn split_env_list(value: Option<String>) -> Vec<String> {
    value
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Leak an owned `Vec<String>` into a `&'static [&'static str]`, once.
fn leak_str_list(items: Vec<String>) -> &'static [&'static str] {
    let leaked: Vec<&'static str> = items
        .into_iter()
        .map(|s| -> &'static str { Box::leak(s.into_boxed_str()) })
        .collect();
    Box::leak(leaked.into_boxed_slice())
}

/// Parse `SEMNAV_CUSTOM_LANGUAGES` (`languages_spec`) into `CustomAdapter`s,
/// resolving each tag's `SEMNAV_LSP_<TAG>_EXTENSIONS`/`_EXTERNAL_MARKERS` via
/// `lookup` (`std::env::var` in production; a fake in tests, so this stays
/// unit-testable without mutating global env state). A tag with no (or
/// empty) `EXTENSIONS` is dropped — an adapter matching zero files would only
/// ever produce a spurious failed `index_language` attempt.
fn parse_custom_adapters(
    languages_spec: &str,
    lookup: impl Fn(&str) -> Option<String>,
) -> Vec<CustomAdapter> {
    languages_spec
        .split(',')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .filter_map(|tag| {
            let tag_lower = tag.to_ascii_lowercase();
            let upper = tag_lower.to_ascii_uppercase();
            let extensions = split_env_list(lookup(&format!("SEMNAV_LSP_{upper}_EXTENSIONS")));
            if extensions.is_empty() {
                eprintln!(
                    "semnav: SEMNAV_CUSTOM_LANGUAGES lists {tag_lower:?} but \
                     SEMNAV_LSP_{upper}_EXTENSIONS is unset or empty; skipping"
                );
                return None;
            }
            let external_markers =
                split_env_list(lookup(&format!("SEMNAV_LSP_{upper}_EXTERNAL_MARKERS")));
            Some(CustomAdapter {
                tag: Box::leak(tag_lower.into_boxed_str()),
                extensions: leak_str_list(extensions),
                external_markers: leak_str_list(external_markers),
                placeholder_program: Box::leak(
                    format!("semnav-custom-{upper}-lsp-not-configured").into_boxed_str(),
                ),
            })
        })
        .collect()
}

static CUSTOM_ADAPTERS: OnceLock<Vec<CustomAdapter>> = OnceLock::new();

/// The user-configured custom languages, parsed once from `SEMNAV_CUSTOM_LANGUAGES`
/// and its per-tag env vars. Empty when that var is unset (the default), so
/// [`crate::adapters::builtin_adapters`] is unaffected unless a user opts in.
pub fn custom_adapters() -> &'static [CustomAdapter] {
    CUSTOM_ADAPTERS.get_or_init(|| {
        let spec = std::env::var("SEMNAV_CUSTOM_LANGUAGES").unwrap_or_default();
        parse_custom_adapters(&spec, |key| std::env::var(key).ok())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup_from<'a>(vars: &'a HashMap<&str, &str>) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| vars.get(key).map(|v| v.to_string())
    }

    #[test]
    fn empty_spec_yields_no_adapters() {
        let vars = HashMap::new();
        assert!(parse_custom_adapters("", lookup_from(&vars)).is_empty());
    }

    #[test]
    fn one_fully_configured_tag_yields_one_adapter() {
        let mut vars = HashMap::new();
        vars.insert("SEMNAV_LSP_JAVA_EXTENSIONS", ".java");
        vars.insert("SEMNAV_LSP_JAVA_EXTERNAL_MARKERS", "/target/, /.m2/");
        let adapters = parse_custom_adapters("java", lookup_from(&vars));
        assert_eq!(adapters.len(), 1);
        let a = &adapters[0];
        assert_eq!(a.language_name(), "java");
        assert_eq!(a.file_extensions(), &[".java"]);
        assert_eq!(a.external_path_markers(), &["/target/", "/.m2/"]);
        assert_eq!(a.server_package(), None);
        assert!(a.matches_uri("file:///repo/Main.java"));
        assert!(!a.matches_uri("file:///repo/Main.py"));
    }

    #[test]
    fn tag_missing_extensions_is_dropped_others_kept() {
        let mut vars = HashMap::new();
        vars.insert("SEMNAV_LSP_CPP_EXTENSIONS", ".cpp,.hpp");
        // "java" has no SEMNAV_LSP_JAVA_EXTENSIONS entry at all.
        let adapters = parse_custom_adapters("java,cpp", lookup_from(&vars));
        assert_eq!(adapters.len(), 1);
        assert_eq!(adapters[0].language_name(), "cpp");
        assert_eq!(adapters[0].file_extensions(), &[".cpp", ".hpp"]);
    }

    #[test]
    fn blank_and_whitespace_only_tags_are_ignored() {
        let mut vars = HashMap::new();
        vars.insert("SEMNAV_LSP_RUBY_EXTENSIONS", ".rb");
        let adapters = parse_custom_adapters(" , ruby ,  , ", lookup_from(&vars));
        assert_eq!(adapters.len(), 1);
        assert_eq!(adapters[0].language_name(), "ruby");
    }

    #[test]
    fn external_markers_default_to_empty_when_unset() {
        let mut vars = HashMap::new();
        vars.insert("SEMNAV_LSP_PHP_EXTENSIONS", ".php");
        let adapters = parse_custom_adapters("php", lookup_from(&vars));
        assert_eq!(adapters.len(), 1);
        assert!(adapters[0].external_path_markers().is_empty());
    }

    #[test]
    fn server_command_uses_a_placeholder_naming_the_tag() {
        let mut vars = HashMap::new();
        vars.insert("SEMNAV_LSP_JAVA_EXTENSIONS", ".java");
        let adapters = parse_custom_adapters("java", lookup_from(&vars));
        let spec = adapters[0].server_command();
        assert!(spec.program.contains("JAVA"));
        assert!(spec.args.is_empty());
    }
}

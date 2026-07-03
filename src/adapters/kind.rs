//! LSP-standard `SymbolKind` and the adapter-classified `NodeKind`.

/// LSP `SymbolKind` (protocol values 1-26). Protocol-common, so pyright and
/// tsserver emit identical numbers and need no cross-server normalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    File = 1,
    Module = 2,
    Namespace = 3,
    Package = 4,
    Class = 5,
    Method = 6,
    Property = 7,
    Field = 8,
    Constructor = 9,
    Enum = 10,
    Interface = 11,
    Function = 12,
    Variable = 13,
    Constant = 14,
    String = 15,
    Number = 16,
    Boolean = 17,
    Array = 18,
    Object = 19,
    Key = 20,
    Null = 21,
    EnumMember = 22,
    Struct = 23,
    Event = 24,
    Operator = 25,
    TypeParameter = 26,
}

impl SymbolKind {
    /// Parse a raw LSP kind number; `None` for out-of-range/unknown values.
    pub fn from_u32(value: u32) -> Option<Self> {
        match value {
            1 => Some(Self::File),
            2 => Some(Self::Module),
            3 => Some(Self::Namespace),
            4 => Some(Self::Package),
            5 => Some(Self::Class),
            6 => Some(Self::Method),
            7 => Some(Self::Property),
            8 => Some(Self::Field),
            9 => Some(Self::Constructor),
            10 => Some(Self::Enum),
            11 => Some(Self::Interface),
            12 => Some(Self::Function),
            13 => Some(Self::Variable),
            14 => Some(Self::Constant),
            15 => Some(Self::String),
            16 => Some(Self::Number),
            17 => Some(Self::Boolean),
            18 => Some(Self::Array),
            19 => Some(Self::Object),
            20 => Some(Self::Key),
            21 => Some(Self::Null),
            22 => Some(Self::EnumMember),
            23 => Some(Self::Struct),
            24 => Some(Self::Event),
            25 => Some(Self::Operator),
            26 => Some(Self::TypeParameter),
            _ => None,
        }
    }

    /// Stable label used for `nodes.node_kind` (`docs/design/graph-model.md`).
    pub fn name(self) -> &'static str {
        match self {
            Self::File => "File",
            Self::Module => "Module",
            Self::Namespace => "Namespace",
            Self::Package => "Package",
            Self::Class => "Class",
            Self::Method => "Method",
            Self::Property => "Property",
            Self::Field => "Field",
            Self::Constructor => "Constructor",
            Self::Enum => "Enum",
            Self::Interface => "Interface",
            Self::Function => "Function",
            Self::Variable => "Variable",
            Self::Constant => "Constant",
            Self::String => "String",
            Self::Number => "Number",
            Self::Boolean => "Boolean",
            Self::Array => "Array",
            Self::Object => "Object",
            Self::Key => "Key",
            Self::Null => "Null",
            Self::EnumMember => "EnumMember",
            Self::Struct => "Struct",
            Self::Event => "Event",
            Self::Operator => "Operator",
            Self::TypeParameter => "TypeParameter",
        }
    }
}

/// Adapter-classified node kind (`docs/design/language-adapters.md` "NodeKind").
///
/// Standard LSP `SymbolKind` is held as-is; server-specific custom values fall
/// back to a string label so information is never lost. The TS `type`-alias
/// (`SymbolKind=13`) trap is refined to `Custom("TypeAlias")` later via hover
/// `construct`, not at `map_symbol_kind` time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    Standard(SymbolKind),
    Custom(String),
}

impl NodeKind {
    /// Serialize to the `nodes.node_kind` column label.
    pub fn to_label(&self) -> String {
        match self {
            Self::Standard(kind) => kind.name().to_string(),
            Self::Custom(label) => label.clone(),
        }
    }

    /// Extract the leading declaration keyword from a hover signature
    /// (`docs/design/language-adapters.md` "Refinement via hover"), e.g. `type`,
    /// `interface`, `class`, `function`, `const`. Skips markdown code-fence
    /// lines and common modifiers (`export`, `declare`, ...) to reach the
    /// keyword. `None` when the first content line has no recognized keyword.
    pub fn construct_from_hover(hover_text: &str) -> Option<String> {
        const KEYWORDS: &[&str] = &[
            "type",
            "interface",
            "class",
            "function",
            "const",
            "let",
            "var",
            "enum",
            "namespace",
            "module",
        ];
        const MODIFIERS: &[&str] = &[
            "export",
            "declare",
            "default",
            "public",
            "private",
            "protected",
            "readonly",
            "abstract",
            "async",
            "static",
        ];
        let first_line = hover_text
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.starts_with("```"))?;
        let word = first_line
            .split_whitespace()
            .find(|w| !MODIFIERS.contains(w))?;
        KEYWORDS.contains(&word).then(|| word.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_u32_roundtrips_standard_values() {
        assert_eq!(SymbolKind::from_u32(5), Some(SymbolKind::Class));
        assert_eq!(SymbolKind::from_u32(12), Some(SymbolKind::Function));
        assert_eq!(SymbolKind::from_u32(13), Some(SymbolKind::Variable));
        assert_eq!(SymbolKind::from_u32(26), Some(SymbolKind::TypeParameter));
    }

    #[test]
    fn from_u32_rejects_unknown() {
        assert_eq!(SymbolKind::from_u32(0), None);
        assert_eq!(SymbolKind::from_u32(27), None);
        assert_eq!(SymbolKind::from_u32(99), None);
    }

    #[test]
    fn node_kind_label_serializes() {
        assert_eq!(NodeKind::Standard(SymbolKind::Class).to_label(), "Class");
        assert_eq!(
            NodeKind::Standard(SymbolKind::Function).to_label(),
            "Function"
        );
        assert_eq!(
            NodeKind::Custom("TypeAlias".to_string()).to_label(),
            "TypeAlias"
        );
        assert_eq!(
            NodeKind::Custom("Unknown(99)".to_string()).to_label(),
            "Unknown(99)"
        );
    }
}

// kernava-indexer: tree-sitter parse dispatch
// P1 task 1.5: Language enum + parser dispatch

use anyhow::{anyhow, Result};
use std::path::Path;
use tree_sitter::{Parser, Tree};

/// Languages supported by the indexer.
/// Add new languages here; the parser and extractor dispatch on this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    TypeScript,
    Tsx,
    JavaScript,
    Jsx,
    Python,
    Rust,
    Go,
    Java,
    CSharp,
    Ruby,
    Php,
    C,
    Cpp,
}

impl Language {
    /// Detect language from file extension.
    pub fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?;
        match ext {
            "ts" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
            "js" | "mjs" | "cjs" => Some(Self::JavaScript),
            "jsx" => Some(Self::Jsx),
            "py" => Some(Self::Python),
            "rs" => Some(Self::Rust),
            "go" => Some(Self::Go),
            "java" => Some(Self::Java),
            "cs" => Some(Self::CSharp),
            "rb" => Some(Self::Ruby),
            "php" => Some(Self::Php),
            "c" | "h" => Some(Self::C),
            "cpp" | "cc" | "cxx" | "hpp" | "hxx" => Some(Self::Cpp),
            _ => None,
        }
    }

    /// Get the tree-sitter Language for this language.
    pub fn ts_language(&self) -> tree_sitter::Language {
        match self {
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::JavaScript | Self::Jsx => tree_sitter_javascript::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
            Self::Java => tree_sitter_java::LANGUAGE.into(),
            Self::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            Self::Ruby => tree_sitter_ruby::LANGUAGE.into(),
            Self::Php => tree_sitter_php::LANGUAGE_PHP.into(),
            Self::C => tree_sitter_c::LANGUAGE.into(),
            Self::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        }
    }

    /// String identifier for storage.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::TypeScript => "typescript",
            Self::Tsx => "tsx",
            Self::JavaScript => "javascript",
            Self::Jsx => "jsx",
            Self::Python => "python",
            Self::Rust => "rust",
            Self::Go => "go",
            Self::Java => "java",
            Self::CSharp => "csharp",
            Self::Ruby => "ruby",
            Self::Php => "php",
            Self::C => "c",
            Self::Cpp => "cpp",
        }
    }

    /// True for TypeScript-family languages (TS, TSX, JS, JSX).
    pub fn is_ts_family(self) -> bool {
        matches!(self, Self::TypeScript | Self::Tsx | Self::JavaScript | Self::Jsx)
    }

    /// True for Rust.
    pub fn is_rust(self) -> bool {
        matches!(self, Self::Rust)
    }

    /// True for Go.
    pub fn is_go(self) -> bool {
        matches!(self, Self::Go)
    }

    /// True for Java.
    pub fn is_java(self) -> bool {
        matches!(self, Self::Java)
    }

    /// True for C and C++ (shared grammar node kinds: function_definition, call_expression, etc).
    pub fn is_c_family(self) -> bool {
        matches!(self, Self::C | Self::Cpp)
    }

    /// ponytail: Kotlin deferred — fwcd/tree-sitter-kotlin 0.3.8 requires tree-sitter <0.23,
    /// incompatible with workspace 0.25. Revisit when fwcd updates or a fork supports 0.25.


    /// True for Ruby.
    pub fn is_ruby(self) -> bool {
        matches!(self, Self::Ruby)
    }

    /// True for PHP.
    pub fn is_php(self) -> bool {
        matches!(self, Self::Php)
    }

    /// True for C#.
    pub fn is_csharp(self) -> bool {
        matches!(self, Self::CSharp)
    }
}

/// Parse source code with the given language. Returns the syntax tree.
pub fn parse(source: &str, lang: Language) -> Result<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&lang.ts_language())
        .map_err(|e| anyhow!("failed to set tree-sitter language: {e}"))?;
    parser
        .parse(source, None)
        .ok_or_else(|| anyhow!("parser returned None (likely OOM or encoding error)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_ts() {
        assert_eq!(
            Language::from_path(Path::new("src/foo.ts")),
            Some(Language::TypeScript)
        );
        assert_eq!(
            Language::from_path(Path::new("src/foo.tsx")),
            Some(Language::Tsx)
        );
        assert_eq!(Language::from_path(Path::new("src/README.md")), None);
    }

    #[test]
    fn test_parse_typescript() {
        let code = "function add(a: number, b: number): number { return a + b; }";
        let tree = parse(code, Language::TypeScript).unwrap();
        let root = tree.root_node();
        assert!(!root.has_error());
        // Root of TS source is a `program` node
        assert_eq!(root.kind(), "program");
    }

    #[test]
    fn test_parse_tsx() {
        let code = "const App = () => <div>Hello</div>;";
        let tree = parse(code, Language::Tsx).unwrap();
        let root = tree.root_node();
        assert!(!root.has_error());
    }

    #[test]
    fn test_parse_invalid_recovers() {
        // tree-sitter is error-tolerant; it parses what it can
        let code = "function broken({  ";
        let tree = parse(code, Language::TypeScript).unwrap();
        let root = tree.root_node();
        // Should still produce a tree, possibly with ERROR nodes
        assert_eq!(root.kind(), "program");
    }
}

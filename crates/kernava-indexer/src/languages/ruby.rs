// kernava-indexer: Ruby import parser
// ponytail: Ruby require paths ('helper', './lib/thing') don't match file-path-based
// qnames in the resolver for v1. Cross-file resolution relies on SameFile + global-unique.
// Upgrade path: resolver learns Ruby require-to-path mapping.

use crate::languages::ts::ModuleMap;
use tree_sitter::Node;

/// Parse Ruby `require`/`require_relative` from the AST root.
/// `require 'json'` → local_name="json", path="json"
/// `require_relative 'helper'` → local_name="helper", path="helper"
pub fn parse_imports(root: &Node, source: &str, map: &mut ModuleMap) {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "call" {
            // require 'json' parses as call with method="require" and argument=string
            let method = child.child_by_field_name("method");
            if let Some(m) = method {
                let method_name = node_text(&m, source);
                if method_name == "require" || method_name == "require_relative" {
                    // Argument is first child of arguments node
                    let args = child.child_by_field_name("arguments");
                    if let Some(args) = args {
                        let mut ac = args.walk();
                        for arg in args.children(&mut ac) {
                            if arg.kind() == "string" {
                                let raw = node_text(&arg, source);
                                // Strip quotes
                                let path = raw.trim_matches(|c| c == '\'' || c == '"').to_string();
                                if !path.is_empty() {
                                    let local = path.rsplit('/').next().unwrap_or(&path).to_string();
                                    map.imports.insert(local, path.clone());
                                    map.module_paths.push(path);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn node_text(node: &Node, source: &str) -> String {
    let start = node.start_byte();
    let end = node.end_byte();
    source[start..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{self, Language};

    fn parse_imports_code(code: &str, map: &mut ModuleMap) {
        let tree = parser::parse(code, Language::Ruby).unwrap();
        parse_imports(&tree.root_node(), code, map);
    }

    #[test]
    fn test_require() {
        let mut map = ModuleMap::default();
        parse_imports_code(r#"require 'json'
"#, &mut map);
        assert_eq!(map.imports.get("json"), Some(&"json".to_string()));
    }

    #[test]
    fn test_require_relative() {
        let mut map = ModuleMap::default();
        parse_imports_code(r#"require_relative 'helper'
"#, &mut map);
        assert_eq!(map.imports.get("helper"), Some(&"helper".to_string()));
    }

    #[test]
    fn test_path_require() {
        let mut map = ModuleMap::default();
        parse_imports_code(r#"require 'lib/thing'
"#, &mut map);
        assert_eq!(map.imports.get("thing"), Some(&"lib/thing".to_string()));
    }
}

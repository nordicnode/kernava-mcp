// kernava-indexer: PHP import parser
// ponytail: PHP use declarations (Example\Math) don't match file-path-based
// qnames in the resolver for v1. Cross-file resolution relies on SameFile + global-unique.
// Upgrade path: resolver learns PHP namespace-to-path mapping.

use crate::languages::ts::ModuleMap;
use tree_sitter::Node;

/// Parse PHP `use` declarations from the AST root.
/// `use Example\Math;` → local_name="Math", path="Example\Math"
/// `use Example\Math as M;` → local_name="M", path="Example\Math"
/// `use function Example\func;` → function import
/// `use const Example\CONST;` → const import
pub fn parse_imports(root: &Node, source: &str, map: &mut ModuleMap) {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "namespace_use_declaration" {
            parse_use_declaration(&child, source, map);
        }
        // Also check inside namespace blocks
        if child.kind() == "namespace_definition" {
            let mut nc = child.walk();
            for nc_child in child.children(&mut nc) {
                if nc_child.kind() == "namespace_use_declaration" {
                    parse_use_declaration(&nc_child, source, map);
                }
            }
        }
    }
}

fn parse_use_declaration(node: &Node, source: &str, map: &mut ModuleMap) {
    // namespace_use_declaration → namespace_use_clause children
    // Each clause has a `qualified_name` child, optional `alias` field
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "namespace_use_clause" || child.kind() == "namespace_use_clause" {
            parse_use_clause(&child, source, map);
        }
        // Group use: namespace_use_group (use Example\{Math, Util})
        if child.kind() == "namespace_use_group" {
            let mut gc = child.walk();
            for gc_child in child.children(&mut gc) {
                if gc_child.kind() == "namespace_use_group_clause" {
                    parse_use_clause(&gc_child, source, map);
                }
            }
        }
    }
}

fn parse_use_clause(node: &Node, source: &str, map: &mut ModuleMap) {
    // Find qualified_name child
    let mut cursor = node.walk();
    let qname = node
        .children(&mut cursor)
        .find(|c| c.kind() == "qualified_name");
    let Some(qname) = qname else {
        return;
    };
    let path = node_text(&qname, source);

    // Check for alias field
    let alias = node
        .child_by_field_name("alias")
        .map(|n| node_text(&n, source));

    let local_name = match alias {
        Some(a) if !a.is_empty() => a,
        _ => path.rsplit('\\').next().unwrap_or(&path).to_string(),
    };

    if !path.is_empty() {
        map.imports.insert(local_name, path.clone());
        map.module_paths.push(path);
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
        let tree = parser::parse(code, Language::Php).unwrap();
        parse_imports(&tree.root_node(), code, map);
    }

    #[test]
    fn test_simple_use() {
        let mut map = ModuleMap::default();
        parse_imports_code(
            r#"<?php
use Example\Math;
"#,
            &mut map,
        );
        assert_eq!(map.imports.get("Math"), Some(&"Example\\Math".to_string()));
    }

    #[test]
    fn test_aliased_use() {
        let mut map = ModuleMap::default();
        parse_imports_code(
            r#"<?php
use Example\Math as M;
"#,
            &mut map,
        );
        assert_eq!(map.imports.get("M"), Some(&"Example\\Math".to_string()));
    }

    #[test]
    fn test_multiple_use() {
        let mut map = ModuleMap::default();
        parse_imports_code(
            r#"<?php
use Example\Math;
use Example\Util;
"#,
            &mut map,
        );
        assert_eq!(map.imports.get("Math"), Some(&"Example\\Math".to_string()));
        assert_eq!(map.imports.get("Util"), Some(&"Example\\Util".to_string()));
    }
}

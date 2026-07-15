// Rust import parser — maps `use` declarations into a module map for call resolution.
//
// Rust `use` forms:
//   use std::collections::HashMap;   → { "HashMap": "std::collections::HashMap" }
//   use foo::bar;                     → { "bar": "foo::bar" }
//   use foo::{bar, baz};              → { "bar": "foo::bar", "baz": "foo::baz" }
//   use foo as bar;                   → { "bar": "foo" }
//
// ponytail: `use` paths are `crate::module::func` style — won't match file-path-based
// qnames in the resolver for v1. Fixture relies on SameFile + global-unique strategies.
// `use` parsing populates ModuleMap for architecture/dead-code analysis. Upgrade path:
// resolver learns crate-relative path mapping (DEVELOPMENT_PLAN.md "Resolver Gaps").

use tree_sitter::Node;

use super::ModuleMap;

/// Parse all Rust `use_declaration` nodes from the AST root.
pub fn parse_imports(root: &Node, source: &str, map: &mut ModuleMap) {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "use_declaration" {
            parse_use_declaration(&child, source, map);
        }
    }
}

/// Parse a `use_declaration` node.
/// Field: `argument` — the use clause (use_as_clause, scoped_use_list, scoped_identifier, identifier, etc.)
fn parse_use_declaration(node: &Node, source: &str, map: &mut ModuleMap) {
    let arg = match node.child_by_field_name("argument") {
        Some(a) => a,
        None => return,
    };
    parse_use_clause(&arg, source, map);
}

/// Recursively parse a use clause, collecting all imported names + their full paths.
fn parse_use_clause(node: &Node, source: &str, map: &mut ModuleMap) {
    match node.kind() {
        // `foo::bar` — scoped identifier; full path is the text, imported name is last segment
        "scoped_identifier" => {
            let full = node_text(node, source);
            let name = full.rsplit("::").next().unwrap_or(&full).to_string();
            map.imports.insert(name, full.clone());
            map.module_paths.push(full);
        }
        // `foo as bar` — use_as_clause, field: path + alias
        "use_as_clause" => {
            let path = node.child_by_field_name("path");
            let alias = node.child_by_field_name("alias");
            if let (Some(p), Some(a)) = (path, alias) {
                let path_str = node_text(&p, source);
                let alias_str = node_text(&a, source);
                map.imports.insert(alias_str, path_str.clone());
                map.module_paths.push(path_str);
            }
        }
        // `foo::{bar, baz}` — scoped_use_list; `path` field = prefix, `list` field = single use_list node
        "scoped_use_list" => {
            let prefix = node
                .child_by_field_name("path")
                .map(|p| node_text(&p, source))
                .unwrap_or_default();
            if let Some(list) = node.child_by_field_name("list") {
                let mut list_cursor = list.walk();
                for item in list.children(&mut list_cursor) {
                    collect_use_item(&item, source, &prefix, map);
                }
            }
        }
        // bare identifier
        "identifier" => {
            let name = node_text(node, source);
            map.imports.insert(name.clone(), name.clone());
            map.module_paths.push(name);
        }
        // wildcard `*` — `foo::*`
        "use_wildcard" => {
            map.module_paths.push(node_text(node, source));
        }
        _ => {
            // Recurse into children for any unhandled node type
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                parse_use_clause(&child, source, map);
            }
        }
    }
}

/// Collect a single item from a `use foo::{bar, baz}` list.
fn collect_use_item(node: &Node, source: &str, prefix: &str, map: &mut ModuleMap) {
    match node.kind() {
        "identifier" => {
            let name = node_text(node, source);
            let full = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{}::{}", prefix, name)
            };
            map.imports.insert(name, full.clone());
            map.module_paths.push(full);
        }
        "use_as_clause" => {
            let path = node.child_by_field_name("path");
            let alias = node.child_by_field_name("alias");
            if let (Some(p), Some(a)) = (path, alias) {
                let p_str = node_text(&p, source);
                let a_str = node_text(&a, source);
                let full = if prefix.is_empty() {
                    p_str
                } else {
                    format!("{}::{}", prefix, p_str)
                };
                map.imports.insert(a_str, full.clone());
                map.module_paths.push(full);
            }
        }
        "scoped_identifier" => {
            let full = node_text(node, source);
            let name = full.rsplit("::").next().unwrap_or(&full).to_string();
            if prefix.is_empty() {
                map.imports.insert(name, full.clone());
                map.module_paths.push(full);
            } else {
                let prefixed = format!("{}::{}", prefix, full);
                map.imports.insert(name, prefixed.clone());
                map.module_paths.push(prefixed);
            }
        }
        "use_wildcard" => {
            // `foo::*` inside a use list — record the module path, no binding
            let path = if prefix.is_empty() {
                "*".to_string()
            } else {
                format!("{}::*", prefix)
            };
            map.module_paths.push(path);
        }
        _ => {}
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
        let tree = parser::parse(code, Language::Rust).unwrap();
        super::parse_imports(&tree.root_node(), code, map);
    }

    #[test]
    fn test_simple_use() {
        let mut map = ModuleMap::default();
        parse_imports_code("use std::collections::HashMap;", &mut map);
        assert_eq!(map.imports.get("HashMap"), Some(&"std::collections::HashMap".to_string()));
    }

    #[test]
    fn test_use_group() {
        let mut map = ModuleMap::default();
        parse_imports_code("use foo::{bar, baz};", &mut map);
        assert_eq!(map.imports.get("bar"), Some(&"foo::bar".to_string()));
        assert_eq!(map.imports.get("baz"), Some(&"foo::baz".to_string()));
    }

    #[test]
    fn test_use_as() {
        let mut map = ModuleMap::default();
        parse_imports_code("use foo::bar as baz;", &mut map);
        assert_eq!(map.imports.get("baz"), Some(&"foo::bar".to_string()));
    }

    #[test]
    fn test_use_wildcard() {
        let mut map = ModuleMap::default();
        parse_imports_code("use foo::*;", &mut map);
        // wildcard just records module path, no binding
        assert!(map.imports.is_empty());
        assert!(map.module_paths.iter().any(|p| p.contains("*")));
    }
}

// kernava-indexer: TypeScript import parser
// P1 task 1.7: Parse import statements into a module map

use std::collections::HashMap;

/// Maps imported names to their source module path.
/// Built from import statements in a single file.
///
/// Examples:
///   import { foo } from './bar'           → {"foo": "./bar"}
///   import { foo as baz } from './bar'    → {"baz": "./bar"}
///   import * as utils from './utils'      → {"utils": "./utils"}  (namespace)
///   import express from 'express'         → {"express": "express"} (default)
///   import Foo from './foo'              → {"Foo": "./foo"}
#[derive(Debug, Clone, Default)]
pub struct ModuleMap {
    /// imported_local_name → source_module_path
    pub imports: HashMap<String, String>,
    /// Set of all imported module paths (for building import_edges reverse-dep map)
    pub module_paths: Vec<String>,
}

/// Parse a tree-sitter import_statement node and add its bindings to the module map.
/// Returns true if the node was an import and was processed.
pub fn parse_import_node(node: &tree_sitter::Node, source: &str, map: &mut ModuleMap) -> bool {
    if node.kind() != "import_statement" {
        return false;
    }

    // Extract the module path from the string_literal child
    let module_path = extract_string_literal(node, source);

    if let Some(path) = module_path {
        map.module_paths.push(path.clone());

        // Walk children to find imported identifiers
        let mut cursor = node.walk();

        // The import_clause contains the imported names
        for child in node.children(&mut cursor) {
            if child.kind() == "import_clause" {
                extract_import_clause(&child, source, &path, map);
            }
        }
    }

    true
}

fn extract_import_clause(
    node: &tree_sitter::Node,
    source: &str,
    module_path: &str,
    map: &mut ModuleMap,
) {
    let mut cursor = node.walk();

    for child in node.children(&mut cursor) {
        match child.kind() {
            "named_imports" => {
                // { foo, bar as baz }
                let mut inner_cursor = child.walk();
                for spec in child.children(&mut inner_cursor) {
                    if spec.kind() == "import_specifier" {
                        extract_import_specifier(&spec, source, module_path, map);
                    }
                }
            }
            "namespace_import" => {
                // import * as utils from '...' — the identifier is a child
                let mut inner_cursor = child.walk();
                for id_node in child.children(&mut inner_cursor) {
                    if id_node.kind() == "identifier" {
                        let name = node_text(&id_node, source);
                        map.imports.insert(name, module_path.to_string());
                    }
                }
            }
            "identifier" => {
                // Default import: import Foo from '...'
                let name = node_text(&child, source);
                map.imports.insert(name, module_path.to_string());
            }
            _ => {}
        }
    }
}

fn extract_import_specifier(
    node: &tree_sitter::Node,
    source: &str,
    module_path: &str,
    map: &mut ModuleMap,
) {
    let mut cursor = node.walk();
    let mut local_name: Option<String> = None;

    for child in node.children(&mut cursor) {
        match child.kind() {
            "identifier" => {
                // First identifier is the imported name;
                // if there's an `as` alias, the second identifier is the local name
                if local_name.is_none() {
                    local_name = Some(node_text(&child, source));
                } else {
                    local_name = Some(node_text(&child, source));
                }
            }
            _ => {}
        }
    }

    if let Some(name) = local_name {
        map.imports.insert(name, module_path.to_string());
    }
}

/// Extract the string literal value from an import statement (the module path).
fn extract_string_literal(node: &tree_sitter::Node, source: &str) -> Option<String> {
    // If the node itself is a string, strip quotes directly.
    if node.kind() == "string" || node.kind() == "string_literal" {
        let raw = node_text(node, source);
        let trimmed = raw
            .trim_start_matches(['"', '\''])
            .trim_end_matches(['"', '\'']);
        return Some(trimmed.to_string());
    }
    // Otherwise look for a string child (e.g. inside an import_statement).
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "string" || child.kind() == "string_literal" {
            let raw = node_text(&child, source);
            let trimmed = raw
                .trim_start_matches(['"', '\''])
                .trim_end_matches(['"', '\'']);
            return Some(trimmed.to_string());
        }
    }
    None
}

/// Get the text covered by a node.
fn node_text(node: &tree_sitter::Node, source: &str) -> String {
    let start = node.start_byte();
    let end = node.end_byte();
    source[start..end].to_string()
}

/// Parse CommonJS require() calls from the AST.
/// Handles:
///   const foo = require('./bar')          → {"foo": "./bar"}
///   const { foo } = require('./bar')       → {"foo": "./bar"}
///   const { foo: bar } = require('./bar')  → {"bar": "./bar"}
///   require('./bar')                       → (no binding, just module_paths)
pub fn parse_require_calls(root: &tree_sitter::Node, source: &str, map: &mut ModuleMap) {
    scan_requires(root, source, map);
}

fn scan_requires(node: &tree_sitter::Node, source: &str, map: &mut ModuleMap) {
    // Look for variable_declarator with require() initializer
    if node.kind() == "variable_declarator" {
        if let Some(name_node) = node.child_by_field_name("name") {
            if let Some(value_node) = node.child_by_field_name("value") {
                if let Some(module_path) = extract_require_path(&value_node, source) {
                    map.module_paths.push(module_path.clone());
                    // Name can be an identifier or an object_pattern (destructuring)
                    if name_node.kind() == "identifier" {
                        let local = node_text(&name_node, source);
                        map.imports.insert(local, module_path);
                    } else if name_node.kind() == "object_pattern" {
                        // Destructuring: { foo } or { foo: bar }
                        let mut c = name_node.walk();
                        for prop in name_node.children(&mut c) {
                            if prop.kind() == "shorthand_property_identifier"
                                || prop.kind() == "shorthand_property_identifier_pattern"
                            {
                                let local = node_text(&prop, source);
                                map.imports.insert(local, module_path.clone());
                            } else if prop.kind() == "pair_pattern" || prop.kind() == "pair_property" {
                                // { foo: bar } — bar is local, foo is imported name
                                if let Some(value) = prop.child_by_field_name("value") {
                                    if value.kind() == "identifier" {
                                        let local = node_text(&value, source);
                                        map.imports.insert(local, module_path.clone());
                                    }
                                }
                            }
                        }
                    }
                    return;
                }
            }
        }
    }

    // Also catch bare require('./bar') with no assignment — track module path only
    if node.kind() == "call_expression" {
        if let Some(module_path) = extract_require_path(node, source) {
            if !map.module_paths.contains(&module_path) {
                map.module_paths.push(module_path);
            }
        }
    }

    // Recurse into all children
    let mut c = node.walk();
    for child in node.children(&mut c) {
        scan_requires(&child, source, map);
    }
}

/// Extract the module path from a require() call expression.
/// Returns Some(path) if the node is require('...') or require("...").
fn extract_require_path(node: &tree_sitter::Node, source: &str) -> Option<String> {
    if node.kind() != "call_expression" {
        return None;
    }
    let callee = node.child_by_field_name("function")?;
    if node_text(&callee, source) != "require" {
        return None;
    }
    let args = node.child_by_field_name("arguments")?;
    // First argument should be a string literal
    let mut cursor = args.walk();
    for arg in args.children(&mut cursor) {
        if arg.kind() == "string" || arg.kind() == "string_fragment" {
            return extract_string_literal(&arg, source);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{parse, Language};

    fn extract_imports(source: &str) -> ModuleMap {
        let tree = parse(source, Language::TypeScript).unwrap();
        let root = tree.root_node();
        let mut map = ModuleMap::default();

        // Walk all nodes looking for import_statements
        fn walk_node(node: &tree_sitter::Node, source: &str, map: &mut ModuleMap) {
            if node.kind() == "import_statement" {
                parse_import_node(node, source, map);
                return; // don't recurse into imports
            }
            let mut c = node.walk();
            for child in node.children(&mut c) {
                walk_node(&child, source, map);
            }
        }

        walk_node(&root, source, &mut map);
        map
    }

    #[test]
    fn test_named_imports() {
        let src = "import { foo, bar } from './utils';";
        let map = extract_imports(src);
        assert_eq!(map.imports.get("foo"), Some(&"./utils".to_string()));
        assert_eq!(map.imports.get("bar"), Some(&"./utils".to_string()));
        assert_eq!(map.module_paths, vec!["./utils".to_string()]);
    }

    #[test]
    fn test_aliased_import() {
        let src = "import { foo as baz } from './utils';";
        let map = extract_imports(src);
        // The local name (what the code uses) is "baz"
        assert_eq!(map.imports.get("baz"), Some(&"./utils".to_string()));
        assert!(map.imports.get("foo").is_none());
    }

    #[test]
    fn test_namespace_import() {
        let src = "import * as utils from './utils';";
        let map = extract_imports(src);
        assert_eq!(map.imports.get("utils"), Some(&"./utils".to_string()));
    }

    #[test]
    fn test_default_import() {
        let src = "import express from 'express';";
        let map = extract_imports(src);
        assert_eq!(map.imports.get("express"), Some(&"express".to_string()));
    }

    #[test]
    fn test_multiple_imports() {
        let src = r#"
            import { foo } from './utils';
            import { bar } from './other';
        "#;
        let map = extract_imports(src);
        assert_eq!(map.imports.get("foo"), Some(&"./utils".to_string()));
        assert_eq!(map.imports.get("bar"), Some(&"./other".to_string()));
        assert_eq!(map.module_paths.len(), 2);
    }

    // ── parse_require_calls tests ──

    fn extract_requires(source: &str) -> ModuleMap {
        let tree = parse(source, Language::JavaScript).unwrap();
        let root = tree.root_node();
        let mut map = ModuleMap::default();
        parse_require_calls(&root, source, &mut map);
        map
    }

    #[test]
    fn test_require_shorthand() {
        let src = "const { add, multiply } = require('./math');";
        let map = extract_requires(src);
        assert_eq!(map.imports.get("add"), Some(&"./math".to_string()));
        assert_eq!(map.imports.get("multiply"), Some(&"./math".to_string()));
        assert_eq!(map.module_paths, vec!["./math".to_string()]);
    }

    #[test]
    fn test_require_simple() {
        let src = "const foo = require('./bar');";
        let map = extract_requires(src);
        assert_eq!(map.imports.get("foo"), Some(&"./bar".to_string()));
        assert_eq!(map.module_paths, vec!["./bar".to_string()]);
    }

    #[test]
    fn test_require_nested_in_function() {
        let src = "function main() { const { add } = require('./math'); return add(1); }";
        let map = extract_requires(src);
        assert_eq!(map.imports.get("add"), Some(&"./math".to_string()));
    }

    #[test]
    fn test_require_aliased_destructure() {
        let src = "const { foo: bar } = require('./mod');";
        let map = extract_requires(src);
        assert_eq!(map.imports.get("bar"), Some(&"./mod".to_string()));
        assert!(map.imports.get("foo").is_none());
    }
}

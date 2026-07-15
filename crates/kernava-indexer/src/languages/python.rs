// Python import parser — maps import names to module paths for call resolution.
//
// Python import forms:
//   import foo            → { "foo": "foo" }
//   import foo.bar.baz    → { "foo": "foo.bar.baz" }  (binding is first segment)
//   import foo as bar     → { "bar": "foo" }
//   from foo import bar   → { "bar": "foo" }
//   from .mod import foo  → { "foo": "./mod" }
//   from . import foo      → { "foo": "." }

use tree_sitter::Node;

use super::ModuleMap;

/// Parse all Python import statements from the AST root.
/// Handles `import_statement` and `import_from_statement`.
pub fn parse_imports(root: &Node, source: &str, map: &mut ModuleMap) {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        match child.kind() {
            "import_statement" => parse_import_statement(&child, source, map),
            "import_from_statement" => parse_import_from(&child, source, map),
            _ => {}
        }
    }
}

/// `import foo` or `import foo.bar.baz` or `import foo as bar`
/// Children are `dotted_name` or `aliased_import` nodes (field: `name`, multiple).
fn parse_import_statement(node: &Node, source: &str, map: &mut ModuleMap) {
    let mut cursor = node.walk();
    for child in node.children_by_field_name("name", &mut cursor) {
        match child.kind() {
            "dotted_name" => {
                let module_path = node_text(&child, source);
                // For `import foo.bar.baz`, the local binding is `foo` (first segment)
                let local = module_path
                    .split('.')
                    .next()
                    .unwrap_or(&module_path)
                    .to_string();
                map.imports.insert(local, module_path.clone());
                map.module_paths.push(module_path);
            }
            "aliased_import" => {
                // `foo as bar` — name=foo (dotted_name), alias=bar (identifier)
                let name = node_text(&child.child_by_field_name("name").unwrap_or(child), source);
                let alias = child
                    .child_by_field_name("alias")
                    .map(|a| node_text(&a, source))
                    .unwrap_or_else(|| name.split('.').next().unwrap_or(&name).to_string());
                map.imports.insert(alias, name.clone());
                map.module_paths.push(name);
            }
            _ => {}
        }
    }
}

/// `from foo import bar` or `from .mod import bar, baz`
/// Fields: `module_name` (dotted_name or relative_import), `name` (multiple: dotted_name or aliased_import).
fn parse_import_from(node: &Node, source: &str, map: &mut ModuleMap) {
    let module_path = get_module_path(node, source);

    let mut cursor = node.walk();
    for child in node.children_by_field_name("name", &mut cursor) {
        match child.kind() {
            "dotted_name" => {
                let local = node_text(&child, source);
                map.imports.insert(local, module_path.clone());
            }
            "aliased_import" => {
                // `bar as baz` — name=bar, alias=baz
                let name = node_text(&child.child_by_field_name("name").unwrap_or(child), source);
                let alias = child
                    .child_by_field_name("alias")
                    .map(|a| node_text(&a, source))
                    .unwrap_or(name.clone());
                map.imports.insert(alias, module_path.clone());
            }
            "wildcard_import" => {
                // `from foo import *` — no specific binding
            }
            _ => {}
        }
    }

    map.module_paths.push(module_path);
}

/// Extract the module path from an import_from_statement.
/// Handles relative imports: `from .mod import x` → "./mod", `from . import x` → ".",
/// `from ..mod import x` → "../mod/", `from ...mod import x` → "../../mod/".
fn get_module_path(node: &Node, source: &str) -> String {
    let mut module_path = String::new();
    let mut relative_dots = 0;

    // module_name field
    if let Some(mod_node) = node.child_by_field_name("module_name") {
        if mod_node.kind() == "relative_import" {
            let text = node_text(&mod_node, source);
            relative_dots = text.chars().take_while(|c| *c == '.').count();
            // relative_import may contain an embedded dotted_name for `from .mod import`
            // Check children for dotted_name
            let mut cursor = mod_node.walk();
            for child in mod_node.children(&mut cursor) {
                if child.kind() == "dotted_name" {
                    module_path = node_text(&child, source);
                }
            }
        } else if mod_node.kind() == "dotted_name" {
            module_path = node_text(&mod_node, source);
        }
    }

    // N dots → N-1 parent traversals: . = current (0 parents), .. = 1 parent, ... = 2 parents
    // Dotted module paths (pkg.mod) become path segments (pkg/mod) for file resolution.
    // No trailing slash — resolve_one_path uses Path::join + with_extension which breaks
    // on paths with trailing slash (produces dir/.ext instead of dir/file.ext).
    if relative_dots > 0 {
        let path_segments = module_path.replace('.', "/");
        if module_path.is_empty() {
            // No module after dots: . = current dir, .. = parent dir, ... = grandparent
            if relative_dots == 1 {
                ".".to_string()
            } else {
                "..".repeat(relative_dots - 1)
            }
        } else if relative_dots == 1 {
            format!("./{}", path_segments)
        } else {
            format!("{}{}", "../".repeat(relative_dots - 1), path_segments)
        }
    } else {
        module_path
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
    use crate::parser::{parse, Language};

    fn extract_py_imports(source: &str) -> ModuleMap {
        let tree = parse(source, Language::Python).unwrap();
        let root = tree.root_node();
        let mut map = ModuleMap::default();
        parse_imports(&root, source, &mut map);
        map
    }

    #[test]
    fn test_import_simple() {
        let map = extract_py_imports("import os");
        assert_eq!(map.imports.get("os"), Some(&"os".to_string()));
        assert_eq!(map.module_paths, vec!["os".to_string()]);
    }

    #[test]
    fn test_import_dotted() {
        let map = extract_py_imports("import os.path");
        assert_eq!(map.imports.get("os"), Some(&"os.path".to_string()));
    }

    #[test]
    fn test_import_aliased() {
        let map = extract_py_imports("import numpy as np");
        assert_eq!(map.imports.get("np"), Some(&"numpy".to_string()));
        assert!(!map.imports.contains_key("numpy"));
    }

    #[test]
    fn test_from_import() {
        let map = extract_py_imports("from math import sqrt");
        assert_eq!(map.imports.get("sqrt"), Some(&"math".to_string()));
    }

    #[test]
    fn test_from_import_aliased() {
        let map = extract_py_imports("from math import sqrt as s");
        assert_eq!(map.imports.get("s"), Some(&"math".to_string()));
        assert!(!map.imports.contains_key("sqrt"));
    }

    #[test]
    fn test_from_relative_import() {
        let map = extract_py_imports("from .util import helper");
        assert_eq!(map.imports.get("helper"), Some(&"./util".to_string()));
    }

    #[test]
    fn test_from_relative_multi_segment_import() {
        let map = extract_py_imports("from .pkg.mod import helper");
        assert_eq!(map.imports.get("helper"), Some(&"./pkg/mod".to_string()));
    }

    #[test]
    fn test_from_relative_dot_import() {
        let map = extract_py_imports("from . import helper");
        assert_eq!(map.imports.get("helper"), Some(&".".to_string()));
    }

    #[test]
    fn test_from_relative_two_dot_import() {
        let map = extract_py_imports("from ..mod import helper");
        assert_eq!(map.imports.get("helper"), Some(&"../mod".to_string()));
    }

    #[test]
    fn test_from_relative_three_dot_import() {
        let map = extract_py_imports("from ...pkg.mod import helper");
        assert_eq!(
            map.imports.get("helper"),
            Some(&"../../pkg/mod".to_string())
        );
    }

    #[test]
    fn test_from_relative_two_dot_no_module() {
        let map = extract_py_imports("from .. import helper");
        assert_eq!(map.imports.get("helper"), Some(&"..".to_string()));
    }

    #[test]
    fn test_multiple_imports() {
        let map = extract_py_imports("from math import sqrt\nfrom os import path");
        assert_eq!(map.imports.get("sqrt"), Some(&"math".to_string()));
        assert_eq!(map.imports.get("path"), Some(&"os".to_string()));
        assert_eq!(map.module_paths.len(), 2);
    }
}

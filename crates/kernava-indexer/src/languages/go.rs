// Go import parser — maps `import` declarations into a module map for call resolution.
//
// Go import forms:
//   import "fmt"                → { "fmt": "fmt" }
//   import "path/pkg"           → { "pkg": "path/pkg" }
//   import . "fmt"              → { ".": "fmt" }  (dot import — all exported names directly)
//   import alias "pkg"          → { "alias": "pkg" }
//   import ( "a"; "b" )         → { "a": "a", "b": "b" }
//
// ponytail: Go imports are package paths, not file paths. Won't match file-path-based
// qnames in resolver. Cross-file resolution relies on SameFile + global-unique.
// Upgrade path: resolver learns Go package-path mapping.

use tree_sitter::Node;

use super::ModuleMap;

/// Parse all Go `import_declaration` nodes from the AST root.
pub fn parse_imports(root: &Node, source: &str, map: &mut ModuleMap) {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "import_declaration" {
            parse_import_declaration(&child, source, map);
        }
    }
}

/// Parse a single `import_declaration` node.
/// Can be a single import or an import group: `import ( "a"; "b" )`
fn parse_import_declaration(node: &Node, source: &str, map: &mut ModuleMap) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "import_spec" => parse_import_spec(&child, source, map),
            "import_spec_list" => {
                let mut c = child.walk();
                for spec in child.children(&mut c) {
                    if spec.kind() == "import_spec" {
                        parse_import_spec(&spec, source, map);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Parse an `import_spec`: `path`, `alias path`, or `. path`
/// Fields: `path` (string literal), optional `name` (identifier or dot).
fn parse_import_spec(node: &Node, source: &str, map: &mut ModuleMap) {
    let path = node
        .child_by_field_name("path")
        .map(|p| node_text(&p, source).trim_matches('"').to_string())
        .unwrap_or_default();

    if path.is_empty() {
        return;
    }

    // Alias: `import f "fmt"` — name field is the alias identifier
    // Dot import: `import . "fmt"` — name field is a dot node
    let local_name = match node.child_by_field_name("name") {
        Some(n) => node_text(&n, source),
        None => path.rsplit('/').next().unwrap_or(&path).to_string(),
    };

    map.imports.insert(local_name, path.clone());
    map.module_paths.push(path);
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
        let tree = parser::parse(code, Language::Go).unwrap();
        super::parse_imports(&tree.root_node(), code, map);
    }

    #[test]
    fn test_single_import() {
        let mut map = ModuleMap::default();
        parse_imports_code(r#"package main
import "fmt"
"#, &mut map);
        assert_eq!(map.imports.get("fmt"), Some(&"fmt".to_string()));
    }

    #[test]
    fn test_path_import() {
        let mut map = ModuleMap::default();
        parse_imports_code(r#"package main
import "path/to/pkg"
"#, &mut map);
        assert_eq!(map.imports.get("pkg"), Some(&"path/to/pkg".to_string()));
    }

    #[test]
    fn test_aliased_import() {
        let mut map = ModuleMap::default();
        parse_imports_code(r#"package main
import f "fmt"
"#, &mut map);
        assert_eq!(map.imports.get("f"), Some(&"fmt".to_string()));
    }

    #[test]
    fn test_group_import() {
        let mut map = ModuleMap::default();
        parse_imports_code(r#"package main
import (
    "fmt"
    "path/to/pkg"
)
"#, &mut map);
        assert_eq!(map.imports.get("fmt"), Some(&"fmt".to_string()));
        assert_eq!(map.imports.get("pkg"), Some(&"path/to/pkg".to_string()));
    }
}

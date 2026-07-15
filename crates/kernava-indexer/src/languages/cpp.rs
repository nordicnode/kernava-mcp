// kernava-indexer: C++ import parser
// ponytail: C++ #include paths ("helper.h") don't match file-path-based
// qnames in the resolver for v1. Cross-file resolution relies on SameFile + global-unique.
// Upgrade path: resolver learns C++ header-to-source mapping.

use crate::languages::ts::ModuleMap;
use tree_sitter::Node;

/// Parse C++ `#include` directives from the AST root.
/// `#include <iostream>` → local_name="iostream", path="iostream"
/// `#include "helper.h"` → local_name="helper", path="helper.h"
pub fn parse_imports(root: &Node, source: &str, map: &mut ModuleMap) {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "preproc_include" {
            parse_include(&child, source, map);
        }
    }
}

fn parse_include(node: &Node, source: &str, map: &mut ModuleMap) {
    let path = match node.child_by_field_name("path") {
        Some(p) => node_text(&p, source),
        None => return,
    };
    let path = path.trim_matches(|c| c == '<' || c == '>' || c == '"').to_string();
    if path.is_empty() {
        return;
    }
    let local = path.rsplit_once('.').map(|(base, _)| base).unwrap_or(&path).to_string();
    let local = local.rsplit('/').next().unwrap_or(&local).to_string();
    map.imports.insert(local, path.clone());
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
        let tree = parser::parse(code, Language::Cpp).unwrap();
        parse_imports(&tree.root_node(), code, map);
    }

    #[test]
    fn test_system_include() {
        let mut map = ModuleMap::default();
        parse_imports_code("#include <iostream>\n", &mut map);
        assert_eq!(map.imports.get("iostream"), Some(&"iostream".to_string()));
    }

    #[test]
    fn test_local_include() {
        let mut map = ModuleMap::default();
        parse_imports_code(r#"#include "helper.h"
"#, &mut map);
        assert_eq!(map.imports.get("helper"), Some(&"helper.h".to_string()));
    }

    #[test]
    fn test_path_include() {
        let mut map = ModuleMap::default();
        parse_imports_code(r#"#include "lib/thing.hpp"
"#, &mut map);
        assert_eq!(map.imports.get("thing"), Some(&"lib/thing.hpp".to_string()));
    }
}

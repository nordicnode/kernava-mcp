// kernava-indexer: Java import parser
// ponytail: Java import paths (com.example.math) don't match file-path-based
// qnames in the resolver for v1. Cross-file resolution relies on SameFile + global-unique.
// Upgrade path: resolver learns Java package-to-path mapping.

use crate::languages::ts::ModuleMap;
use tree_sitter::Node;

/// Parse Java `import` declarations from the AST root.
/// `import java.util.List;` → local_name="List", path="java.util.List"
/// `import com.example.*;` → local_name="com.example", path="com.example" (wildcard)
/// `import static com.example.Math.PI;` → local_name="PI", path="com.example.Math.PI"
pub fn parse_imports(root: &Node, source: &str, map: &mut ModuleMap) {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "import_declaration" {
            parse_import(&child, source, map);
        }
    }
}

fn parse_import(node: &Node, source: &str, map: &mut ModuleMap) {
    // import_declaration has no named fields, only children:
    // - `import` keyword
    // - optional `static` keyword (for static imports)
    // - `scoped_identifier` (the full path like java.util.List)
    // - optional `asterisk` (for wildcard imports like java.util.*)
    // - `;` punctuation
    let mut cursor = node.walk();
    let scoped = node
        .children(&mut cursor)
        .find(|c| c.kind() == "scoped_identifier");

    let Some(scoped) = scoped else {
        return;
    };
    let path = node_text(&scoped, source);

    // Check for wildcard: sibling asterisk or path ends with .*
    let mut cursor = node.walk();
    let has_wildcard = node.children(&mut cursor).any(|c| c.kind() == "asterisk");

    let local_name = if has_wildcard {
        // For `import java.util.*;` → local_name = "java.util" (package, not class)
        path.trim_end_matches(".*").to_string()
    } else {
        // Last component after last dot
        path.rsplit('.').next().unwrap_or(&path).to_string()
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
        let tree = parser::parse(code, Language::Java).unwrap();
        parse_imports(&tree.root_node(), code, map);
    }

    #[test]
    fn test_single_import() {
        let mut map = ModuleMap::default();
        parse_imports_code(
            r#"package com.example;
import java.util.List;
class Foo {}
"#,
            &mut map,
        );
        assert_eq!(map.imports.get("List"), Some(&"java.util.List".to_string()));
    }

    #[test]
    fn test_wildcard_import() {
        let mut map = ModuleMap::default();
        parse_imports_code(
            r#"package com.example;
import java.util.*;
class Foo {}
"#,
            &mut map,
        );
        assert_eq!(map.imports.get("java.util"), Some(&"java.util".to_string()));
    }

    #[test]
    fn test_multiple_imports() {
        let mut map = ModuleMap::default();
        parse_imports_code(
            r#"package com.example;
import java.util.List;
import com.example.math;
class Foo {}
"#,
            &mut map,
        );
        assert_eq!(map.imports.get("List"), Some(&"java.util.List".to_string()));
        assert_eq!(
            map.imports.get("math"),
            Some(&"com.example.math".to_string())
        );
    }
}

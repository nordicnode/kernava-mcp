// kernava-indexer: C# import parser
// ponytail: C# using directives (System, Example.Math) don't match file-path-based
// qnames in the resolver for v1. Cross-file resolution relies on SameFile + global-unique.
// Upgrade path: resolver learns C# namespace-to-path mapping.

use crate::languages::ts::ModuleMap;
use tree_sitter::Node;

/// Parse C# `using` directives from the AST root.
/// `using System;` → local_name="System", path="System"
/// `using System.Collections.Generic;` → local_name="Generic", path="System.Collections.Generic"
/// `using Math = System.Math;` → local_name="Math", path="System.Math" (alias)
pub fn parse_imports(root: &Node, source: &str, map: &mut ModuleMap) {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "using_directive" {
            parse_using_directive(&child, source, map);
        }
        // C# namespaces can be nested, recurse into namespace_declaration
        if child.kind() == "namespace_declaration" {
            let mut nc = child.walk();
            for nc_child in child.children(&mut nc) {
                if nc_child.kind() == "using_directive" {
                    parse_using_directive(&nc_child, source, map);
                }
            }
        }
    }
}

fn parse_using_directive(node: &Node, source: &str, map: &mut ModuleMap) {
    // using_directive children: either `using_directive` (name) or `using_alias_directive` (name + value)
    // For `using System;` — child is identifier or qualified_name
    // For `using System.Collections.Generic;` — child is qualified_name
    // For `using Math = System.Math;` — using_alias_directive with name + value fields

    // Try alias first: using X = Some.Namespace;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let (local_name, path) =
            if child.kind() == "using_alias_directive" || child.kind() == "using_directive" {
                // Alias: look for `name` and `value` fields
                let alias_name = child
                    .child_by_field_name("name")
                    .map(|n| node_text(&n, source))
                    .unwrap_or_default();
                let alias_value = child
                    .child_by_field_name("value")
                    .map(|n| node_text(&n, source))
                    .unwrap_or_default();
                if !alias_name.is_empty() && !alias_value.is_empty() {
                    (alias_name, alias_value)
                } else {
                    // Non-alias: full qualified name text
                    let raw = node_text(&child, source);
                    // Strip trailing ;
                    let raw = raw.trim_end_matches(';').trim();
                    let local = raw.rsplit('.').next().unwrap_or(raw).to_string();
                    (local, raw.to_string())
                }
            } else {
                // Direct child is the name node (identifier or qualified_name)
                let raw = node_text(&child, source);
                let raw = raw.trim_end_matches(';').trim();
                let local = raw.rsplit('.').next().unwrap_or(raw).to_string();
                (local, raw.to_string())
            };

        if !path.is_empty() {
            map.imports.insert(local_name, path.clone());
            map.module_paths.push(path);
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
        let tree = parser::parse(code, Language::CSharp).unwrap();
        parse_imports(&tree.root_node(), code, map);
    }

    #[test]
    fn test_single_using() {
        let mut map = ModuleMap::default();
        parse_imports_code(
            r#"using System;
"#,
            &mut map,
        );
        assert_eq!(map.imports.get("System"), Some(&"System".to_string()));
    }

    #[test]
    fn test_qualified_using() {
        let mut map = ModuleMap::default();
        parse_imports_code(
            r#"using System.Collections.Generic;
"#,
            &mut map,
        );
        assert_eq!(
            map.imports.get("Generic"),
            Some(&"System.Collections.Generic".to_string())
        );
    }

    #[test]
    fn test_multiple_using() {
        let mut map = ModuleMap::default();
        parse_imports_code(
            r#"using System;
using Example.Math;
"#,
            &mut map,
        );
        assert_eq!(map.imports.get("System"), Some(&"System".to_string()));
        assert_eq!(map.imports.get("Math"), Some(&"Example.Math".to_string()));
    }
}

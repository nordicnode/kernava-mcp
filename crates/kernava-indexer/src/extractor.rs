// kernava-indexer: symbol extraction from tree-sitter ASTs
// P1 task 1.6: TypeScript extractor — functions, methods, classes, interfaces, enums,
// type aliases, import statements, call expressions

use crate::languages::ModuleMap;
use crate::parser::Language;
use anyhow::Result;
use std::collections::VecDeque;
use tree_sitter::Node;

/// A symbol definition extracted from source code.
#[derive(Debug, Clone)]
pub struct SymbolDef {
    pub kind: SymbolKind,
    pub name: String,
    pub qualified_name: String,
    pub file_path: String,
    pub line_start: usize,
    pub line_end: usize,
    pub signature: Option<String>,
    pub return_type: Option<String>,
    pub receiver_type: Option<String>,
    pub is_exported: bool,
    pub complexity: u32,
    pub decorators: Vec<String>,
}

/// A call site extracted from source code.
#[derive(Debug, Clone)]
pub struct CallSite {
    /// The callee name as it appears in source (e.g., "foo", "obj.method", "Bar.baz")
    pub callee: String,
    /// Line where the call occurs
    pub line: usize,
    /// The containing function's qualified name (if inside a function)
    pub caller_qualified: Option<String>,
    /// Column where the call starts
    pub col: usize,
}

/// What the indexer extracted from a single file.
#[derive(Debug, Clone, Default)]
pub struct ExtractionResult {
    pub symbols: Vec<SymbolDef>,
    pub calls: Vec<CallSite>,
    pub module_map: ModuleMap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SymbolKind {
    Function,
    Method,
    Class,
    Interface,
    Enum,
    TypeAlias,
    Variable, // const arrow functions
}

impl SymbolKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Method => "method",
            Self::Class => "class",
            Self::Interface => "interface",
            Self::Enum => "enum",
            Self::TypeAlias => "type",
            Self::Variable => "variable",
        }
    }
}

/// Extract all symbols, calls, and imports from a source file.
pub fn extract(source: &str, lang: Language, file_path: &str) -> Result<ExtractionResult> {
    let tree = crate::parser::parse(source, lang)?;
    let root = tree.root_node();

    let mut result = ExtractionResult::default();

    // Extract imports first (needed for call resolution later)
    extract_imports(&root, source, lang, &mut result.module_map);

    // Walk the AST iteratively, extracting symbols and calls.
    // Iterative (work-stack) instead of recursive: avoids stack overflow on
    // deeply-nested ASTs (C/C++ preprocessor-heavy headers can produce trees
    // hundreds of levels deep). Each level is a work item on the heap, not a
    // native stack frame.
    let mut work: VecDeque<(Node<'_>, Option<String>)> = VecDeque::new();
    work.push_back((root, None));
    while let Some((node, parent_symbol)) = work.pop_front() {
        walk_one(
            &node,
            source,
            file_path,
            &parent_symbol,
            lang,
            &mut result,
            &mut work,
        );
    }

    Ok(result)
}

/// Process one AST node: emit its symbol/call records, then push the children
/// that should be visited next onto `work`. Iterative equivalent of the old
/// recursive `walk` — no native recursion, so unbounded AST depth is safe.
fn walk_one<'t>(
    node: &Node<'t>,
    source: &str,
    file_path: &str,
    parent_symbol: &Option<String>,
    lang: Language,
    result: &mut ExtractionResult,
    work: &mut VecDeque<(Node<'t>, Option<String>)>,
) {
    match node.kind() {
        // ── TS/JS: function declarations ──
        "function_declaration" if lang.is_ts_family() => {
            if let Some(sym) = extract_function(node, source, file_path, parent_symbol.as_deref()) {
                let qn = sym.qualified_name.clone();
                result.symbols.push(sym);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    work.push_back((child, Some(qn.clone())));
                }
                return;
            }
        }
        // ── Python: function definitions ──
        "function_definition" if lang == Language::Python => {
            if let Some(sym) = extract_function(node, source, file_path, parent_symbol.as_deref()) {
                let qn = sym.qualified_name.clone();
                result.symbols.push(sym);
                // Walk the body (skip parameters/return type to avoid false calls)
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "block" {
                        work.push_back((child, Some(qn.clone())));
                    }
                }
                return;
            }
        }
        // ── TS/JS: class declarations ──
        "class_declaration" if lang.is_ts_family() => {
            if let Some(sym) = extract_class(node, source, file_path, parent_symbol.as_deref()) {
                let qn = sym.qualified_name.clone();
                result.symbols.push(sym);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "class_body" {
                        let mut body_cursor = child.walk();
                        for member in child.children(&mut body_cursor) {
                            walk_method(&member, source, file_path, &qn, lang, result, work, false);
                        }
                    }
                }
                return;
            }
        }
        // ── Python: class definitions ──
        "class_definition" if lang == Language::Python => {
            if let Some(sym) = extract_class(node, source, file_path, parent_symbol.as_deref()) {
                let qn = sym.qualified_name.clone();
                result.symbols.push(sym);
                // Python class body is a `block` containing function_definition nodes
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "block" {
                        let mut body_cursor = child.walk();
                        for member in child.children(&mut body_cursor) {
                            // Python methods are function_definition inside class block
                            if matches!(
                                member.kind(),
                                "function_definition" | "decorated_definition"
                            ) {
                                walk_method(
                                    &member, source, file_path, &qn, lang, result, work, false,
                                );
                            }
                        }
                    }
                }
                return;
            }
        }
        // ── Rust: free functions ──
        "function_item" if lang.is_rust() => {
            // Walk back through preceding attribute_item siblings to find
            // #[test] / #[tokio::test]. Attribute chains like #[cfg(test)]
            // \n#[test] or #[ignore]\n#[test] are valid — the immediate
            // prev_sibling may not be #[test]. Stop at first non-attribute.
            // Test functions are entry points (invoked by harness, not via
            // call edges) — treat as exported, matching has_rust_attrs for
            // impl methods (line ~943).
            let mut has_test_attr = false;
            let mut prev = node.prev_sibling();
            while let Some(attr) = prev {
                if attr.kind() != "attribute_item" {
                    break;
                }
                let t = node_text(&attr, source);
                if t.contains("#[test") || t.contains("#[tokio::test") {
                    has_test_attr = true;
                    break;
                }
                prev = attr.prev_sibling();
            }
            if let Some(mut sym) =
                extract_function(node, source, file_path, parent_symbol.as_deref())
            {
                if has_test_attr {
                    sym.is_exported = true;
                }
                let qn = sym.qualified_name.clone();
                result.symbols.push(sym);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "block" {
                        work.push_back((child, Some(qn.clone())));
                    }
                }
                return;
            }
        }
        // ── Rust: impl blocks ──
        "impl_item" if lang.is_rust() => {
            // impl blocks have a `body` field containing the methods
            // For `impl Type`, `type` field = Type. For `impl Trait for Type`, both exist.
            // Methods belong to the implementing type, so prefer `type` over `trait`.
            let impl_type = node
                .child_by_field_name("type")
                .or_else(|| node.child_by_field_name("trait"))
                .map(|n| node_text(&n, source))
                .unwrap_or_default();
            // Qualify with file_path so method qnames match the struct's qname
            let impl_qn = format!("{}.{}", file_path, impl_type);
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "declaration_list" {
                    let mut body_cursor = child.walk();
                    let mut has_attrs = false;
                    for member in child.children(&mut body_cursor) {
                        // Track attribute_items so walk_method knows if the
                        // method is annotated with #[tool(...)] etc.
                        if member.kind() == "attribute_item" {
                            has_attrs = true;
                            continue;
                        }
                        if member.kind() == "function_item"
                            || member.kind() == "function_signature_item"
                        {
                            walk_method(
                                &member, source, file_path, &impl_qn, lang, result, work, has_attrs,
                            );
                            has_attrs = false;
                        } else {
                            has_attrs = false;
                        }
                    }
                }
            }
            return;
        }
        // ── Rust: struct ──
        "struct_item" if lang.is_rust() => {
            if let Some(sym) = extract_class(node, source, file_path, parent_symbol.as_deref()) {
                result.symbols.push(sym);
                return;
            }
        }
        // ── Rust: enum ──
        "enum_item" if lang.is_rust() => {
            if let Some(sym) = extract_enum(node, source, file_path, parent_symbol.as_deref()) {
                result.symbols.push(sym);
                return;
            }
        }
        // ── Rust: trait ──
        "trait_item" if lang.is_rust() => {
            if let Some(sym) = extract_class(node, source, file_path, parent_symbol.as_deref()) {
                let qn = sym.qualified_name.clone();
                result.symbols.push(sym);
                // Walk trait body for method signatures + default impls
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "declaration_list" {
                        let mut body_cursor = child.walk();
                        for member in child.children(&mut body_cursor) {
                            walk_method(&member, source, file_path, &qn, lang, result, work, false);
                        }
                    }
                }
                return;
            }
        }
        // ── Go: free functions ──
        "function_declaration" if lang.is_go() => {
            if let Some(mut sym) =
                extract_function(node, source, file_path, parent_symbol.as_deref())
            {
                sym.is_exported = is_go_exported(&sym.name);
                let qn = sym.qualified_name.clone();
                result.symbols.push(sym);
                // Walk the body (block) for calls
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "block" {
                        work.push_back((child, Some(qn.clone())));
                    }
                }
                return;
            }
        }
        // ── Go: method declarations (with receiver) ──
        "method_declaration" if lang.is_go() => {
            // Receiver: (c Calculator) — field "receiver" → parameter_list → parameter_declaration
            let receiver_type = extract_go_receiver_type(node, source);

            if let Some(sym) = extract_go_method(node, source, file_path, &receiver_type) {
                let qn = sym.qualified_name.clone();
                result.symbols.push(sym);
                // Walk body for calls
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "block" {
                        work.push_back((child, Some(qn.clone())));
                    }
                }
                return;
            }
        }
        // ── Go: type declarations (struct, interface, etc.) ──
        "type_declaration" if lang.is_go() => {
            // type_declaration wraps one or more type_spec nodes
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "type_spec" {
                    if let Some(sym) = extract_go_type_spec(&child, source, file_path) {
                        result.symbols.push(sym);
                    }
                }
            }
            return;
        }
        // ── Python: decorated definitions (wrapper) ──
        "decorated_definition" if lang == Language::Python => {
            // decorated_definition wraps function_definition/class_definition
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "function_definition" || child.kind() == "class_definition" {
                    work.push_back((child, parent_symbol.clone()));
                }
            }
            return;
        }
        // ── Java: class declaration ──
        "class_declaration" if lang.is_java() => {
            if let Some(sym) = extract_class(node, source, file_path, parent_symbol.as_deref()) {
                let qn = sym.qualified_name.clone();
                result.symbols.push(sym);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "class_body" {
                        let mut body_cursor = child.walk();
                        for member in child.children(&mut body_cursor) {
                            walk_method(&member, source, file_path, &qn, lang, result, work, false);
                        }
                    }
                }
                return;
            }
        }
        // ── Java: interface declaration ──
        "interface_declaration" if lang.is_java() => {
            if let Some(sym) = extract_interface(node, source, file_path, parent_symbol.as_deref())
            {
                let qn = sym.qualified_name.clone();
                result.symbols.push(sym);
                // Interface body has method signatures (no body)
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "interface_body" {
                        let mut body_cursor = child.walk();
                        for member in child.children(&mut body_cursor) {
                            walk_method(&member, source, file_path, &qn, lang, result, work, false);
                        }
                    }
                }
                return;
            }
        }
        // ── Java: enum declaration ──
        "enum_declaration" if lang.is_java() => {
            if let Some(sym) = extract_enum(node, source, file_path, parent_symbol.as_deref()) {
                result.symbols.push(sym);
                return;
            }
        }
        // ── Java: free-standing method_declaration (shouldn't happen — Java methods are always in classes) ──
        // Java method_declaration inside class_body is handled by walk_method
        // ── Java: method invocation (calls) ──
        "method_invocation" if lang.is_java() => {
            let callee = extract_callee_name(node, source, lang);
            if let Some(callee) = callee {
                result.calls.push(CallSite {
                    callee,
                    line: node.start_position().row + 1,
                    caller_qualified: parent_symbol.clone(),
                    col: node.start_position().column,
                });
            }
            // Fall through to default recursion
        }
        // ── C#: class declaration ──
        "class_declaration" if lang.is_csharp() => {
            if let Some(sym) = extract_class(node, source, file_path, parent_symbol.as_deref()) {
                let qn = sym.qualified_name.clone();
                result.symbols.push(sym);
                // C# body is via `body` field → declaration_list
                if let Some(body) = node.child_by_field_name("body") {
                    let mut bc = body.walk();
                    for member in body.children(&mut bc) {
                        walk_method(&member, source, file_path, &qn, lang, result, work, false);
                    }
                }
                return;
            }
        }
        // ── C#: interface declaration ──
        "interface_declaration" if lang.is_csharp() => {
            if let Some(sym) = extract_interface(node, source, file_path, parent_symbol.as_deref())
            {
                let qn = sym.qualified_name.clone();
                result.symbols.push(sym);
                if let Some(body) = node.child_by_field_name("body") {
                    let mut bc = body.walk();
                    for member in body.children(&mut bc) {
                        walk_method(&member, source, file_path, &qn, lang, result, work, false);
                    }
                }
                return;
            }
        }
        // ── C#: enum declaration ──
        "enum_declaration" if lang.is_csharp() => {
            if let Some(sym) = extract_enum(node, source, file_path, parent_symbol.as_deref()) {
                result.symbols.push(sym);
                return;
            }
        }
        // ── C#: namespace declaration — recurse into children ──
        "namespace_declaration" if lang.is_csharp() => {
            // Recurse into the namespace body (declaration_list)
            if let Some(body) = node.child_by_field_name("body") {
                work.push_back((body, parent_symbol.clone()));
            }
            return;
        }
        // ── C#: invocation expression (calls) ──
        "invocation_expression" if lang.is_csharp() => {
            let callee = extract_callee_name(node, source, lang);
            if let Some(callee) = callee {
                result.calls.push(CallSite {
                    callee,
                    line: node.start_position().row + 1,
                    caller_qualified: parent_symbol.clone(),
                    col: node.start_position().column,
                });
            }
            // Fall through to default recursion
        }
        // ── Ruby: class ──
        "class" if lang.is_ruby() => {
            if let Some(sym) = extract_class(node, source, file_path, parent_symbol.as_deref()) {
                let qn = sym.qualified_name.clone();
                result.symbols.push(sym);
                if let Some(body) = node.child_by_field_name("body") {
                    let mut bc = body.walk();
                    for member in body.children(&mut bc) {
                        walk_method(&member, source, file_path, &qn, lang, result, work, false);
                    }
                }
                return;
            }
        }
        // ── Ruby: module ──
        "module" if lang.is_ruby() => {
            if let Some(sym) = extract_class(node, source, file_path, parent_symbol.as_deref()) {
                let qn = sym.qualified_name.clone();
                result.symbols.push(sym);
                if let Some(body) = node.child_by_field_name("body") {
                    let mut bc = body.walk();
                    for member in body.children(&mut bc) {
                        walk_method(&member, source, file_path, &qn, lang, result, work, false);
                    }
                }
                return;
            }
        }
        // ── Ruby: top-level method (free function) ──
        "method" if lang.is_ruby() && parent_symbol.is_none() => {
            if let Some(name) = get_child_text(node, "name", source) {
                let qn = make_qualified_name(file_path, &name, parent_symbol.as_deref());
                result.symbols.push(SymbolDef {
                    kind: SymbolKind::Function,
                    name,
                    qualified_name: qn.clone(),
                    file_path: file_path.to_string(),
                    line_start: node.start_position().row + 1,
                    line_end: node.end_position().row + 1,
                    signature: None,
                    return_type: None,
                    receiver_type: None,
                    is_exported: false,
                    complexity: count_complexity(node, source),
                    decorators: Vec::new(),
                });
                if let Some(body) = node.child_by_field_name("body") {
                    work.push_back((body, Some(qn.clone())));
                }
                return;
            }
        }
        // ── Ruby: call (function calls) ──
        "call" if lang.is_ruby() => {
            let callee = extract_callee_name(node, source, lang);
            if let Some(callee) = callee {
                result.calls.push(CallSite {
                    callee,
                    line: node.start_position().row + 1,
                    caller_qualified: parent_symbol.clone(),
                    col: node.start_position().column,
                });
            }
            // Fall through to default recursion
        }
        // ── PHP: class declaration ──
        "class_declaration" if lang.is_php() => {
            if let Some(sym) = extract_class(node, source, file_path, parent_symbol.as_deref()) {
                let qn = sym.qualified_name.clone();
                result.symbols.push(sym);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "declaration_list" {
                        let mut bc = child.walk();
                        for member in child.children(&mut bc) {
                            walk_method(&member, source, file_path, &qn, lang, result, work, false);
                        }
                    }
                }
                return;
            }
        }
        // ── PHP: interface declaration ──
        "interface_declaration" if lang.is_php() => {
            if let Some(sym) = extract_interface(node, source, file_path, parent_symbol.as_deref())
            {
                let qn = sym.qualified_name.clone();
                result.symbols.push(sym);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "declaration_list" {
                        let mut bc = child.walk();
                        for member in child.children(&mut bc) {
                            walk_method(&member, source, file_path, &qn, lang, result, work, false);
                        }
                    }
                }
                return;
            }
        }
        // ── PHP: free function definition ──
        "function_definition" if lang.is_php() => {
            if let Some(sym) = extract_function(node, source, file_path, parent_symbol.as_deref()) {
                let qn = sym.qualified_name.clone();
                result.symbols.push(sym);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "compound_statement" {
                        work.push_back((child, Some(qn.clone())));
                    }
                }
                return;
            }
        }
        // ── PHP: function call expression ──
        "function_call_expression" if lang.is_php() => {
            let callee = extract_callee_name(node, source, lang);
            if let Some(callee) = callee {
                result.calls.push(CallSite {
                    callee,
                    line: node.start_position().row + 1,
                    caller_qualified: parent_symbol.clone(),
                    col: node.start_position().column,
                });
            }
            // Fall through to default recursion
        }
        // ── C/C++: function definition (free function) ──
        "function_definition" if lang.is_c_family() => {
            let name = extract_c_function_name(node, source);
            if let Some(name) = name {
                let qn = make_qualified_name(file_path, &name, parent_symbol.as_deref());
                result.symbols.push(SymbolDef {
                    kind: SymbolKind::Function,
                    name: name.clone(),
                    qualified_name: qn.clone(),
                    file_path: file_path.to_string(),
                    line_start: node.start_position().row + 1,
                    line_end: node.end_position().row + 1,
                    signature: None,
                    return_type: None,
                    receiver_type: None,
                    is_exported: false,
                    complexity: count_complexity(node, source),
                    decorators: Vec::new(),
                });
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "compound_statement" {
                        work.push_back((child, Some(qn.clone())));
                    }
                }
                return;
            }
        }
        // ── C/C++: call expression ──
        "call_expression" if lang.is_c_family() => {
            let callee = extract_callee_name(node, source, lang);
            if let Some(callee) = callee {
                result.calls.push(CallSite {
                    callee,
                    line: node.start_position().row + 1,
                    caller_qualified: parent_symbol.clone(),
                    col: node.start_position().column,
                });
            }
            // Fall through to default recursion
        }
        // ── C++ only: class specifier ──
        "class_specifier" if lang == Language::Cpp => {
            if let Some(sym) = extract_class(node, source, file_path, parent_symbol.as_deref()) {
                let qn = sym.qualified_name.clone();
                result.symbols.push(sym);
                // class_specifier body is field_declaration_list
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "field_declaration_list" {
                        let mut fc = child.walk();
                        for member in child.children(&mut fc) {
                            walk_method(&member, source, file_path, &qn, lang, result, work, false);
                        }
                    }
                }
                return;
            }
        }
        // ── C++ only: namespace definition — recurse ──
        "namespace_definition" if lang == Language::Cpp => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "declaration_list" || child.kind() == "compound_statement" {
                    work.push_back((child, parent_symbol.clone()));
                }
            }
            return;
        }
        // ── PHP: member call expression ($obj->method()) ──
        "member_call_expression" if lang.is_php() => {
            let callee = extract_callee_name(node, source, lang);
            if let Some(callee) = callee {
                result.calls.push(CallSite {
                    callee,
                    line: node.start_position().row + 1,
                    caller_qualified: parent_symbol.clone(),
                    col: node.start_position().column,
                });
            }
            // Fall through to default recursion
        }
        // ── C/C++: struct/union specifier → Class ──
        "struct_specifier" | "union_specifier" if lang.is_c_family() => {
            if let Some(name) = get_child_text(node, "name", source) {
                let qn = make_qualified_name(file_path, &name, parent_symbol.as_deref());
                result.symbols.push(SymbolDef {
                    kind: SymbolKind::Class,
                    name: name.clone(),
                    qualified_name: qn.clone(),
                    file_path: file_path.to_string(),
                    line_start: node.start_position().row + 1,
                    line_end: node.end_position().row + 1,
                    signature: None,
                    return_type: None,
                    receiver_type: None,
                    is_exported: false,
                    complexity: 0,
                    decorators: Vec::new(),
                });
                // Walk field_declaration_list for methods (C++ only — C has no methods)
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "field_declaration_list" {
                        let mut fc = child.walk();
                        for member in child.children(&mut fc) {
                            walk_method(&member, source, file_path, &qn, lang, result, work, false);
                        }
                    }
                }
            }
            return;
        }
        // ── C/C++: enum specifier → Enum ──
        "enum_specifier" if lang.is_c_family() => {
            if let Some(name) = get_child_text(node, "name", source) {
                let qn = make_qualified_name(file_path, &name, parent_symbol.as_deref());
                result.symbols.push(SymbolDef {
                    kind: SymbolKind::Enum,
                    name,
                    qualified_name: qn,
                    file_path: file_path.to_string(),
                    line_start: node.start_position().row + 1,
                    line_end: node.end_position().row + 1,
                    signature: None,
                    return_type: None,
                    receiver_type: None,
                    is_exported: false,
                    complexity: 0,
                    decorators: Vec::new(),
                });
            }
            return;
        }
        // ── TS-only: interface/enum/type_alias ──
        "interface_declaration" if lang.is_ts_family() => {
            if let Some(sym) = extract_interface(node, source, file_path, parent_symbol.as_deref())
            {
                result.symbols.push(sym);
                return;
            }
        }
        "enum_declaration" if lang.is_ts_family() => {
            if let Some(sym) = extract_enum(node, source, file_path, parent_symbol.as_deref()) {
                result.symbols.push(sym);
                return;
            }
        }
        "type_alias_declaration" if lang.is_ts_family() => {
            if let Some(sym) = extract_type_alias(node, source, file_path, parent_symbol.as_deref())
            {
                result.symbols.push(sym);
                return;
            }
        }
        // Export statements may wrap a declaration
        "export_statement" if lang.is_ts_family() => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                work.push_back((child, parent_symbol.clone()));
            }
            return;
        }
        // Variable declarations: `const foo = () => {}` or `const foo = function() {}`
        "lexical_declaration" | "variable_declaration" if lang.is_ts_family() => {
            extract_variable(
                node,
                source,
                file_path,
                parent_symbol.as_deref(),
                lang,
                result,
                work,
            );
            return;
        }
        // ── TS/JS: call expressions ──
        "call_expression" if lang.is_ts_family() => {
            let callee = extract_callee_name(node, source, lang);
            if let Some(callee) = callee {
                result.calls.push(CallSite {
                    callee,
                    line: node.start_position().row + 1,
                    caller_qualified: parent_symbol.clone(),
                    col: node.start_position().column,
                });
            }
            // Fall through to default recursion to collect nested calls in arguments
        }
        // ── Python: call ──
        "call" if lang == Language::Python => {
            let callee = extract_callee_name(node, source, lang);
            if let Some(callee) = callee {
                result.calls.push(CallSite {
                    callee,
                    line: node.start_position().row + 1,
                    caller_qualified: parent_symbol.clone(),
                    col: node.start_position().column,
                });
            }
            // Fall through to default recursion to collect nested calls in arguments
        }
        // ── Go: call expressions ──
        "call_expression" if lang.is_go() => {
            let callee = extract_callee_name(node, source, lang);
            if let Some(callee) = callee {
                result.calls.push(CallSite {
                    callee,
                    line: node.start_position().row + 1,
                    caller_qualified: parent_symbol.clone(),
                    col: node.start_position().column,
                });
            }
            // Fall through to default recursion to collect nested calls in arguments
        }
        // ── Rust: call expressions ──
        "call_expression" if lang.is_rust() => {
            let callee = extract_callee_name(node, source, lang);
            if let Some(callee) = callee {
                result.calls.push(CallSite {
                    callee,
                    line: node.start_position().row + 1,
                    caller_qualified: parent_symbol.clone(),
                    col: node.start_position().column,
                });
            }
            // Fall through to default recursion to collect nested calls in arguments
        }
        _ => {}
    }

    // Recurse into children for any node type we didn't handle (or that fell through)
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        work.push_back((child, parent_symbol.clone()));
    }
}

/// Extract a function_declaration or function_definition node.
fn extract_function(
    node: &Node,
    source: &str,
    file_path: &str,
    parent: Option<&str>,
) -> Option<SymbolDef> {
    let name = get_child_text(node, "name", source)?;
    let qualified_name = make_qualified_name(file_path, &name, parent);

    let signature = get_signature(node, source);
    let return_type = get_return_type(node, source);

    Some(SymbolDef {
        kind: SymbolKind::Function,
        name,
        qualified_name,
        file_path: file_path.to_string(),
        line_start: node.start_position().row + 1,
        line_end: node.end_position().row + 1,
        signature,
        return_type,
        receiver_type: None,
        is_exported: is_exported(node, source),
        complexity: count_complexity(node, source),
        decorators: get_decorators(node, source),
    })
}

/// Walk a class body member, extracting methods.
#[allow(clippy::too_many_arguments)]
fn walk_method<'t>(
    node: &Node<'t>,
    source: &str,
    file_path: &str,
    class_qn: &str,
    lang: Language,
    result: &mut ExtractionResult,
    work: &mut VecDeque<(Node<'t>, Option<String>)>,
    has_rust_attrs: bool,
) {
    // Python: decorated_definition wraps function_definition — recurse into it
    if lang == Language::Python && node.kind() == "decorated_definition" {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "function_definition" {
                walk_method(
                    &child, source, file_path, class_qn, lang, result, work, false,
                );
            }
        }
        return;
    }

    // Python: function_definition inside class block = method
    if lang == Language::Python && node.kind() == "function_definition" {
        if let Some(name) = get_child_text(node, "name", source) {
            let qualified_name = format!("{}.{}", class_qn, name);
            let signature = get_signature(node, source);

            result.symbols.push(SymbolDef {
                kind: SymbolKind::Method,
                name,
                qualified_name: qualified_name.clone(),
                file_path: file_path.to_string(),
                line_start: node.start_position().row + 1,
                line_end: node.end_position().row + 1,
                signature,
                return_type: get_return_type(node, source),
                receiver_type: Some(class_qn.to_string()),
                is_exported: false,
                complexity: count_complexity(node, source),
                decorators: get_decorators(node, source),
            });

            // Walk the block for calls
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "block" {
                    work.push_back((child, Some(qualified_name.clone())));
                }
            }
        }
        return;
    }

    // TS/JS: method_definition
    if lang.is_ts_family() && node.kind() == "method_definition" {
        if let Some(name) = get_child_text(node, "name", source) {
            let qualified_name = format!("{}.{}", class_qn, name);
            let signature = get_signature(node, source);
            let return_type = get_return_type(node, source);

            result.symbols.push(SymbolDef {
                kind: SymbolKind::Method,
                name,
                qualified_name: qualified_name.clone(),
                file_path: file_path.to_string(),
                line_start: node.start_position().row + 1,
                line_end: node.end_position().row + 1,
                signature,
                return_type,
                receiver_type: Some(class_qn.to_string()),
                is_exported: false,
                complexity: count_complexity(node, source),
                decorators: get_decorators(node, source),
            });

            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "statement_block" {
                    work.push_back((child, Some(qualified_name.clone())));
                }
            }
        }
    }

    // Rust: function_item inside impl/trait = method
    if lang.is_rust() && node.kind() == "function_item" {
        if let Some(name) = get_child_text(node, "name", source) {
            let qualified_name = format!("{}.{}", class_qn, name);
            let signature = get_signature(node, source);
            let return_type = get_return_type(node, source);

            // A method with attributes (e.g. #[tool(...)]) is an entry point —
            // it's invoked by macros, not through direct call edges the
            // indexer can capture. Treat it as exported so detect_dead_code
            // doesn't report false positives.
            let exported = is_exported(node, source) || has_rust_attrs;

            result.symbols.push(SymbolDef {
                kind: SymbolKind::Method,
                name,
                qualified_name: qualified_name.clone(),
                file_path: file_path.to_string(),
                line_start: node.start_position().row + 1,
                line_end: node.end_position().row + 1,
                signature,
                return_type,
                receiver_type: Some(class_qn.to_string()),
                is_exported: exported,
                complexity: count_complexity(node, source),
                decorators: Vec::new(),
            });

            // Walk the block for calls
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "block" {
                    work.push_back((child, Some(qualified_name.clone())));
                }
            }
        }
        return;
    }

    // Rust: function_signature_item (trait method declaration, no body)
    if lang.is_rust() && node.kind() == "function_signature_item" {
        if let Some(name) = get_child_text(node, "name", source) {
            let qualified_name = format!("{}.{}", class_qn, name);
            let signature = get_signature(node, source);
            let return_type = get_return_type(node, source);

            let exported = is_exported(node, source) || has_rust_attrs;

            result.symbols.push(SymbolDef {
                kind: SymbolKind::Method,
                name,
                qualified_name,
                file_path: file_path.to_string(),
                line_start: node.start_position().row + 1,
                line_end: node.end_position().row + 1,
                signature,
                return_type,
                receiver_type: Some(class_qn.to_string()),
                is_exported: exported,
                complexity: 1,
                decorators: Vec::new(),
            });
        }
        return;
    }
    // Java: method_declaration inside class_body = method
    if lang.is_java()
        && (node.kind() == "method_declaration" || node.kind() == "constructor_declaration")
    {
        if let Some(name) = get_child_text(node, "name", source) {
            let qualified_name = format!("{}.{}", class_qn, name);
            let signature = get_signature(node, source);

            result.symbols.push(SymbolDef {
                kind: SymbolKind::Method,
                name,
                qualified_name: qualified_name.clone(),
                file_path: file_path.to_string(),
                line_start: node.start_position().row + 1,
                line_end: node.end_position().row + 1,
                signature,
                return_type: get_return_type(node, source),
                receiver_type: Some(class_qn.to_string()),
                is_exported: false,
                complexity: count_complexity(node, source),
                decorators: Vec::new(),
            });

            // Walk the block for calls
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "block" {
                    work.push_back((child, Some(qualified_name.clone())));
                }
            }
        }
        return;
    }
    // C#: method_declaration inside class body = method
    if lang.is_csharp()
        && (node.kind() == "method_declaration" || node.kind() == "constructor_declaration")
    {
        if let Some(name) = get_child_text(node, "name", source) {
            let qualified_name = format!("{}.{}", class_qn, name);
            let signature = get_signature(node, source);

            result.symbols.push(SymbolDef {
                kind: SymbolKind::Method,
                name,
                qualified_name: qualified_name.clone(),
                file_path: file_path.to_string(),
                line_start: node.start_position().row + 1,
                line_end: node.end_position().row + 1,
                signature,
                return_type: get_return_type(node, source),
                receiver_type: Some(class_qn.to_string()),
                is_exported: false,
                complexity: count_complexity(node, source),
                decorators: Vec::new(),
            });

            // Walk the block for calls
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "block" {
                    work.push_back((child, Some(qualified_name.clone())));
                }
            }
        }
        return;
    }
    // Ruby: method inside class/module = method
    if lang.is_ruby() && node.kind() == "method" {
        if let Some(name) = get_child_text(node, "name", source) {
            let qualified_name = format!("{}.{}", class_qn, name);

            result.symbols.push(SymbolDef {
                kind: SymbolKind::Method,
                name,
                qualified_name: qualified_name.clone(),
                file_path: file_path.to_string(),
                line_start: node.start_position().row + 1,
                line_end: node.end_position().row + 1,
                signature: None,
                return_type: None,
                receiver_type: Some(class_qn.to_string()),
                is_exported: false,
                complexity: count_complexity(node, source),
                decorators: Vec::new(),
            });

            // Walk the body field only — avoid walking parameters/name
            if let Some(body) = node.child_by_field_name("body") {
                work.push_back((body, Some(qualified_name.clone())));
            }
        }
        return;
    }
    // PHP: method_declaration inside class body = method
    if lang.is_php() && node.kind() == "method_declaration" {
        if let Some(name) = get_child_text(node, "name", source) {
            let qualified_name = format!("{}.{}", class_qn, name);
            let signature = get_signature(node, source);

            result.symbols.push(SymbolDef {
                kind: SymbolKind::Method,
                name,
                qualified_name: qualified_name.clone(),
                file_path: file_path.to_string(),
                line_start: node.start_position().row + 1,
                line_end: node.end_position().row + 1,
                signature,
                return_type: None,
                receiver_type: Some(class_qn.to_string()),
                is_exported: false,
                complexity: count_complexity(node, source),
                decorators: Vec::new(),
            });

            // Walk the body for calls
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "compound_statement" {
                    work.push_back((child, Some(qualified_name.clone())));
                }
            }
        }
        return;
    }
    // C++: function_definition inside class_specifier = method
    if lang == Language::Cpp && node.kind() == "function_definition" {
        // C++ function name is inside declarator → function_declarator → identifier
        let name = extract_c_function_name(node, source);
        if let Some(name) = name {
            let qualified_name = format!("{}.{}", class_qn, name);

            result.symbols.push(SymbolDef {
                kind: SymbolKind::Method,
                name,
                qualified_name: qualified_name.clone(),
                file_path: file_path.to_string(),
                line_start: node.start_position().row + 1,
                line_end: node.end_position().row + 1,
                signature: None,
                return_type: None,
                receiver_type: Some(class_qn.to_string()),
                is_exported: false,
                complexity: count_complexity(node, source),
                decorators: Vec::new(),
            });

            // Walk the body for calls
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "compound_statement" {
                    work.push_back((child, Some(qualified_name.clone())));
                }
            }
        }
    }
}

/// Extract a class_declaration node.
fn extract_class(
    node: &Node,
    source: &str,
    file_path: &str,
    parent: Option<&str>,
) -> Option<SymbolDef> {
    let name = get_child_text(node, "name", source)?;
    let qualified_name = make_qualified_name(file_path, &name, parent);

    Some(SymbolDef {
        kind: SymbolKind::Class,
        name,
        qualified_name,
        file_path: file_path.to_string(),
        line_start: node.start_position().row + 1,
        line_end: node.end_position().row + 1,
        signature: None,
        return_type: None,
        receiver_type: None,
        is_exported: is_exported(node, source),
        complexity: 0,
        decorators: get_decorators(node, source),
    })
}

/// Extract an interface_declaration node.
fn extract_interface(
    node: &Node,
    source: &str,
    file_path: &str,
    parent: Option<&str>,
) -> Option<SymbolDef> {
    let name = get_child_text(node, "name", source)?;
    let qualified_name = make_qualified_name(file_path, &name, parent);

    Some(SymbolDef {
        kind: SymbolKind::Interface,
        name,
        qualified_name,
        file_path: file_path.to_string(),
        line_start: node.start_position().row + 1,
        line_end: node.end_position().row + 1,
        signature: None,
        return_type: None,
        receiver_type: None,
        is_exported: is_exported(node, source),
        complexity: 0,
        decorators: Vec::new(),
    })
}

/// Extract an enum_declaration node.
fn extract_enum(
    node: &Node,
    source: &str,
    file_path: &str,
    parent: Option<&str>,
) -> Option<SymbolDef> {
    let name = get_child_text(node, "name", source)?;
    let qualified_name = make_qualified_name(file_path, &name, parent);

    Some(SymbolDef {
        kind: SymbolKind::Enum,
        name,
        qualified_name,
        file_path: file_path.to_string(),
        line_start: node.start_position().row + 1,
        line_end: node.end_position().row + 1,
        signature: None,
        return_type: None,
        receiver_type: None,
        is_exported: is_exported(node, source),
        complexity: 0,
        decorators: Vec::new(),
    })
}

/// Extract a type_alias_declaration node.
fn extract_type_alias(
    node: &Node,
    source: &str,
    file_path: &str,
    parent: Option<&str>,
) -> Option<SymbolDef> {
    let name = get_child_text(node, "name", source)?;
    let qualified_name = make_qualified_name(file_path, &name, parent);

    Some(SymbolDef {
        kind: SymbolKind::TypeAlias,
        name,
        qualified_name,
        file_path: file_path.to_string(),
        line_start: node.start_position().row + 1,
        line_end: node.end_position().row + 1,
        signature: None,
        return_type: None,
        receiver_type: None,
        is_exported: is_exported(node, source),
        complexity: 0,
        decorators: Vec::new(),
    })
}

/// Extract variable declarations that are arrow functions or function expressions.
fn extract_variable<'t>(
    node: &Node<'t>,
    source: &str,
    file_path: &str,
    parent: Option<&str>,
    // `lang` is part of the iterative-walk plumbing symmetry but isn't read
    // by this helper (variable/arrow extraction is language-agnostic via the
    // tree-sitter node kinds). Kept to preserve the caller-side signature
    // shape; mark `_lang` to silence the unused-variable warning.
    _lang: Language,
    result: &mut ExtractionResult,
    work: &mut VecDeque<(Node<'t>, Option<String>)>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "variable_declarator" {
            let name = match get_child_text(&child, "name", source) {
                Some(n) => n,
                None => continue,
            };
            // Check if the value is an arrow_function or function_expression
            let mut val_cursor = child.walk();
            let mut is_arrow_or_fn = false;
            for val_child in child.children(&mut val_cursor) {
                if val_child.kind() == "arrow_function" || val_child.kind() == "function_expression"
                {
                    is_arrow_or_fn = true;

                    let qualified_name = make_qualified_name(file_path, &name, parent);
                    let mut vc = val_child.walk();
                    for vc_child in val_child.children(&mut vc) {
                        work.push_back((vc_child, Some(qualified_name.clone())));
                    }
                } else {
                    work.push_back((val_child, parent.map(|s| s.to_string())));
                }
            }

            if is_arrow_or_fn {
                let qualified_name = make_qualified_name(file_path, &name, parent);
                result.symbols.push(SymbolDef {
                    kind: SymbolKind::Variable,
                    name,
                    qualified_name: qualified_name.clone(),
                    file_path: file_path.to_string(),
                    line_start: node.start_position().row + 1,
                    line_end: node.end_position().row + 1,
                    signature: None,
                    return_type: None,
                    receiver_type: None,
                    is_exported: is_exported(node, source),
                    complexity: count_complexity(node, source),
                    decorators: Vec::new(),
                });
            }
        }
    }
}

/// Extract function name from C/C++ function_definition node.
/// C/C++ function name is inside declarator → function_declarator → identifier field.
fn extract_c_function_name(node: &Node, source: &str) -> Option<String> {
    let declarator = node.child_by_field_name("declarator")?;
    if declarator.kind() == "function_declarator" {
        let inner = declarator.child_by_field_name("declarator")?;
        if inner.kind() == "identifier" {
            return Some(node_text(&inner, source));
        }
        if inner.kind() == "scoped_identifier" {
            let full = node_text(&inner, source);
            return Some(full.rsplit("::").next().unwrap_or(&full).to_string());
        }
        return Some(node_text(&inner, source));
    }
    if declarator.kind() == "pointer_declarator" {
        let inner = declarator.child_by_field_name("declarator")?;
        if inner.kind() == "function_declarator" {
            let name = inner.child_by_field_name("declarator")?;
            if name.kind() == "identifier" {
                return Some(node_text(&name, source));
            }
        }
    }
    if declarator.kind() == "identifier" {
        return Some(node_text(&declarator, source));
    }
    None
}

/// Extract the callee name from a call_expression or call node.
/// Handles: foo(), obj.method(), a.b.c()
/// TS/JS: call_expression with function→identifier or member_expression.
/// Python: call with function→identifier or attribute.
fn extract_callee_name(node: &Node, source: &str, lang: Language) -> Option<String> {
    // Python, Rust, Go, C/C++, C# use `function` field; Ruby uses `method` field;
    // PHP uses `function` or `member` field; Java uses `name`+`object` fields; TS/JS uses child(0)
    let function_node = if matches!(
        lang,
        Language::Python
            | Language::Rust
            | Language::Go
            | Language::C
            | Language::Cpp
            | Language::CSharp
    ) {
        node.child_by_field_name("function")?
    } else if lang.is_ruby() {
        node.child_by_field_name("method")?
    } else if lang.is_php() {
        // PHP: function_call_expression has `function` field; member_call_expression has `name` field
        node.child_by_field_name("function")
            .or_else(|| node.child_by_field_name("name"))?
    } else if lang.is_java() {
        // Java method_invocation: construct "object.name" from fields
        let name = node.child_by_field_name("name")?;
        let obj = node.child_by_field_name("object");
        return match obj {
            Some(o) => Some(format!(
                "{}.{}",
                node_text(&o, source),
                node_text(&name, source)
            )),
            None => Some(node_text(&name, source)),
        };
    } else {
        // TS/JS: first child is the callee
        node.child(0)?
    };

    match function_node.kind() {
        "identifier" | "name" => Some(node_text(&function_node, source)),
        "member_expression"
        | "attribute"
        | "field_expression"
        | "scoped_identifier"
        | "selector_expression"
        | "field_access"
        | "member_access_expression"
        | "qualified_name" => {
            let full = node_text(&function_node, source);
            Some(full)
        }
        _ => None,
    }
}

/// Get the text of a named child by field name.
fn get_child_text(node: &Node, field: &str, source: &str) -> Option<String> {
    let child = node.child_by_field_name(field)?;
    Some(node_text(&child, source))
}

/// Construct a qualified name: `file_path.symbol_name` or `file_path.owner.name`.
fn make_qualified_name(file_path: &str, name: &str, parent: Option<&str>) -> String {
    match parent {
        Some(p) => format!("{}.{}.{}", file_path, p, name),
        None => format!("{}.{}", file_path, name),
    }
}

/// Extract the function signature (parameters) from a function node.
fn get_signature(node: &Node, source: &str) -> Option<String> {
    let params = node.child_by_field_name("parameters")?;
    Some(node_text(&params, source))
}

/// Extract the return type annotation from a function node.
fn get_return_type(node: &Node, source: &str) -> Option<String> {
    // TS: return_type field (`: Type`). Rust: return_type field (`-> Type`). Go: result field.
    let return_type = node
        .child_by_field_name("return_type")
        .or_else(|| node.child_by_field_name("result"))?;
    let raw = node_text(&return_type, source);
    // TS: `: Type` — strip colon. Rust: `-> Type` — strip arrow. Go: raw type text.
    let stripped = raw.trim_start_matches("->").trim_start_matches(':').trim();
    Some(stripped.to_string())
}

/// Check if a node is exported (public API).
/// TS/JS: inside an export_statement.
/// Rust: has a `pub` visibility_modifier (not `pub(crate)`/`pub(super)` — those are crate-internal).
fn is_exported(node: &Node, source: &str) -> bool {
    // TS/JS: parent is export_statement
    if let Some(parent) = node.parent() {
        if parent.kind() == "export_statement" {
            return true;
        }
    }
    // Rust: scan children for visibility_modifier
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            let text = node_text(&child, source);
            // `pub` is exported; `pub(crate)` / `pub(super)` are crate-internal
            return !text.contains("(");
        }
    }
    false
}

/// Go: exported if name starts with uppercase letter.
fn is_go_exported(name: &str) -> bool {
    name.chars()
        .next()
        .map(|c| c.is_uppercase())
        .unwrap_or(false)
}
/// Count cyclomatic complexity: if, else if, for, while, switch case, &&, ||, ternary.
fn count_complexity(node: &Node, source: &str) -> u32 {
    // Score one node's kind for branching complexity. Pure (no recursion); the
    // iterative driver below walks the subtree with an explicit work stack.
    fn score(node: &Node, source: &str) -> u32 {
        match node.kind() {
            "if_statement" | "for_statement" | "for_in_statement" | "while_statement"
            | "do_statement" | "switch_case" | "ternary_expression" => 1,
            "elif_clause" | "conditional_expression" | "case_clause" => 1,
            // Rust: if_expression, for_expression, while_expression, match arms
            "if_expression" | "for_expression" | "while_expression" | "match_arm" => 1,
            // Ruby: unless, until, case
            "unless_statement" | "until_statement" | "when_clause" => 1,
            // PHP: elseif_clause
            "elseif_clause" => 1,
            "binary_expression" => {
                let op = node.child_by_field_name("operator");
                if let Some(op) = op {
                    let text = node_text(&op, source);
                    if text == "&&" || text == "||" {
                        1
                    } else {
                        0
                    }
                } else {
                    0
                }
            }
            // Python: boolean_operator (and/or)
            "boolean_operator" => {
                let op = node.child_by_field_name("operator");
                if let Some(op) = op {
                    let text = node_text(&op, source);
                    if text == "and" || text == "or" {
                        1
                    } else {
                        0
                    }
                } else {
                    0
                }
            }
            _ => 0,
        }
    }
    // Iterative subtree scan — no native recursion. Critical for deeply-nested
    // ASTs: a depth-5000 function body has a 5000-deep subtree, and recursive
    // count_node would overflow the stack on the single outermost call.
    let mut total: u32 = 0;
    let mut stack: VecDeque<Node> = VecDeque::new();
    {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push_back(child);
        }
    }
    while let Some(n) = stack.pop_front() {
        total = total.saturating_add(score(&n, source));
        let mut cursor = n.walk();
        for child in n.children(&mut cursor) {
            stack.push_back(child);
        }
    }
    1 + total
}

/// Extract decorators (TypeScript decorators are `@decorator` syntax).
fn get_decorators(node: &Node, source: &str) -> Vec<String> {
    let mut decorators = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "decorator" {
            decorators.push(node_text(&child, source));
        }
    }
    decorators
}

/// Get the text covered by a node.
fn node_text(node: &Node, source: &str) -> String {
    let start = node.start_byte();
    let end = node.end_byte();
    source[start..end].to_string()
}

/// Extract the receiver type name from a Go method_declaration.
/// `func (c Calculator) ...` → "Calculator"
/// `func (c *Calculator) ...` → "Calculator" (strip pointer)
fn extract_go_receiver_type(node: &Node, source: &str) -> String {
    let receiver = match node.child_by_field_name("receiver") {
        Some(r) => r,
        None => return String::new(),
    };
    // receiver → parameter_list → parameter_declaration → type field
    let mut cursor = receiver.walk();
    let pd = receiver
        .children(&mut cursor)
        .find(|ch| ch.kind() == "parameter_declaration");
    let Some(pd) = pd else {
        return String::new();
    };

    // type field gives the type node; for pointer receivers it's pointer_type wrapping type_identifier
    pd.child_by_field_name("type")
        .map(|t| {
            // Strip leading * for pointer receivers: `*Calculator` → `Calculator`
            node_text(&t, source).trim_start_matches('*').to_string()
        })
        .unwrap_or_default()
}

/// Extract a Go method_declaration (function with receiver).
/// receiver_type is the extracted type name from the receiver parameter.
fn extract_go_method(
    node: &Node,
    source: &str,
    file_path: &str,
    receiver_type: &str,
) -> Option<SymbolDef> {
    let name = get_child_text(node, "name", source)?;
    let qualified_name = if receiver_type.is_empty() {
        format!("{}.{}", file_path, name)
    } else {
        format!("{}.{}.{}", file_path, receiver_type, name)
    };
    let signature = get_signature(node, source);
    let return_type = get_return_type(node, source);
    let exported = is_go_exported(&name);

    Some(SymbolDef {
        kind: SymbolKind::Method,
        name,
        qualified_name,
        file_path: file_path.to_string(),
        line_start: node.start_position().row + 1,
        line_end: node.end_position().row + 1,
        signature,
        return_type,
        receiver_type: Some(receiver_type.to_string()),
        is_exported: exported,
        complexity: count_complexity(node, source),
        decorators: Vec::new(),
    })
}

/// Extract a Go type_spec (inside type_declaration).
/// `type Foo struct {...}` → Class symbol. `type Bar int` → Type symbol.
fn extract_go_type_spec(node: &Node, source: &str, file_path: &str) -> Option<SymbolDef> {
    let name = get_child_text(node, "name", source)?;
    let qualified_name = format!("{}.{}", file_path, name);

    // Check if the type is a struct (→ Class) or something else (→ TypeAlias)
    let type_node = node.child_by_field_name("type")?;
    let kind = if type_node.kind() == "struct_type" {
        SymbolKind::Class
    } else if type_node.kind() == "interface_type" {
        SymbolKind::Interface
    } else {
        SymbolKind::TypeAlias
    };
    let exported = is_go_exported(&name);

    Some(SymbolDef {
        kind,
        name,
        qualified_name,
        file_path: file_path.to_string(),
        line_start: node.start_position().row + 1,
        line_end: node.end_position().row + 1,
        signature: None,
        return_type: None,
        receiver_type: None,
        is_exported: exported,
        complexity: 1,
        decorators: Vec::new(),
    })
}
/// Extract all import statements from the AST root.
/// TS/JS: import statements + CommonJS require().
/// Python: import/import_from statements.
fn extract_imports(root: &Node, source: &str, lang: Language, map: &mut ModuleMap) {
    if matches!(lang, Language::Python | Language::Rust | Language::Go) {
        if lang == Language::Python {
            crate::languages::python::parse_imports(root, source, map);
        } else if lang == Language::Rust {
            crate::languages::rust::parse_imports(root, source, map);
        } else {
            crate::languages::go::parse_imports(root, source, map);
        }
        return;
    }
    if matches!(
        lang,
        Language::Java
            | Language::CSharp
            | Language::Ruby
            | Language::Php
            | Language::C
            | Language::Cpp
    ) {
        match lang {
            Language::Java => crate::languages::java::parse_imports(root, source, map),
            Language::CSharp => crate::languages::csharp::parse_imports(root, source, map),
            Language::Ruby => crate::languages::ruby::parse_imports(root, source, map),
            Language::Php => crate::languages::php::parse_imports(root, source, map),
            Language::C => crate::languages::c::parse_imports(root, source, map),
            Language::Cpp => crate::languages::cpp::parse_imports(root, source, map),
            _ => unreachable!(),
        }
        return;
    }

    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "import_statement" {
            crate::languages::ts::parse_import_node(&child, source, map);
        }
    }
    // CommonJS require() — JS only
    if matches!(lang, Language::JavaScript | Language::Jsx) {
        crate::languages::ts::parse_require_calls(root, source, map);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract_ts(src: &str) -> ExtractionResult {
        extract(src, Language::TypeScript, "test.ts").unwrap()
    }

    #[test]
    fn test_extract_function() {
        let src = r#"
            function add(a: number, b: number): number {
              return a + b;
            }
        "#;
        let result = extract_ts(src);
        assert_eq!(result.symbols.len(), 1);
        let sym = &result.symbols[0];
        assert_eq!(sym.kind, SymbolKind::Function);
        assert_eq!(sym.name, "add");
        assert_eq!(sym.qualified_name, "test.ts.add");
        assert!(sym.signature.is_some());
        assert_eq!(sym.complexity, 1); // no branching
    }

    #[test]
    fn test_extract_class_with_methods() {
        let src = r#"
            class Calculator {
              add(a: number, b: number): number {
                return a + b;
              }
              multiply(a: number, b: number): number {
                return a * b;
              }
            }
        "#;
        let result = extract_ts(src);
        // 1 class + 2 methods
        assert_eq!(result.symbols.len(), 3);
        assert_eq!(result.symbols[0].kind, SymbolKind::Class);
        assert_eq!(result.symbols[0].name, "Calculator");
        assert_eq!(result.symbols[1].kind, SymbolKind::Method);
        assert_eq!(result.symbols[1].name, "add");
        assert_eq!(result.symbols[1].qualified_name, "test.ts.Calculator.add");
        assert_eq!(result.symbols[2].name, "multiply");
    }

    #[test]
    fn test_extract_interface() {
        let src = "interface Foo { bar(): void; }";
        let result = extract_ts(src);
        assert_eq!(result.symbols.len(), 1);
        assert_eq!(result.symbols[0].kind, SymbolKind::Interface);
        assert_eq!(result.symbols[0].name, "Foo");
    }

    #[test]
    fn test_extract_enum() {
        let src = "enum Color { Red, Green, Blue }";
        let result = extract_ts(src);
        assert_eq!(result.symbols.len(), 1);
        assert_eq!(result.symbols[0].kind, SymbolKind::Enum);
    }

    #[test]
    fn test_extract_type_alias() {
        let src = "type UserId = number;";
        let result = extract_ts(src);
        assert_eq!(result.symbols.len(), 1);
        assert_eq!(result.symbols[0].kind, SymbolKind::TypeAlias);
        assert_eq!(result.symbols[0].name, "UserId");
    }

    #[test]
    fn test_extract_arrow_function() {
        let src = "const add = (a: number, b: number) => a + b;";
        let result = extract_ts(src);
        assert_eq!(result.symbols.len(), 1);
        assert_eq!(result.symbols[0].kind, SymbolKind::Variable);
        assert_eq!(result.symbols[0].name, "add");
    }

    #[test]
    fn test_complexity_counting() {
        let src = r#"
            function complex(x: number): number {
              if (x > 0) {
                if (x > 10) {
                  return x;
                }
                for (let i = 0; i < x; i++) {
                  console.log(i);
                }
              }
              return x > 5 ? 1 : 0;
            }
        "#;
        let result = extract_ts(src);
        let sym = &result.symbols[0];
        // 1 base + 2 if + 1 for + 1 ternary = 5
        assert_eq!(sym.complexity, 5);
    }

    #[test]
    fn test_exported_function() {
        let src = "export function publicApi(): void {}";
        let result = extract_ts(src);
        assert!(result.symbols[0].is_exported);
    }

    #[test]
    fn test_extract_imports() {
        let src = r#"
            import { foo } from './utils';
            import express from 'express';
        "#;
        let result = extract_ts(src);
        assert_eq!(result.module_map.imports.len(), 2);
        assert_eq!(result.module_map.module_paths.len(), 2);
    }

    #[test]
    fn test_extract_calls() {
        let src = r#"
            function main() {
              const result = calculate(42);
              helper.doThing(result);
            }
        "#;
        let result = extract_ts(src);
        assert_eq!(result.symbols.len(), 1);
        assert_eq!(result.symbols[0].name, "main");
        // Calls inside the function body are now collected via extract_calls
        assert_eq!(result.calls.len(), 2);
        assert_eq!(result.calls[0].callee, "calculate");
        assert_eq!(result.calls[1].callee, "helper.doThing");
        assert_eq!(
            result.calls[0].caller_qualified.as_deref(),
            Some("test.ts.main")
        );
    }

    #[test]
    fn test_parse_error_recovery_extracts_symbols_before_error() {
        // Tree-sitter is error-recoverable: it extracts valid symbols before
        // and after syntax errors. Verify that a function defined before
        // garbage is still extracted.
        let src = r#"
            export function valid(): number {
              return 42;
            }

            @#$%^&*  // syntax error garbage
        "#;
        let result = extract_ts(src);
        let valid = result.symbols.iter().find(|s| s.name == "valid");
        assert!(
            valid.is_some(),
            "valid() should be extracted despite trailing syntax error"
        );
        let valid = valid.unwrap();
        assert_eq!(valid.kind, SymbolKind::Function);
        assert!(valid.is_exported);
        assert_eq!(valid.line_start, 2);
    }

    #[test]
    fn test_parse_error_recovery_extracts_symbols_after_error() {
        // Symbols defined after a syntax error should also be extracted.
        let src = r#"
            @#$%^&*  // syntax error garbage

            export function after_error(): void {
              console.log("recovered");
            }
        "#;
        let result = extract_ts(src);
        let after = result.symbols.iter().find(|s| s.name == "after_error");
        assert!(
            after.is_some(),
            "after_error() should be extracted despite preceding syntax error"
        );
        let after = after.unwrap();
        assert_eq!(after.kind, SymbolKind::Function);
        assert!(after.is_exported);
    }

    fn extract_rust(src: &str) -> ExtractionResult {
        extract(src, Language::Rust, "test.rs").unwrap()
    }

    #[test]
    fn test_rust_extract_full() {
        let src = r#"
use std::collections::HashMap;

fn add(a: i32, b: i32) -> i32 {
    a + b
}

struct Calculator {
    value: i32,
}

impl Calculator {
    fn new() -> Self {
        Calculator { value: 0 }
    }
    fn compute(&self, x: i32) -> i32 {
        if x > 0 {
            self.value + x
        } else {
            match x {
                0 => 0,
                _ => -x,
            }
        }
    }
}

enum Op {
    Add,
    Sub(i32),
}

fn main() {
    let calc = Calculator::new();
    let result = calc.compute(42);
    let sum = add(1, 2);
}
"#;
        let result = extract_rust(src);

        // ── Symbols ──
        // add (Function), Calculator (Class), Calculator.new (Method),
        // Calculator.compute (Method), Op (Enum), main (Function)
        assert_eq!(
            result.symbols.len(),
            6,
            "expected 6 symbols, got {:?}",
            result.symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
        );

        let add = result.symbols.iter().find(|s| s.name == "add").unwrap();
        assert_eq!(add.kind, SymbolKind::Function);
        assert_eq!(add.qualified_name, "test.rs.add");

        let calc = result
            .symbols
            .iter()
            .find(|s| s.name == "Calculator")
            .unwrap();
        assert_eq!(calc.kind, SymbolKind::Class);
        assert_eq!(calc.qualified_name, "test.rs.Calculator");

        let new = result.symbols.iter().find(|s| s.name == "new").unwrap();
        assert_eq!(new.kind, SymbolKind::Method);
        assert_eq!(new.qualified_name, "test.rs.Calculator.new");

        let compute = result.symbols.iter().find(|s| s.name == "compute").unwrap();
        assert_eq!(compute.kind, SymbolKind::Method);
        assert_eq!(compute.qualified_name, "test.rs.Calculator.compute");
        // complexity: base(1) + if(1) + match is handled by match_arm recursion
        // if_expression=1, match_arm 0=>0 and _ => -x = 2 match arms
        // Total: 1 + 1 + 2 = 4
        assert!(
            compute.complexity >= 3,
            "compute complexity={}, expected >=3",
            compute.complexity
        );

        let op = result.symbols.iter().find(|s| s.name == "Op").unwrap();
        assert_eq!(op.kind, SymbolKind::Enum);

        let main = result.symbols.iter().find(|s| s.name == "main").unwrap();
        assert_eq!(main.kind, SymbolKind::Function);

        // ── Calls ──
        // Calculator::new(), calc.compute(42), add(1, 2)
        assert_eq!(
            result.calls.len(),
            3,
            "expected 3 calls, got {:?}",
            result.calls
        );

        // Check callee names
        let callees: Vec<&str> = result.calls.iter().map(|c| c.callee.as_str()).collect();
        assert!(callees.contains(&"add"), "no call to 'add'");
        assert!(
            callees.iter().any(|c| c.contains("compute")),
            "no call to compute: {:?}",
            callees
        );
        assert!(
            callees.iter().any(|c| c.contains("new")),
            "no call to new: {:?}",
            callees
        );

        // ── Imports ──
        assert_eq!(result.module_map.imports.len(), 1);
        assert_eq!(
            result.module_map.imports.get("HashMap"),
            Some(&"std::collections::HashMap".to_string())
        );
    }

    #[test]
    fn test_rust_is_exported() {
        // Non-pub functions/structs are NOT exported
        let src = r#"
fn private_fn() {}
struct Private;
"#;
        let result = extract_rust(src);
        let pf = result
            .symbols
            .iter()
            .find(|s| s.name == "private_fn")
            .unwrap();
        assert!(!pf.is_exported, "non-pub fn should be is_exported=false");
        let st = result.symbols.iter().find(|s| s.name == "Private").unwrap();
        assert!(
            !st.is_exported,
            "non-pub struct should be is_exported=false"
        );
    }

    #[test]
    fn test_rust_pub_crate_not_exported() {
        // pub(crate) is crate-internal — NOT is_exported
        let src = r#"
pub(crate) fn internal_fn() {}
pub fn public_fn() {}
"#;
        let result = extract_rust(src);
        let internal = result
            .symbols
            .iter()
            .find(|s| s.name == "internal_fn")
            .unwrap();
        assert!(
            !internal.is_exported,
            "pub(crate) fn should be is_exported=false"
        );
        let public = result
            .symbols
            .iter()
            .find(|s| s.name == "public_fn")
            .unwrap();
        assert!(public.is_exported, "pub fn should be is_exported=true");
    }

    #[test]
    fn test_rust_test_attribute_is_exported() {
        // #[test] and #[tokio::test] functions are entry points — the test
        // harness invokes them without producing call edges the indexer sees.
        // They must be treated as exported so detect_dead_code doesn't report FPs.
        let src = r#"
#[test]
fn my_test() {
    assert!(true);
}

#[tokio::test]
async fn async_test() {
    assert!(true);
}

fn real_func() {}
"#;
        let result = extract_rust(src);
        let test_fn = result.symbols.iter().find(|s| s.name == "my_test").unwrap();
        assert!(test_fn.is_exported, "#[test] fn should be is_exported=true");

        let async_fn = result
            .symbols
            .iter()
            .find(|s| s.name == "async_test")
            .unwrap();
        assert!(
            async_fn.is_exported,
            "#[tokio::test] fn should be is_exported=true"
        );

        let real = result
            .symbols
            .iter()
            .find(|s| s.name == "real_func")
            .unwrap();
        assert!(
            !real.is_exported,
            "non-pub fn without #[test] should be is_exported=false"
        );
    }

    /// Regression: deeply-nested ASTs must not overflow the stack.
    /// Recursive `walk()` / `count_complexity()` use one stack frame per AST
    /// level (~hundreds of bytes/frame). The default 8 MiB thread stack
    /// overflows at a few hundred levels of nested TS functions. Iterative
    /// extraction must index 1200-deep nesting without crashing.
    /// Runs the extraction explicitly inside a DEFAULT-stack (8 MiB) thread so
    /// the 256 MiB mitigation in `index_cmd` cannot mask a regression here.
    /// 1200 is well past the ~600 recursive overflow point and fast in CI
    /// (~2s) while still proving the iterative port. Anything deeper rapidly
    /// inflates the work-stack VecDeque in RAM, ballooning runtime — this is a
    /// crash-protection test, not a perf test.
    #[test]
    fn test_extract_deeply_nested_functions_no_stack_overflow() {
        let depth = 1_200;
        let mut src = String::with_capacity(depth * 30);
        for _ in 0..depth {
            src.push_str("function f() { ");
        }
        src.push_str("return 0; ");
        for _ in 0..depth {
            src.push_str("} ");
        }

        let handle = std::thread::Builder::new()
            // Deliberately the DEFAULT stack — exercises the production
            // watcher/server path, NOT the 256 MiB CLI mitigation.
            .spawn(move || {
                let n = extract(&src, Language::TypeScript, "depth.ts").unwrap();
                assert_eq!(
                    n.symbols.len(),
                    depth,
                    "every nested function should yield a symbol"
                );
                assert!(n.calls.is_empty(), "no call sites in this snippet");
                n.symbols.len()
            })
            .expect("spawn depth-stress thread");
        let got = handle.join().expect("depth-stress thread panicked");
        assert_eq!(got, depth);
    }
}

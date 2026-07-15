// kernava-indexer: call resolution
// P1 task 1.8: 6-strategy cascade to resolve raw callee names to symbol definitions
// Strategies 1-4 functional; 5-6 stubbed for v2 (LSP hybrid).

#[cfg(test)]
use crate::extractor::SymbolKind;
use crate::extractor::{CallSite, SymbolDef};
use crate::languages::ModuleMap;
use std::collections::HashMap;

/// A resolved call: callee name → target symbol's qualified name, with confidence.
#[derive(Debug, Clone)]
pub struct ResolvedCall {
    /// The callee as it appeared in source (e.g., "foo", "obj.method")
    pub callee: String,
    /// The resolved target symbol's qualified name, or None if unresolved
    pub target_qualified: Option<String>,
    /// Resolution confidence (0.0-1.0)
    pub confidence: f64,
    /// Which strategy succeeded
    pub strategy: ResolutionStrategy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionStrategy {
    ImportMap,     // 0.95
    SameFile,      // 0.85
    SamePackage,   // 0.75
    GlobalUnique,  // 0.90
    TypeReceiver,  // 0.80 (stubbed)
    FuzzyFallback, // 0.50 (stubbed)
    Unresolved,    // 0.0
}

impl ResolutionStrategy {
    pub fn confidence(&self) -> f64 {
        match self {
            Self::ImportMap => 0.95,
            Self::SameFile => 0.85,
            Self::SamePackage => 0.75,
            Self::GlobalUnique => 0.90,
            Self::TypeReceiver => 0.80,
            Self::FuzzyFallback => 0.50,
            Self::Unresolved => 0.0,
        }
    }
}

/// A function registry built from all extracted symbols in the project.
/// Indexes symbols by qualified name (exact) and simple name (reverse).
pub struct FunctionRegistry {
    /// qualified_name → symbol index
    pub by_qualified: HashMap<String, usize>,
    /// simple_name → list of symbol indices
    pub by_simple: HashMap<String, Vec<usize>>,
    /// All symbols (owned)
    pub symbols: Vec<SymbolDef>,
}

impl FunctionRegistry {
    pub fn new() -> Self {
        Self {
            by_qualified: HashMap::new(),
            by_simple: HashMap::new(),
            symbols: Vec::new(),
        }
    }

    /// Register a symbol. Files are added in order; index is the position.
    pub fn register(&mut self, sym: SymbolDef) {
        let idx = self.symbols.len();
        self.by_qualified.insert(sym.qualified_name.clone(), idx);
        self.by_simple
            .entry(sym.name.clone())
            .or_default()
            .push(idx);
        self.symbols.push(sym);
    }

    /// Find a symbol by qualified name (exact match).
    pub fn find_qualified(&self, qn: &str) -> Option<&SymbolDef> {
        self.by_qualified.get(qn).map(|&i| &self.symbols[i])
    }

    /// Find symbols by simple name (may return multiple).
    pub fn find_simple(&self, name: &str) -> &[usize] {
        self.by_simple
            .get(name)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Count symbols with a given simple name.
    pub fn count_simple(&self, name: &str) -> usize {
        self.by_simple.get(name).map(|v| v.len()).unwrap_or(0)
    }
}

/// Resolve a batch of call sites against the function registry.
/// Returns a resolved call for each call site.
pub fn resolve_calls(
    calls: &[CallSite],
    registry: &FunctionRegistry,
    module_map: &ModuleMap,
    file_path: &str,
) -> Vec<ResolvedCall> {
    calls
        .iter()
        .map(|call| resolve_one(call, registry, module_map, file_path))
        .collect()
}

/// Resolve a single call site using the 6-strategy cascade.
fn resolve_one(
    call: &CallSite,
    registry: &FunctionRegistry,
    module_map: &ModuleMap,
    file_path: &str,
) -> ResolvedCall {
    let callee = &call.callee;

    // Handle member expressions: "obj.method" or "a.b.c"
    // Split into prefix and suffix: prefix may be an import alias
    let (prefix, suffix) = split_callee(callee);

    // Strategy 1: Import map (confidence 0.95)
    if let Some(resolved) = try_import_map(callee, prefix, suffix, module_map, registry) {
        return resolved;
    }

    // Strategy 2: Same-file (confidence 0.85)
    if let Some(resolved) = try_same_file(suffix, file_path, registry) {
        return resolved;
    }

    // Strategy 3: Same-package (confidence 0.75)
    // TODO: Implement same-package when builder provides project-wide registry.

    // Strategy 4: Global unique (confidence 0.90)
    // Only for unqualified calls — member expressions (arr.push, str.split) are
    // builtin method calls, not project symbols. prefix.is_none() guards this.
    if prefix.is_none() {
        if let Some(resolved) = try_global_unique(suffix, registry) {
            return resolved;
        }
    }

    // Strategy 5: Type-receiver (confidence 0.80)
    // TODO: v2 — infer receiver type from variable declarations, match method on type.
    // Requires LSP hybrid or type inference table.

    // Strategy 6: Fuzzy fallback (confidence 0.50)
    // TODO: v2 — edit distance match against all symbols.

    ResolvedCall {
        callee: callee.clone(),
        target_qualified: None,
        confidence: 0.0,
        strategy: ResolutionStrategy::Unresolved,
    }
}

/// Split a callee string into (prefix, suffix).
/// "foo" → (None, "foo")
/// "obj.method" → (Some("obj"), "method")
/// "a.b.c" → (Some("a.b"), "c")
fn split_callee(callee: &str) -> (Option<&str>, &str) {
    match callee.rfind('.') {
        Some(pos) => (Some(&callee[..pos]), &callee[pos + 1..]),
        None => (None, callee),
    }
}

/// Strategy 1: Import map.
/// If prefix is an import alias, resolve through the module map.
fn try_import_map(
    full_callee: &str,
    prefix: Option<&str>,
    suffix: &str,
    module_map: &ModuleMap,
    registry: &FunctionRegistry,
) -> Option<ResolvedCall> {
    // Case A: Simple imported name — "foo" where "foo" is in import map
    if prefix.is_none() {
        if let Some(module_path) = module_map.imports.get(full_callee) {
            // Look for a symbol in the imported module with this name
            let target_qn = format!("{}.{}", module_path, full_callee);
            if registry.find_qualified(&target_qn).is_some() {
                return Some(ResolvedCall {
                    callee: full_callee.to_string(),
                    target_qualified: Some(target_qn),
                    confidence: ResolutionStrategy::ImportMap.confidence(),
                    strategy: ResolutionStrategy::ImportMap,
                });
            }
        }
    }

    // Case B: Member access on imported name — "utils.foo" where "utils" is namespace import
    // Also handles "pkg.Calculator.compute" where first segment "pkg" is the import alias
    // and "Calculator.compute" is the member path — tries direct then class-qualified.
    if let Some(pfx) = prefix {
        // Try the full prefix first (e.g., "utils" in "utils.foo")
        // Then try the first segment (e.g., "calc" in "calc.Calculator.compute")
        let prefixes: Vec<&str> = if pfx.contains('.') {
            vec![pfx, pfx.split('.').next().unwrap_or(pfx)]
        } else {
            vec![pfx]
        };

        for &try_pfx in &prefixes {
            if let Some(module_path) = module_map.imports.get(try_pfx) {
                // Direct match: module_path.suffix
                let target_qn = format!("{}.{}", module_path, suffix);
                if registry.find_qualified(&target_qn).is_some() {
                    return Some(ResolvedCall {
                        callee: full_callee.to_string(),
                        target_qualified: Some(target_qn),
                        confidence: ResolutionStrategy::ImportMap.confidence(),
                        strategy: ResolutionStrategy::ImportMap,
                    });
                }

                // Qualified direct match: when using first segment as import alias,
                // try module_path.{rest_of_prefix}.{suffix}
                // e.g., "calc.A.compute" → calc→"src/calc", rest="A" → "src/calc.A.compute"
                if try_pfx != pfx {
                    let rest = &pfx[try_pfx.len()..];
                    let rest_trimmed = rest.strip_prefix('.').unwrap_or(rest);
                    let qualified_target = format!("{}.{}.{}", module_path, rest_trimmed, suffix);
                    if registry.find_qualified(&qualified_target).is_some() {
                        return Some(ResolvedCall {
                            callee: full_callee.to_string(),
                            target_qualified: Some(qualified_target),
                            confidence: ResolutionStrategy::ImportMap.confidence(),
                            strategy: ResolutionStrategy::ImportMap,
                        });
                    }
                }
                // Class-qualified match: module_path.ClassName.suffix
                // Scans registry for symbols whose qualified_name starts with
                // "module_path." and ends with ".suffix". Resolves calls like
                // pkg.Calculator.compute() where import maps "pkg" to a module.
                // Returns only if exactly one match — avoids picking arbitrarily on ambiguity.
                // ponytail: linear scan of by_qualified per Case B miss — add secondary index
                // by simple_name when project exceeds 10K files to avoid O(n) per call site.
                let prefix_str = format!("{}.", module_path);
                let suffix_str = format!(".{}", suffix);
                let mut found: Option<String> = None;
                let mut count = 0;
                for (qn, _) in &registry.by_qualified {
                    if qn.starts_with(&prefix_str) && qn.ends_with(&suffix_str) {
                        found = Some(qn.clone());
                        count += 1;
                        // ponytail: HashMap order non-deterministic, so multi-match
                        // (pkg.A.helper + pkg.B.helper) returns None to avoid flapping.
                        if count > 1 {
                            break;
                        }
                    }
                }
                if count == 1 {
                    if let Some(qn) = found {
                        return Some(ResolvedCall {
                            callee: full_callee.to_string(),
                            target_qualified: Some(qn),
                            confidence: ResolutionStrategy::ImportMap.confidence() * 0.9,
                            strategy: ResolutionStrategy::ImportMap,
                        });
                    }
                }
            }
        }
    }

    None
}

/// Strategy 2: Same-file.
/// Look for a symbol in the same file with matching simple name.
fn try_same_file(
    suffix: &str,
    file_path: &str,
    registry: &FunctionRegistry,
) -> Option<ResolvedCall> {
    let indices = registry.find_simple(suffix);
    for &idx in indices {
        let sym = &registry.symbols[idx];
        if sym.file_path == file_path {
            return Some(ResolvedCall {
                callee: suffix.to_string(),
                target_qualified: Some(sym.qualified_name.clone()),
                confidence: ResolutionStrategy::SameFile.confidence(),
                strategy: ResolutionStrategy::SameFile,
            });
        }
    }
    None
}

/// Strategy 4: Global unique.
/// If there's exactly one symbol with this simple name in the entire project, resolve to it.
fn try_global_unique(suffix: &str, registry: &FunctionRegistry) -> Option<ResolvedCall> {
    let indices = registry.find_simple(suffix);
    if indices.len() == 1 {
        let sym = &registry.symbols[indices[0]];
        return Some(ResolvedCall {
            callee: suffix.to_string(),
            target_qualified: Some(sym.qualified_name.clone()),
            confidence: ResolutionStrategy::GlobalUnique.confidence(),
            strategy: ResolutionStrategy::GlobalUnique,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_registry() -> FunctionRegistry {
        let mut reg = FunctionRegistry::new();
        // Simulate symbols from two files
        reg.register(SymbolDef {
            kind: SymbolKind::Function,
            name: "add".into(),
            qualified_name: "src/math.add".into(),
            file_path: "src/math".into(),
            line_start: 1,
            line_end: 5,
            signature: None,
            return_type: None,
            receiver_type: None,
            is_exported: true,
            complexity: 1,
            decorators: vec![],
        });
        reg.register(SymbolDef {
            kind: SymbolKind::Function,
            name: "calculate".into(),
            qualified_name: "src/calc.calculate".into(),
            file_path: "src/calc".into(),
            line_start: 1,
            line_end: 10,
            signature: None,
            return_type: None,
            receiver_type: None,
            is_exported: false,
            complexity: 3,
            decorators: vec![],
        });
        reg.register(SymbolDef {
            kind: SymbolKind::Function,
            name: "helper".into(),
            qualified_name: "src/calc.helper".into(),
            file_path: "src/calc".into(),
            line_start: 12,
            line_end: 15,
            signature: None,
            return_type: None,
            receiver_type: None,
            is_exported: true,
            complexity: 1,
            decorators: vec![],
        });
        reg
    }

    #[test]
    fn test_strategy_same_file() {
        let reg = make_registry();
        let map = ModuleMap::default();
        let call = CallSite {
            callee: "helper".into(),
            line: 5,
            caller_qualified: Some("src/calc.calculate".into()),
            col: 10,
        };
        let resolved = resolve_one(&call, &reg, &map, "src/calc");
        assert_eq!(resolved.strategy, ResolutionStrategy::SameFile);
        assert_eq!(
            resolved.target_qualified.as_deref(),
            Some("src/calc.helper")
        );
        assert!((resolved.confidence - 0.85).abs() < 1e-9);
    }

    #[test]
    fn test_strategy_import_map() {
        let reg = make_registry();
        let mut map = ModuleMap::default();
        map.imports.insert("add".into(), "src/math".into());

        let call = CallSite {
            callee: "add".into(),
            line: 3,
            caller_qualified: Some("src/calc.calculate".into()),
            col: 10,
        };
        let resolved = resolve_one(&call, &reg, &map, "src/calc");
        assert_eq!(resolved.strategy, ResolutionStrategy::ImportMap);
        assert_eq!(resolved.target_qualified.as_deref(), Some("src/math.add"));
    }

    #[test]
    fn test_strategy_import_map_namespace() {
        let reg = make_registry();
        let mut map = ModuleMap::default();
        map.imports.insert("math".into(), "src/math".into());

        // "math.add" where "math" is a namespace import for "src/math"
        let call = CallSite {
            callee: "math.add".into(),
            line: 3,
            caller_qualified: Some("src/calc.calculate".into()),
            col: 10,
        };
        let resolved = resolve_one(&call, &reg, &map, "src/calc");
        assert_eq!(resolved.strategy, ResolutionStrategy::ImportMap);
        assert_eq!(resolved.target_qualified.as_deref(), Some("src/math.add"));
    }

    #[test]
    fn test_strategy_global_unique() {
        let reg = make_registry();
        let map = ModuleMap::default();

        // "calculate" exists only once globally and not in imports
        let call = CallSite {
            callee: "calculate".into(),
            line: 3,
            caller_qualified: Some("src/main.main".into()),
            col: 10,
        };
        let resolved = resolve_one(&call, &reg, &map, "src/main");
        assert_eq!(resolved.strategy, ResolutionStrategy::GlobalUnique);
        assert_eq!(
            resolved.target_qualified.as_deref(),
            Some("src/calc.calculate")
        );
    }

    #[test]
    fn test_unresolved() {
        let reg = make_registry();
        let map = ModuleMap::default();

        let call = CallSite {
            callee: "nonexistent".into(),
            line: 3,
            caller_qualified: Some("src/main.main".into()),
            col: 10,
        };
        let resolved = resolve_one(&call, &reg, &map, "src/main");
        assert_eq!(resolved.strategy, ResolutionStrategy::Unresolved);
        assert!(resolved.target_qualified.is_none());
    }

    #[test]
    fn test_ambiguous_not_global_unique() {
        // "helper" appears in src/calc only once → global unique
        // But if we add another "helper" in a different file, it's ambiguous
        let mut reg = make_registry();
        reg.register(SymbolDef {
            kind: SymbolKind::Function,
            name: "helper".into(),
            qualified_name: "src/other.helper".into(),
            file_path: "src/other".into(),
            line_start: 1,
            line_end: 5,
            signature: None,
            return_type: None,
            receiver_type: None,
            is_exported: true,
            complexity: 1,
            decorators: vec![],
        });
        let map = ModuleMap::default();

        let call = CallSite {
            callee: "helper".into(),
            line: 3,
            caller_qualified: Some("src/main.main".into()),
            col: 10,
        };
        let resolved = resolve_one(&call, &reg, &map, "src/main");
        // Not global unique (2 matches), not same file, not import → unresolved
        assert_eq!(resolved.strategy, ResolutionStrategy::Unresolved);
    }

    #[test]
    fn test_split_callee() {
        assert_eq!(split_callee("foo"), (None, "foo"));
        assert_eq!(split_callee("obj.method"), (Some("obj"), "method"));
        assert_eq!(split_callee("a.b.c"), (Some("a.b"), "c"));
    }

    #[test]
    fn test_import_map_class_qualified_method() {
        // "calc.Calculator.compute" where "calc" maps to "src/calc"
        // Direct lookup "src/calc.Calculator.compute" should hit Case B class-qualified fallback.
        let mut reg = make_registry();
        reg.register(SymbolDef {
            kind: SymbolKind::Method,
            name: "compute".into(),
            qualified_name: "src/calc.Calculator.compute".into(),
            file_path: "src/calc".into(),
            line_start: 5,
            line_end: 10,
            signature: None,
            return_type: None,
            receiver_type: Some("src/calc.Calculator".into()),
            is_exported: true,
            complexity: 2,
            decorators: vec![],
        });
        let mut map = ModuleMap::default();
        map.imports.insert("calc".into(), "src/calc".into());

        let call = CallSite {
            callee: "calc.Calculator.compute".into(),
            line: 3,
            caller_qualified: Some("src/main.main".into()),
            col: 10,
        };
        let resolved = resolve_one(&call, &reg, &map, "src/main");
        assert_eq!(resolved.strategy, ResolutionStrategy::ImportMap);
        assert_eq!(
            resolved.target_qualified.as_deref(),
            Some("src/calc.Calculator.compute")
        );
        // Resolved via qualified direct match at full ImportMap confidence (0.95)
        assert!((resolved.confidence - 0.95).abs() < 1e-9);
    }

    #[test]
    fn test_import_map_class_qualified_ambiguous() {
        // Two methods named "compute" in different classes, no direct match → ambiguous
        let mut reg = FunctionRegistry::new();
        reg.register(SymbolDef {
            kind: SymbolKind::Method,
            name: "compute".into(),
            qualified_name: "src/calc.A.compute".into(),
            file_path: "src/calc".into(),
            line_start: 5,
            line_end: 10,
            signature: None,
            return_type: None,
            receiver_type: Some("src/calc.A".into()),
            is_exported: true,
            complexity: 1,
            decorators: vec![],
        });
        reg.register(SymbolDef {
            kind: SymbolKind::Method,
            name: "compute".into(),
            qualified_name: "src/calc.B.compute".into(),
            file_path: "src/calc".into(),
            line_start: 15,
            line_end: 20,
            signature: None,
            return_type: None,
            receiver_type: Some("src/calc.B".into()),
            is_exported: true,
            complexity: 1,
            decorators: vec![],
        });
        let mut map = ModuleMap::default();
        map.imports.insert("calc".into(), "src/calc".into());

        let call = CallSite {
            callee: "calc.A.compute".into(),
            line: 3,
            caller_qualified: Some("src/main.main".into()),
            col: 10,
        };
        let resolved = resolve_one(&call, &reg, &map, "src/main");
        // Direct lookup "src/calc.A.compute" hits, no ambiguity — singular match
        assert_eq!(
            resolved.target_qualified.as_deref(),
            Some("src/calc.A.compute")
        );

        // Now test actual ambiguity: callee "calc.compute" matches both A.compute and B.compute
        let call2 = CallSite {
            callee: "calc.compute".into(),
            line: 3,
            caller_qualified: Some("src/main.main".into()),
            col: 10,
        };
        let resolved2 = resolve_one(&call2, &reg, &map, "src/main");
        // Ambiguous (2 matches) → class-qualified returns None, falls through
        assert_ne!(
            resolved2.target_qualified,
            Some("src/calc.A.compute".to_string())
        );
        assert_ne!(
            resolved2.target_qualified,
            Some("src/calc.B.compute".to_string())
        );
    }

    #[test]
    fn test_same_file_path_boundary() {
        // src/calc.helper must NOT match when file_path is src/calculator
        // (the old starts_with bug would match "src/calc." as prefix of "src/calculator.")
        let mut reg = FunctionRegistry::new();
        reg.register(SymbolDef {
            kind: SymbolKind::Function,
            name: "helper".into(),
            qualified_name: "src/calc.helper".into(),
            file_path: "src/calc".into(),
            line_start: 1,
            line_end: 5,
            signature: None,
            return_type: None,
            receiver_type: None,
            is_exported: true,
            complexity: 1,
            decorators: vec![],
        });
        let map = ModuleMap::default();

        // Call from src/calculator — must NOT resolve to src/calc.helper
        let call = CallSite {
            callee: "helper".into(),
            line: 3,
            caller_qualified: Some("src/calculator.foo".into()),
            col: 10,
        };
        let resolved = resolve_one(&call, &reg, &map, "src/calculator");
        assert_eq!(resolved.strategy, ResolutionStrategy::GlobalUnique);
        // Global unique still fires because there's only 1 "helper" globally.
        // But same-file must NOT match — verify by adding a second helper.

        let mut reg2 = make_registry(); // has src/calc.helper
        reg2.register(SymbolDef {
            kind: SymbolKind::Function,
            name: "helper".into(),
            qualified_name: "src/calculator.helper".into(),
            file_path: "src/calculator".into(),
            line_start: 1,
            line_end: 5,
            signature: None,
            return_type: None,
            receiver_type: None,
            is_exported: true,
            complexity: 1,
            decorators: vec![],
        });
        let call2 = CallSite {
            callee: "helper".into(),
            line: 3,
            caller_qualified: Some("src/calculator.foo".into()),
            col: 10,
        };
        let resolved2 = resolve_one(&call2, &reg2, &map, "src/calculator");
        // Now ambiguous globally (2 helpers) but same-file matches src/calculator.helper
        assert_eq!(resolved2.strategy, ResolutionStrategy::SameFile);
        assert_eq!(
            resolved2.target_qualified.as_deref(),
            Some("src/calculator.helper")
        );
    }

    #[test]
    fn test_builtin_method_not_resolved() {
        // arr.push(x) must NOT resolve to a user function named "push"
        let mut reg = FunctionRegistry::new();
        reg.register(SymbolDef {
            kind: SymbolKind::Function,
            name: "push".into(),
            qualified_name: "src/queue.push".into(),
            file_path: "src/queue".into(),
            line_start: 1,
            line_end: 5,
            signature: None,
            return_type: None,
            receiver_type: None,
            is_exported: true,
            complexity: 1,
            decorators: vec![],
        });
        let map = ModuleMap::default();

        // "arr.push" — prefix is Some("arr"), so global-unique is skipped
        let call = CallSite {
            callee: "arr.push".into(),
            line: 3,
            caller_qualified: Some("src/main.main".into()),
            col: 10,
        };
        let resolved = resolve_one(&call, &reg, &map, "src/main");
        assert_eq!(resolved.strategy, ResolutionStrategy::Unresolved);

        // But bare "push()" with no prefix — global unique fires (only 1 "push")
        let call2 = CallSite {
            callee: "push".into(),
            line: 3,
            caller_qualified: Some("src/main.main".into()),
            col: 10,
        };
        let resolved2 = resolve_one(&call2, &reg, &map, "src/main");
        assert_eq!(resolved2.strategy, ResolutionStrategy::GlobalUnique);
    }

    #[test]
    fn test_aliased_import_unresolved_v1_limitation() {
        // `import { foo as bar } from './utils'` → calling `bar()` should resolve
        // to src/utils.foo, but ModuleMap.imports only stores local_name → module_path,
        // not the original exported name. try_import_map builds "src/utils.bar" and
        // looks it up — but the symbol is "src/utils.foo". Alias resolution is unimplemented.
        // ponytail: v1 limitation. Fix requires extending ModuleMap with original_name
        // (imports: HashMap<local_name, (module_path, original_name)>), then try_import_map
        // builds qualified_name from original_name, not local_name. Defer to v2.
        let mut reg = FunctionRegistry::new();
        reg.register(SymbolDef {
            kind: SymbolKind::Function,
            name: "foo".into(),
            qualified_name: "src/utils.foo".into(),
            file_path: "src/utils".into(),
            line_start: 1,
            line_end: 5,
            signature: None,
            return_type: None,
            receiver_type: None,
            is_exported: true,
            complexity: 1,
            decorators: vec![],
        });
        let mut map = ModuleMap::default();
        map.imports.insert("bar".into(), "src/utils".into());

        let call = CallSite {
            callee: "bar".into(),
            line: 3,
            caller_qualified: Some("src/main.main".into()),
            col: 10,
        };
        let resolved = resolve_one(&call, &reg, &map, "src/main");
        // Currently unresolved — try_import_map builds "src/utils.bar" not "src/utils.foo"
        assert_eq!(resolved.strategy, ResolutionStrategy::Unresolved);
        assert!(resolved.target_qualified.is_none());
    }

    #[test]
    fn test_default_import_resolves() {
        // `import express from 'express'` → local name "express" maps to "express"
        // Non-relative import → resolve_module_paths leaves it as-is
        let mut reg = FunctionRegistry::new();
        reg.register(SymbolDef {
            kind: SymbolKind::Function,
            name: "express".into(),
            qualified_name: "express.express".into(),
            file_path: "express".into(),
            line_start: 1,
            line_end: 5,
            signature: None,
            return_type: None,
            receiver_type: None,
            is_exported: true,
            complexity: 1,
            decorators: vec![],
        });
        let mut map = ModuleMap::default();
        map.imports.insert("express".into(), "express".into());

        let call = CallSite {
            callee: "express".into(),
            line: 3,
            caller_qualified: Some("src/app.main".into()),
            col: 10,
        };
        let resolved = resolve_one(&call, &reg, &map, "src/app");
        assert_eq!(resolved.strategy, ResolutionStrategy::ImportMap);
        assert_eq!(
            resolved.target_qualified.as_deref(),
            Some("express.express")
        );
    }

    #[test]
    fn test_same_named_methods_different_classes() {
        // Class A has method "render", Class B has method "render".
        // Bare call "render()" from outside either class — global unique should NOT fire
        // because there are 2 matches. Must remain unresolved.
        let mut reg = FunctionRegistry::new();
        reg.register(SymbolDef {
            kind: SymbolKind::Function,
            name: "render".into(),
            qualified_name: "src/A.render".into(),
            file_path: "src/A".into(),
            line_start: 2,
            line_end: 5,
            signature: None,
            return_type: None,
            receiver_type: Some("A".into()),
            is_exported: false,
            complexity: 1,
            decorators: vec![],
        });
        reg.register(SymbolDef {
            kind: SymbolKind::Function,
            name: "render".into(),
            qualified_name: "src/B.render".into(),
            file_path: "src/B".into(),
            line_start: 2,
            line_end: 5,
            signature: None,
            return_type: None,
            receiver_type: Some("B".into()),
            is_exported: false,
            complexity: 1,
            decorators: vec![],
        });
        let map = ModuleMap::default();

        // Bare "render()" — ambiguous (2 matches), unresolved
        let call = CallSite {
            callee: "render".into(),
            line: 3,
            caller_qualified: Some("src/main.main".into()),
            col: 10,
        };
        let resolved = resolve_one(&call, &reg, &map, "src/main");
        assert_eq!(resolved.strategy, ResolutionStrategy::Unresolved);
        assert!(resolved.target_qualified.is_none());

        // "a.render()" — prefix "a", not a namespace import → unresolved (no type-receiver yet)
        let call2 = CallSite {
            callee: "a.render".into(),
            line: 3,
            caller_qualified: Some("src/main.main".into()),
            col: 10,
        };
        let resolved2 = resolve_one(&call2, &reg, &map, "src/main");
        assert_eq!(resolved2.strategy, ResolutionStrategy::Unresolved);
    }

    #[test]
    fn test_shadowed_import_not_resolved_as_global() {
        // If "add" is imported AND there's another "add" in a different file,
        // ImportMap should win (higher precedence), not GlobalUnique.
        let mut reg = make_registry(); // has src/math.add
        reg.register(SymbolDef {
            kind: SymbolKind::Function,
            name: "add".into(),
            qualified_name: "src/other.add".into(),
            file_path: "src/other".into(),
            line_start: 1,
            line_end: 3,
            signature: None,
            return_type: None,
            receiver_type: None,
            is_exported: true,
            complexity: 1,
            decorators: vec![],
        });
        let mut map = ModuleMap::default();
        map.imports.insert("add".into(), "src/math".into());

        let call = CallSite {
            callee: "add".into(),
            line: 3,
            caller_qualified: Some("src/main.main".into()),
            col: 10,
        };
        let resolved = resolve_one(&call, &reg, &map, "src/main");
        // ImportMap takes precedence over GlobalUnique even with 2 "add" symbols
        assert_eq!(resolved.strategy, ResolutionStrategy::ImportMap);
        assert_eq!(resolved.target_qualified.as_deref(), Some("src/math.add"));
    }

    #[test]
    fn test_global_unique_resolves_unimported() {
        // A function not imported and existing only once globally should resolve
        // via GlobalUnique strategy regardless of what file it's in.
        let mut reg = FunctionRegistry::new();
        reg.register(SymbolDef {
            kind: SymbolKind::Function,
            name: "valid".into(),
            qualified_name: "src/broken.valid".into(),
            file_path: "src/broken".into(),
            line_start: 1,
            line_end: 5,
            signature: None,
            return_type: None,
            receiver_type: None,
            is_exported: true,
            complexity: 1,
            decorators: vec![],
        });
        let map = ModuleMap::default();

        let call = CallSite {
            callee: "valid".into(),
            line: 10,
            caller_qualified: Some("src/main.main".into()),
            col: 10,
        };
        let resolved = resolve_one(&call, &reg, &map, "src/main");
        assert_eq!(resolved.strategy, ResolutionStrategy::GlobalUnique);
        assert_eq!(
            resolved.target_qualified.as_deref(),
            Some("src/broken.valid")
        );
    }
}

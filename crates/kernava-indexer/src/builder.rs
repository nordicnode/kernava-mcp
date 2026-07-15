// kernava-indexer: builder orchestrates parse → extract → resolve → upsert
// P1 task 1.9: single-file indexing in one atomic SQLite transaction.

use crate::extractor::{self, SymbolDef, SymbolKind};
use crate::languages::ModuleMap;
use crate::parser::Language;
use crate::resolver::{self, FunctionRegistry};
use anyhow::Result;
use tracing::warn;
use kernava_store::{EdgeRecord, FileRecord, ImportEdgeRecord, NodeRecord, Store};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

/// Result of indexing a single file.
#[derive(Debug, Clone)]
pub struct IndexFileResult {
    pub file_path: String,
    pub language: String,
    pub symbols_inserted: usize,
    pub edges_inserted: usize,
    pub calls_resolved: usize,
    pub calls_unresolved: usize,
}

/// Index a single source file into the store atomically.
///
/// Steps:
/// 1. Read source, detect language, parse with tree-sitter
/// 2. Extract symbols + calls + imports from AST
/// 3. Build a function registry from existing store symbols + new symbols
/// 4. Resolve calls using the 6-strategy cascade
/// 5. In one SQLite transaction: delete old symbols for the file, upsert file,
///    insert nodes (symbols), insert edges (resolved calls), insert import edges
pub fn index_file(store: &mut Store, file_path: &Path) -> Result<IndexFileResult> {
    let source = std::fs::read_to_string(file_path)?;
    let lang = Language::from_path(file_path)
        .ok_or_else(|| anyhow::anyhow!("unsupported file type: {:?}", file_path))?;

    // 1. Extract symbols + calls + imports
    let mut extraction = extractor::extract(&source, lang, &file_path.to_string_lossy())?;

    // Resolve relative import paths to absolute file paths so they match
    // the file_path convention used in upsert_file and qualified_name.
    resolve_module_paths(&mut extraction.module_map, file_path);

    // 2. Build function registry from existing nodes + new ones
    let mut registry = build_registry(store, &file_path.to_string_lossy())?;
    for sym in &extraction.symbols {
        registry.register(sym.clone());
    }

    // 3. Resolve calls
    let resolved = resolver::resolve_calls(
        &extraction.calls,
        &registry,
        &extraction.module_map,
        &file_path.to_string_lossy(),
    );

    let calls_resolved = resolved
        .iter()
        .filter(|r| r.target_qualified.is_some())
        .count();
    let calls_unresolved = resolved.len() - calls_resolved;

    // 4. Compute content hash (xxh3 128-bit)
    let content_hash = xxhash128_bytes(source.as_bytes());
    let metadata = std::fs::metadata(file_path)?;
    let mtime = metadata
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;

    // 5. Begin transaction and persist everything atomically
    let txn = store.transaction()?;

    // Delete old symbols for this file (if it was indexed before)
    if let Some(old_file_id) = txn.get_file_id(&file_path.to_string_lossy())? {
        txn.delete_file_symbols(old_file_id)?;
    }

    // Upsert file record
    let file_id = txn.upsert_file(&FileRecord {
        path: file_path.to_string_lossy().into(),
        language: lang.as_str().into(),
        content_hash: content_hash.clone(),
        mtime,
        size: source.len() as i64,
    })?;

    // Insert symbol nodes
    let node_recs: Vec<NodeRecord> = extraction
        .symbols
        .iter()
        .map(|sym| symbol_to_node_record(sym, file_id))
        .collect();
    let node_ids = txn.insert_nodes_batch(&node_recs)?;

    // Build qualified_name → node_id lookup
    let mut qn_to_id: HashMap<String, i64> = HashMap::with_capacity(extraction.symbols.len());
    for (sym, id) in extraction.symbols.iter().zip(node_ids.iter()) {
        qn_to_id.insert(sym.qualified_name.clone(), *id);
    }

    // Insert edges for resolved calls
    // resolved.iter() is index-aligned with extraction.calls (resolve_calls uses .iter().map().collect())
    let edge_recs: Vec<EdgeRecord> = extraction
        .calls
        .iter()
        .zip(resolved.iter())
        .filter_map(|(call, r)| {
            // Skip unresolved calls — only persist edges with a known target
            if r.target_qualified.is_none() {
                return None;
            }
            let source_id = *qn_to_id.get(call.caller_qualified.as_ref()?)?;

            // Look up target_id: first try this file's new nodes, then fall back
            // to the store for cross-file targets (e.g., import-resolved calls).
            let target_id = if let Some(qn) = &r.target_qualified {
                if let Some(id) = qn_to_id.get(qn) {
                    Some(*id)
                } else {
                    // Cross-file target — look it up in the store within this transaction
                    txn.find_node_by_qualified(qn).ok().flatten().map(|n| n.id)
                }
            } else {
                None
            };

            Some(EdgeRecord {
                source_id,
                target_id,
                edge_type: "calls".into(),
                confidence: r.confidence,
                file_id: Some(file_id),
                line: Some(call.line as i32),
                metadata: Some(format!("{:?}", r.strategy)),
            })
        })
        .collect();

    let edges_count = edge_recs.len();
    txn.insert_edges_batch(&edge_recs)?;

    // Insert import edges (reverse-dependency map)
    // For each imported module path, find its file_id and record the edge
    for module_path in extraction.module_map.module_paths.iter() {
        if let Some(imported_file_id) = txn.get_file_id(module_path)? {
            txn.insert_import_edge(&ImportEdgeRecord {
                importer_file_id: file_id,
                imported_file_id,
            })?;
        }
        // ponytail: if the imported file isn't indexed yet, we skip recording
        // the import edge. It'll be recorded when builder re-runs after that
        // file is indexed. Two-pass indexing handles this naturally.
    }

    txn.commit()?;

    Ok(IndexFileResult {
        file_path: file_path.to_string_lossy().into(),
        language: lang.as_str().into(),
        symbols_inserted: extraction.symbols.len(),
        edges_inserted: edges_count,
        calls_resolved,
        calls_unresolved,
    })
}

/// Index an entire project root. Walks the directory tree, parses each file
/// to build the import graph, topologically sorts by dependencies (producers
/// before consumers), then indexes in topo order.
/// ponytail: parses each file twice (once for import graph, once for indexing).
/// Could cache parse results to halve parse cost, but tree-sitter is fast enough.
pub fn index_full(store: &mut Store, project_root: &Path) -> Result<Vec<IndexFileResult>> {
    // 1. Collect all source files.
    // Canonicalize root so all paths are absolute+normalized — must match
    // the absolute paths resolve_module_paths produces off file.parent().
    let root = std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                warn!("skipping directory {:?}: {e}", dir);
                continue;
            }
        };
        for entry in entries {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            if name_str.starts_with('.')
                || matches!(
                    name_str.as_ref(),
                    "node_modules" | "target" | "dist" | "build" | "vendor"
                )
            {
                continue;
            }

            if path.is_dir() {
                stack.push(path);
            } else if Language::from_path(&path).is_some() {
                files.push(path);
            }
        }
    }
    files.sort();
    let import_deps = build_import_deps(&files);
    let order = topo_sort(&files, &import_deps);
    // 3. Index in topo order. Producers' nodes are committed before consumers.
    // Skip files that fail (binary, encoding, parse) — don't abort the whole index.
    let mut results = Vec::new();
    for p in &order {
        match index_file(store, p) {
            Ok(r) => results.push(r),
            Err(e) => {
                warn!("skipping file {:?}: {e}", p);
                continue;
            }
        }
    }
    Ok(results)
}

/// Parse each file to extract its imports (parse-only, no store writes).
/// Returns a map of file → its imported file targets (only those in `files`).
/// Used by both `index_full` and `index_incremental` for topo-sort ordering.
fn build_import_deps(files: &[PathBuf]) -> HashMap<PathBuf, Vec<PathBuf>> {
    let file_set: HashSet<&PathBuf> = files.iter().collect();
    let mut deps = HashMap::new();
    for p in files {
        let source = match std::fs::read_to_string(p) {
            Ok(s) => s,
            Err(_) => {
                deps.insert(p.clone(), Vec::new());
                continue;
            }
        };
        let Some(lang) = Language::from_path(p) else {
            deps.insert(p.clone(), Vec::new());
            continue;
        };
        let mut extraction = match extractor::extract(&source, lang, &p.to_string_lossy()) {
            Ok(e) => e,
            Err(_) => {
                deps.insert(p.clone(), Vec::new());
                continue;
            }
        };
        resolve_module_paths(&mut extraction.module_map, p);
        let file_deps: Vec<PathBuf> = extraction
            .module_map
            .module_paths
            .iter()
            .filter_map(|mp| {
                let dep = PathBuf::from(mp);
                if file_set.contains(&dep) {
                    Some(dep)
                } else {
                    None
                }
            })
            .collect();
        deps.insert(p.clone(), file_deps);
    }
    deps
}
/// Topological sort by import dependencies (Kahn's algorithm).
/// Files with no dependents come first; files importing from them come later.
/// ponytail: cycles (circular imports) get appended in sorted order at the end.
fn topo_sort(files: &[PathBuf], deps: &HashMap<PathBuf, Vec<PathBuf>>) -> Vec<PathBuf> {
    // Count how many deps each file has (outgoing edges = imports)
    let mut pending_deps: HashMap<&PathBuf, usize> = HashMap::new();
    let mut dependents: HashMap<&PathBuf, Vec<&PathBuf>> = HashMap::new();
    for f in files {
        let deps_count = deps.get(f).map(|d| d.len()).unwrap_or(0);
        pending_deps.insert(f, deps_count);
        if let Some(d) = deps.get(f) {
            for dep in d {
                dependents.entry(dep).or_default().push(f);
            }
        }
    }

    // Start with files that have no imports (producers)
    let mut queue: VecDeque<&PathBuf> = files
        .iter()
        .filter(|f| *pending_deps.get(f).unwrap() == 0)
        .collect();
    let mut result = Vec::new();
    while let Some(f) = queue.pop_front() {
        result.push(f.clone());
        if let Some(deps_list) = dependents.get(f) {
            let deps_vec: Vec<&PathBuf> = deps_list.iter().copied().collect();
            for dependent in deps_vec {
                let d = pending_deps.get_mut(dependent).unwrap();
                *d -= 1;
                if *d == 0 {
                    queue.push_back(dependent);
                }
            }
        }
    }

    // Remaining files have cycles — append in sorted order
    for f in files {
        if !result.contains(f) {
            result.push(f.clone());
        }
    }
    result
}

/// Index only changed files plus their reverse-dependents (transitive importers).
/// A changed export can change resolution for importing files — without this,
/// the graph silently rots when exports change.
pub fn index_incremental(
    store: &mut Store,
    changed: Vec<std::path::PathBuf>,
) -> Result<Vec<IndexFileResult>> {
    let mut to_index: HashSet<std::path::PathBuf> = changed.into_iter().collect();
    let mut visited: HashSet<i64> = HashSet::new();
    let mut queue: VecDeque<std::path::PathBuf> = to_index.iter().cloned().collect();

    // BFS: expand each changed file's reverse-dependents transitively
    while let Some(p) = queue.pop_front() {
        let Some(fid) = store.get_file_id(&p.to_string_lossy())? else {
            continue;
        };
        if !visited.insert(fid) {
            continue;
        }
        for importer_id in store.get_reverse_deps(fid)? {
            if let Some(importer_path) = store.get_file_path(importer_id)? {
                let ip = std::path::PathBuf::from(importer_path);
                if to_index.insert(ip.clone()) {
                    queue.push_back(ip);
                }
            }
        }
    }

    // Topo-sort the expanded set by import deps (producers before consumers).
    // Uses build_import_deps (parse-based) — works even for files not yet in
    // the store (no import_edges rows). Avoids the FK cascade bug where
    // alphabetical sort indexes main.ts before math.ts.
    let sorted_files: Vec<PathBuf> = to_index.into_iter().collect();
    let import_deps = build_import_deps(&sorted_files);
    let sorted = topo_sort(&sorted_files, &import_deps);

    let mut results = Vec::new();
    for p in &sorted {
        match index_file(store, p) {
            Ok(r) => results.push(r),
            Err(e) => {
                warn!("skipping file {:?}: {e}", p);
                continue;
            }
        }
    }
    Ok(results)
}

/// Build a function registry from symbols already in the store.
fn build_registry(store: &Store, current_file_path: &str) -> Result<FunctionRegistry> {
    let mut registry = FunctionRegistry::new();

    // Load all nodes from the store (except those for the current file,
    // which will be replaced)
    // ponytail: for v1, we load all nodes. For large projects this should be
    // scoped to the file's package + its import targets. Upgrade: add
    // Store::get_all_nodes() with a WHERE clause filtering out current file.
    let conn = store.conn();
    let mut stmt = conn.prepare(
        "SELECT kind, name, qualified_name, file_id, line_start, line_end,
            col_start, signature, return_type, receiver_type, is_exported,
            complexity, decorators
         FROM nodes ORDER BY file_id, line_start",
    )?;

    let rows: Vec<(
        String,
        String,
        String,
        i64,
        i32,
        i32,
        Option<i32>,
        Option<String>,
        Option<String>,
        Option<String>,
        bool,
        i32,
        Option<String>,
    )> = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
                row.get::<_, i32>(10)? != 0,
                row.get(11)?,
                row.get(12)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    // We need file paths for each file_id to set SymbolDef.file_path correctly
    let mut file_id_to_path: HashMap<i64, String> = HashMap::new();
    {
        let mut path_stmt = conn.prepare("SELECT id, path FROM files")?;
        let path_rows = path_stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in path_rows {
            let (id, path) = row?;
            file_id_to_path.insert(id, path);
        }
    }

    for row in rows {
        let (
            kind,
            name,
            qualified_name,
            file_id,
            line_start,
            line_end,
            _col_start,
            signature,
            return_type,
            receiver_type,
            is_exported,
            complexity,
            decorators,
        ) = row;

        // Skip nodes for the current file (they'll be replaced)
        let file_path = file_id_to_path.get(&file_id).cloned().unwrap_or_default();
        if file_path == current_file_path {
            continue;
        }

        registry.register(SymbolDef {
            kind: parse_symbol_kind(&kind),
            name,
            qualified_name,
            file_path,
            line_start: line_start as usize,
            line_end: line_end as usize,
            signature,
            return_type,
            receiver_type,
            is_exported,
            complexity: complexity as u32,
            decorators: decorators
                .unwrap_or_default()
                .split('\n')
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect(),
        });
    }

    Ok(registry)
}

fn parse_symbol_kind(s: &str) -> SymbolKind {
    match s {
        "function" => SymbolKind::Function,
        "method" => SymbolKind::Method,
        "class" => SymbolKind::Class,
        "interface" => SymbolKind::Interface,
        "enum" => SymbolKind::Enum,
        "type" => SymbolKind::TypeAlias,
        "variable" => SymbolKind::Variable,
        _ => SymbolKind::Function,
    }
}

fn symbol_to_node_record(sym: &SymbolDef, file_id: i64) -> NodeRecord {
    NodeRecord {
        kind: sym.kind.as_str().into(),
        name: sym.name.clone(),
        qualified_name: sym.qualified_name.clone(),
        file_id,
        line_start: sym.line_start as i32,
        line_end: sym.line_end as i32,
        col_start: None,
        signature: sym.signature.clone(),
        return_type: sym.return_type.clone(),
        receiver_type: sym.receiver_type.clone(),
        is_exported: sym.is_exported,
        complexity: sym.complexity as i32,
        decorators: if sym.decorators.is_empty() {
            None
        } else {
            Some(sym.decorators.join("\n"))
        },
        metadata: None,
    }
}

/// Resolve relative import paths in a ModuleMap to absolute filesystem paths,
/// matching the file_path convention used in upsert_file and SymbolDef.file_path.
/// E.g. "./math" imported from "/abs/dir/main.ts" → "/abs/dir/math.ts".
/// Non-relative imports (e.g. "express", "react") are left unchanged.
fn resolve_module_paths(map: &mut ModuleMap, file_path: &Path) {
    let parent = match file_path.parent() {
        Some(p) => p,
        None => return,
    };

    // Rewrite imports: local_name → source_module_path
    let old_imports = std::mem::take(&mut map.imports);
    for (local_name, module_path) in old_imports {
        let resolved = resolve_one_path(&module_path, parent, file_path);
        map.imports.insert(local_name, resolved);
    }

    // Rewrite module_paths
    let old_paths = std::mem::take(&mut map.module_paths);
    for module_path in old_paths {
        let resolved = resolve_one_path(&module_path, parent, file_path);
        if !map.module_paths.contains(&resolved) {
            map.module_paths.push(resolved);
        }
    }
}

/// Resolve a single module path: relative → absolute with extension.
/// Extension is determined by the importing file's language (.ts for TS/TSX, .js for JS/JSX).
/// Lexically normalizes `.` and `..` components without filesystem access.
fn resolve_one_path(module_path: &str, parent: &Path, file_path: &Path) -> String {
    // Only resolve relative imports (starts with ./ or ../)
    if !module_path.starts_with("./") && !module_path.starts_with("../") {
        return module_path.to_string();
    }

    let joined = parent.join(module_path);

    // Append extension if missing (import specifiers omit extensions).
    // Use the importing file's language to pick .ts vs .js.
    let ext = match crate::parser::Language::from_path(file_path) {
        Some(crate::parser::Language::TypeScript) => "ts",
        Some(crate::parser::Language::Tsx) => "tsx",
        Some(crate::parser::Language::JavaScript) => "js",
        Some(crate::parser::Language::Jsx) => "jsx",
        Some(crate::parser::Language::Python) => "py",
        Some(crate::parser::Language::Rust) => "rs",
        Some(crate::parser::Language::Go) => "go",
        Some(crate::parser::Language::Java) => "java",
        Some(crate::parser::Language::CSharp) => "cs",
        Some(crate::parser::Language::Ruby) => "rb",
        Some(crate::parser::Language::Php) => "php",
        Some(crate::parser::Language::C) => "c",
        Some(crate::parser::Language::Cpp) => "cpp",
        None => "ts", // unknown defaults to .ts
    };
    let with_ext = if joined.extension().is_none() {
        joined.with_extension(ext)
    } else {
        joined
    };

    // Lexically normalize: strip CurDir, pop on ParentDir.
    let mut out = std::path::PathBuf::new();
    for c in with_ext.components() {
        match c {
            std::path::Component::ParentDir if out.parent().is_some() => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out.to_string_lossy().into_owned()
}

/// Fast content hash using xxh3 (128-bit).
pub fn xxhash128_bytes(data: &[u8]) -> Vec<u8> {
    let hash = xxhash_rust::xxh3::xxh3_128(data);
    hash.to_le_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_ts_fixture(source: &str, path: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("kernava_builder_test");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join(path);
        let mut f = std::fs::File::create(&file_path).unwrap();
        f.write_all(source.as_bytes()).unwrap();
        file_path
    }

    fn make_ts_fixture_in(source: &str, dir: &Path, name: &str) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let file_path = dir.join(name);
        let mut f = std::fs::File::create(&file_path).unwrap();
        f.write_all(source.as_bytes()).unwrap();
        file_path
    }

    #[test]
    fn test_index_simple_function() {
        let mut store = Store::open_in_memory().unwrap();
        let fx = make_ts_fixture(
            "function add(a: number, b: number): number { return a + b; }",
            "add.ts",
        );
        let result = index_file(&mut store, &fx).unwrap();

        assert_eq!(result.symbols_inserted, 1);
        assert_eq!(result.calls_resolved, 0);
        assert_eq!(result.calls_unresolved, 0);

        let nodes = store
            .find_node_by_qualified(&format!("{}.add", fx.to_string_lossy()))
            .unwrap();
        assert!(nodes.is_some());
    }

    #[test]
    fn test_index_class_with_methods() {
        let mut store = Store::open_in_memory().unwrap();
        let fx = make_ts_fixture(
            "class Calculator { add(a: number, b: number): number { return a + b; } }",
            "calc.ts",
        );
        let result = index_file(&mut store, &fx).unwrap();

        // 1 class + 1 method
        assert_eq!(result.symbols_inserted, 2);
    }

    #[test]
    fn test_index_with_calls_and_resolution() {
        let mut store = Store::open_in_memory().unwrap();
        let dir = std::env::temp_dir().join("kernava_call_test");
        let _ = std::fs::remove_dir_all(&dir);

        // First index the "library" file with an exported function
        let lib_path = make_ts_fixture_in(
            "export function greet(name: string): string { return name; }",
            &dir,
            "lib.ts",
        );
        let lib_result = index_file(&mut store, &lib_path).unwrap();
        assert_eq!(lib_result.symbols_inserted, 1);

        // Then index the "caller" file which imports and calls greet
        let caller_path = make_ts_fixture_in(
            r#"
            import { greet } from './lib';
            function main() {
              const result = greet("world");
            }
            "#,
            &dir,
            "main.ts",
        );
        let result = index_file(&mut store, &caller_path).unwrap();

        assert!(result.symbols_inserted >= 1); // at least main()
                                               // greet() should be resolved via import map strategy
        assert!(
            result.calls_resolved >= 1,
            "expected at least 1 resolved call, got {} resolved / {} unresolved",
            result.calls_resolved,
            result.calls_unresolved
        );
    }

    #[test]
    fn test_index_updates_on_reindex() {
        let mut store = Store::open_in_memory().unwrap();
        let dir = std::env::temp_dir().join("kernava_reindex_test");
        let _ = std::fs::remove_dir_all(&dir);

        let path = make_ts_fixture_in("function foo() { return 1; }", &dir, "reindex.ts");
        let r1 = index_file(&mut store, &path).unwrap();
        assert_eq!(r1.symbols_inserted, 1);

        // Re-index with different content
        std::fs::write(
            &path,
            "function bar() { return 2; }\nfunction baz() { return 3; }",
        )
        .unwrap();
        let r2 = index_file(&mut store, &path).unwrap();
        // Should have replaced old symbols — 2 new symbols, old one gone
        assert_eq!(r2.symbols_inserted, 2);

        let nodes = store
            .get_nodes_for_file(store.get_file_id(&path.to_string_lossy()).unwrap().unwrap())
            .unwrap();
        assert_eq!(nodes.len(), 2);
        assert!(nodes.iter().any(|n| n.name == "bar"));
        assert!(nodes.iter().any(|n| n.name == "baz"));
        assert!(!nodes.iter().any(|n| n.name == "foo"));
    }

    #[test]
    fn test_resolve_one_path_strips_dotsegments() {
        let parent = std::path::Path::new("/abs/dir");
        // TS: default .ts extension
        assert_eq!(
            resolve_one_path("./math", parent, std::path::Path::new("/abs/dir/main.ts")),
            "/abs/dir/math.ts"
        );
        assert_eq!(
            resolve_one_path("../sibling/util", parent, std::path::Path::new("/abs/dir/main.ts")),
            "/abs/sibling/util.ts"
        );
        // JS: .js extension
        assert_eq!(
            resolve_one_path("./math", parent, std::path::Path::new("/abs/dir/main.js")),
            "/abs/dir/math.js"
        );
        // TSX: .tsx extension
        assert_eq!(
            resolve_one_path("./Foo", parent, std::path::Path::new("/abs/dir/app.tsx")),
            "/abs/dir/Foo.tsx"
        );
        // JSX: .jsx extension
        assert_eq!(
            resolve_one_path("./Foo", parent, std::path::Path::new("/abs/dir/app.jsx")),
            "/abs/dir/Foo.jsx"
        );
        // Non-relative imports left unchanged
        assert_eq!(
            resolve_one_path("express", parent, std::path::Path::new("/abs/dir/main.ts")),
            "express"
        );
        // Already has extension — no extension appended
        assert_eq!(
            resolve_one_path("./math.ts", parent, std::path::Path::new("/abs/dir/main.ts")),
            "/abs/dir/math.ts"
        );
    }

    #[test]
    fn test_index_full_skips_binary_file() {
        let dir = std::env::temp_dir().join(format!(
            "kernava_binary_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Valid TS file
        std::fs::write(dir.join("valid.ts"), "export function foo() { return 1; }").unwrap();
        // Binary file with .ts extension (invalid UTF-8)
        std::fs::write(dir.join("binary.ts"), b"\xff\xfe\x00\x01\x02\x03").unwrap();

        let mut store = Store::open_in_memory().unwrap();
        let results = index_full(&mut store, &dir).unwrap();

        // Should index the valid file, skip the binary one
        assert_eq!(results.len(), 1, "should index 1 file, skip binary");
        assert_eq!(results[0].file_path, dir.join("valid.ts").to_string_lossy());
        assert_eq!(results[0].symbols_inserted, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_index_full_empty_project() {
        let dir = std::env::temp_dir().join(format!(
            "kernava_empty_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut store = Store::open_in_memory().unwrap();
        let results = index_full(&mut store, &dir).unwrap();
        assert!(results.is_empty(), "empty project should produce zero results");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

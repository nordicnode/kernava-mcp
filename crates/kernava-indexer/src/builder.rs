#![allow(clippy::type_complexity)]
// kernava-indexer: builder orchestrates parse → extract → resolve → upsert
// P1 task 1.9: single-file indexing in one atomic SQLite transaction.

use crate::extractor::{self, SymbolDef, SymbolKind};
use crate::languages::ModuleMap;
use crate::parser::Language;
use crate::resolver::{self, FunctionRegistry};
use anyhow::Result;
use kernava_store::{EdgeRecord, FileRecord, ImportEdgeRecord, NodeRecord, Store};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use tracing::warn;

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
    index_file_with_config(store, file_path, &Default::default())
}

/// Index a single file with config. Checks max_file_size via metadata before
/// reading — bails cheaply on oversized files without allocating a String.
pub fn index_file_with_config(
    store: &mut Store,
    file_path: &Path,
    config: &crate::config::IndexerConfig,
) -> Result<IndexFileResult> {
    let mut registry = build_registry(store, &file_path.to_string_lossy())?;
    index_file_inner(store, file_path, config, &mut registry)
}

/// Internal: index a file with a pre-built registry (avoids O(N²) rebuild).
/// The registry is built once in `index_full_with_config` and passed `&mut`
/// across the loop — each file's new symbols are registered before resolving
/// calls, so cross-file resolution works without re-scanning the store.
fn index_file_inner(
    store: &mut Store,
    file_path: &Path,
    config: &crate::config::IndexerConfig,
    registry: &mut resolver::FunctionRegistry,
) -> Result<IndexFileResult> {
    // Size gate — check metadata before reading to avoid allocating a huge String.
    let metadata = std::fs::metadata(file_path)?;
    if metadata.len() > config.max_file_size as u64 {
        anyhow::bail!(
            "file exceeds max_file_size ({} > {} bytes)",
            metadata.len(),
            config.max_file_size
        );
    }
    let source = std::fs::read_to_string(file_path)?;
    let lang = Language::from_path(file_path)
        .ok_or_else(|| anyhow::anyhow!("unsupported file type: {:?}", file_path))?;

    // 1. Extract symbols + calls + imports
    let mut extraction = extractor::extract(&source, lang, &file_path.to_string_lossy())?;

    // Resolve relative import paths to absolute file paths so they match
    // the file_path convention used in upsert_file and qualified_name.
    resolve_module_paths(&mut extraction.module_map, file_path);

    // 2. Register new symbols into the shared registry.
    // Registry is built once in index_full_with_config and reused — O(N) total.
    for sym in &extraction.symbols {
        registry.register(sym.clone());
    }

    // 3. Resolve calls
    let resolved = resolver::resolve_calls(
        &extraction.calls,
        registry,
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
    // Reuse metadata from size gate above — avoid redundant syscall.
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
            r.target_qualified.as_ref()?;
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
    index_full_with_config(
        store,
        project_root,
        &crate::config::IndexerConfig::default(),
    )
}

/// Index an entire project root with config. Walks the directory tree,
/// applies custom ignore globs + max_file_size filter, parses each file
/// to build the import graph, topologically sorts by dependencies (producers
/// before consumers), then indexes in topo order.
/// ponytail: parses each file twice (once for import graph, once for indexing).
/// Could cache parse results to halve parse cost, but tree-sitter is fast enough.
pub fn index_full_with_config(
    store: &mut Store,
    project_root: &Path,
    config: &crate::config::IndexerConfig,
) -> Result<Vec<IndexFileResult>> {
    // 1. Collect all source files.
    // Canonicalize root so all paths are absolute+normalized — must match
    // the absolute paths resolve_module_paths produces off file.parent().
    let root = std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    // Use ignore crate for .gitignore-aware file discovery.
    // Replaces hand-rolled skip list (.git, node_modules, target, etc.)
    // with proper gitignore + .ignore file support.
    let mut builder = ignore::WalkBuilder::new(&root);
    builder
        .hidden(true)
        .git_ignore(true)
        .ignore(true)
        // Native size limit — skips oversized files at walk level.
        .max_filesize(Some(config.max_file_size as u64))
        // Symlink following is opt-in — default false prevents cycles.
        .follow_links(config.follow_symlinks);
    // Build custom ignore matcher from config globs.
    // ponytail: file-level filter only — doesn't prune directory descent.
    // Upgrade: write globs to a temp .ignore file and pass to add_ignore().
    let matcher = if config.ignore.is_empty() {
        None
    } else {
        let mut gi = ignore::gitignore::GitignoreBuilder::new(&root);
        for pat in &config.ignore {
            if let Err(e) = gi.add_line(None, pat) {
                warn!("invalid ignore glob {:?}: {e}", pat);
            }
        }
        gi.build().ok()
    };
    for entry in builder.build() {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !path.is_file() || Language::from_path(path).is_none() {
            continue;
        }
        // Apply custom ignore globs from config.
        if let Some(m) = &matcher {
            if m.matched(path, path.is_dir()).is_ignore() {
                continue;
            }
        }
        files.push(path.to_path_buf());
    }
    files.sort();
    let import_deps = build_import_deps(&files, config);
    let order = topo_sort(&files, &import_deps);
    // 3. Index in topo order. Producers' nodes are committed before consumers.
    // Seed the registry ONCE from the store so that:
    //   - files skipped this run (oversized/unreadable/parse-error) keep their
    //     symbols resolvable by later files (cross-file edges don't rot), and
    //   - re-index runs on a warm DB don't lose already-persisted targets.
    // O(1) store scan here vs the old O(N²) that called build_registry per file.
    // `register` is idempotent, so re-registering this run's new symbols just
    // overwrites the seeded entries with the fresh definitions.
    let mut registry = build_registry(store, "")?;
    let mut results = Vec::new();
    for p in &order {
        match index_file_inner(store, p, config, &mut registry) {
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
fn build_import_deps(
    files: &[PathBuf],
    config: &crate::config::IndexerConfig,
) -> HashMap<PathBuf, Vec<PathBuf>> {
    let file_set: HashSet<&PathBuf> = files.iter().collect();
    let mut deps = HashMap::new();
    for p in files {
        // Skip oversized / unreadable files at the metadata level — matches the
        // gate in index_file_inner and avoids a wasteful read+parse of a file
        // that will be skipped at index time anyway.
        let oversized = match std::fs::metadata(p) {
            Ok(m) => m.len() > config.max_file_size as u64,
            Err(_) => true, // unreadable → treat like oversized (no deps)
        };
        if oversized {
            deps.insert(p.clone(), Vec::new());
            continue;
        }
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
    let mut result: Vec<PathBuf> = Vec::with_capacity(files.len());
    // Track which files Kahn's algorithm already emitted, so the cycle-tail
    // loop is O(N) total instead of O(N²) via result.contains(f). Files in
    // the tail are exactly `!visited` after the queue drains.
    let mut visited: HashSet<&PathBuf> = HashSet::with_capacity(files.len());
    while let Some(f) = queue.pop_front() {
        visited.insert(f);
        result.push(f.clone());
        if let Some(deps_list) = dependents.get(f) {
            let deps_vec: Vec<&PathBuf> = deps_list.to_vec();
            for dependent in deps_vec {
                let d = pending_deps.get_mut(dependent).unwrap();
                *d -= 1;
                if *d == 0 {
                    queue.push_back(dependent);
                }
            }
        }
    }

    // Remaining files are in cycles — append in sorted order (O(N), single pass).
    for f in files {
        if !visited.contains(f) {
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
    index_incremental_with_config(store, changed, &crate::config::IndexerConfig::default())
}

/// Index changed files with config. Same as index_incremental but applies
/// max_file_size via index_file_with_config.
pub fn index_incremental_with_config(
    store: &mut Store,
    changed: Vec<std::path::PathBuf>,
    config: &crate::config::IndexerConfig,
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
    let import_deps = build_import_deps(&sorted_files, config);
    let sorted = topo_sort(&sorted_files, &import_deps);

    let mut results = Vec::new();
    for p in &sorted {
        match index_file_with_config(store, p, config) {
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

    /// Regression: shared-registry must not lose symbols when a producer file
    /// is skipped on a re-index (oversized), or cross-file edges from consumers
    /// into that producer regress from resolved → NULL target.
    #[test]
    fn test_index_full_skipped_producer_keeps_resolved_edges_on_reindex() {
        use crate::config::IndexerConfig;
        use std::fs;

        let dir = std::env::temp_dir().join("kernava_reindex_skip_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // lib.ts: exports greet; main.ts imports + calls greet.
        fs::write(
            dir.join("lib.ts"),
            "export function greet() { return 1; }\n",
        )
        .unwrap();
        fs::write(
            dir.join("main.ts"),
            "import { greet } from './lib';\nfunction main() { greet(); }\n",
        )
        .unwrap();

        let mut store = Store::open_in_memory().unwrap();

        // Run 1: both files index. main.ts → lib.greet edge resolves.
        {
            let config = IndexerConfig {
                max_file_size: 1_048_576,
                ignore: vec![],
                follow_symlinks: false,
            };
            let r = index_full_with_config(&mut store, &dir, &config).unwrap();
            assert_eq!(r.len(), 2, "both files should index on run 1");
            let edges = store.get_all_edges().unwrap();
            let resolved = edges.iter().filter(|e| e.target_id.is_some()).count();
            assert!(
                resolved >= 1,
                "run 1: greet call should resolve; got {resolved} resolved edges"
            );
        }

        // Grow lib.ts past max_file_size so it gets skipped on run 2.
        // main.ts unchanged → its edge into lib.greet must STAY resolved.
        let big = "x".repeat(2048);
        fs::write(
            dir.join("lib.ts"),
            format!("export function greet() {{ return 1; }}\n// {big}"),
        )
        .unwrap();
        {
            let config = IndexerConfig {
                max_file_size: 1024, // lib.ts now exceeds → skipped
                ignore: vec![],
                follow_symlinks: false,
            };
            let r = index_full_with_config(&mut store, &dir, &config).unwrap();
            // lib.ts is skipped; main.ts re-indexed.
            assert_eq!(r.len(), 1, "lib.ts should be skipped on run 2");

            let edges = store.get_all_edges().unwrap();
            let resolved = edges.iter().filter(|e| e.target_id.is_some()).count();
            // main.ts's edge to lib.greet must still resolve because lib.greet
            // is still in the store. If the shared-registry regression is present,
            // resolve_calls returns None for greet (not in the per-run registry),
            // and the edge gets a NULL target.
            assert!(
                resolved >= 1,
                "run 2: greet call should stay resolved since lib.greet is still \
                 in the store; got {resolved} resolved edges"
            );
        }

        let _ = fs::remove_dir_all(&dir);
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
            resolve_one_path(
                "../sibling/util",
                parent,
                std::path::Path::new("/abs/dir/main.ts")
            ),
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
            resolve_one_path(
                "./math.ts",
                parent,
                std::path::Path::new("/abs/dir/main.ts")
            ),
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
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();

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
        assert!(
            results.is_empty(),
            "empty project should produce zero results"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_config_max_file_size_skips_oversized() {
        let dir = std::env::temp_dir().join(format!(
            "kernava_cfg_size_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Small file — under limit
        std::fs::write(dir.join("small.ts"), "export function s() { return 1; }").unwrap();
        // Large file — over limit (2 KB content, 1 byte limit)
        let big_content = "x".repeat(2048);
        std::fs::write(
            dir.join("big.ts"),
            format!("export function b() {{ return \"{big_content}\"; }}"),
        )
        .unwrap();

        let config = crate::config::IndexerConfig {
            max_file_size: 100,
            ignore: Vec::new(),
            follow_symlinks: false,
        };
        let mut store = Store::open_in_memory().unwrap();
        let results = index_full_with_config(&mut store, &dir, &config).unwrap();

        // Only small.ts should be indexed; big.ts skipped by max_file_size
        assert_eq!(results.len(), 1, "should index only small.ts");
        assert!(results[0].file_path.ends_with("small.ts"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_config_ignore_glob_filters_files() {
        let dir = std::env::temp_dir().join(format!(
            "kernava_cfg_ignore_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(dir.join("generated")).unwrap();

        // Normal file
        std::fs::write(dir.join("main.ts"), "export function m() { return 1; }").unwrap();
        // File in generated/ — should be ignored
        std::fs::write(
            dir.join("generated/gen.ts"),
            "export function g() { return 2; }",
        )
        .unwrap();

        let config = crate::config::IndexerConfig {
            max_file_size: 1_048_576,
            ignore: vec!["generated/**".to_string()],
            follow_symlinks: false,
        };
        let mut store = Store::open_in_memory().unwrap();
        let results = index_full_with_config(&mut store, &dir, &config).unwrap();

        // Only main.ts should be indexed; generated/gen.ts filtered by ignore glob
        assert_eq!(
            results.len(),
            1,
            "should index only main.ts, not generated/gen.ts"
        );
        assert!(results[0].file_path.ends_with("main.ts"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}

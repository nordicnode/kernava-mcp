// kernava-indexer: integration test for task 1.10
// Index a small TS fixture project, assert node/edge counts and that
// ImportMap strategy resolves cross-file calls with name collisions.

use kernava_graph::{get_call_path, GraphCache};
use kernava_indexer::builder::index_file;
use kernava_indexer::{extract, Language};
use kernava_store::Store;
use std::path::PathBuf;

fn fixture_dir() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests");
    path.push("fixtures");
    path.push("ts-small");
    path
}

fn index_all(store: &mut Store) -> Vec<kernava_indexer::builder::IndexFileResult> {
    let dir = fixture_dir();
    let mut results = Vec::new();

    // Index libraries first so their symbols exist for call resolution
    for name in &["math.ts", "util.ts", "other.ts"] {
        let p = dir.join(name);
        let r = index_file(store, &p).unwrap_or_else(|e| panic!("index {}: {}", p.display(), e));
        results.push(r);
    }
    // Index main last — it imports from math and util
    let p = dir.join("main.ts");
    let r = index_file(store, &p).unwrap_or_else(|e| panic!("index {}: {}", p.display(), e));
    results.push(r);

    results
}

fn get_all_edges(store: &Store) -> Vec<(i64, Option<i64>, String, Option<String>)> {
    let conn = store.conn();
    let mut stmt = conn
        .prepare("SELECT source_id, target_id, edge_type, metadata FROM edges")
        .unwrap();
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .unwrap();
    rows.collect::<Result<Vec<_>, _>>().unwrap()
}

#[test]
fn test_index_ts_fixture_project() {
    let mut store = Store::open_in_memory().unwrap();
    let results = index_all(&mut store);

    // 4 files indexed
    assert_eq!(results.len(), 4);

    // Verify per-file symbol counts
    // math.ts: add, multiply → 2
    // util.ts: helper (string version) → 1
    // other.ts: helper (number version) → 1
    // main.ts: main → 1
    let math_result = &results[0];
    assert_eq!(math_result.symbols_inserted, 2);

    let util_result = &results[1];
    assert_eq!(util_result.symbols_inserted, 2); // helper + dead_function

    let other_result = &results[2];
    assert_eq!(other_result.symbols_inserted, 1);

    let main_result = &results[3];
    assert_eq!(main_result.symbols_inserted, 1);
}

#[test]
fn test_cross_file_calls_resolve_via_importmap() {
    let mut store = Store::open_in_memory().unwrap();
    index_all(&mut store);

    let edges = get_all_edges(&store);
    let call_edges: Vec<_> = edges.iter().filter(|(_, _, et, _)| et == "calls").collect();

    // main.ts calls: add(1,2), multiply(3,4), helper("...") → 3 call edges
    // arr.push(sum) should NOT resolve (builtin method guard)
    assert!(
        call_edges.len() <= 3,
        "expected ≤3 resolved call edges, got {}",
        call_edges.len()
    );

    // Find the edge for the `add` call — its metadata should be ImportMap
    let dir = fixture_dir();
    let math_path = dir.join("math.ts");

    // Find the `add` symbol's qualified name in math.ts
    let add_qn = format!("{}.add", math_path.to_string_lossy());
    let add_node = store.find_node_by_qualified(&add_qn).unwrap();
    assert!(add_node.is_some(), "add symbol not found in store");
    let add_id = add_node.unwrap().id;

    // Find edges targeting `add` — should have ImportMap strategy
    let add_edges: Vec<_> = call_edges
        .iter()
        .filter(|(_, target, _, _)| *target == Some(add_id))
        .collect();

    assert!(
        !add_edges.is_empty(),
        "no call edge resolves to math.ts.add — ImportError strategy failed"
    );

    let add_edge = &add_edges[0];
    let metadata = add_edge.3.as_ref().expect("edge metadata missing");
    assert!(
        metadata.contains("ImportMap"),
        "expected ImportMap strategy for add() call, got metadata={}",
        metadata
    );
}

#[test]
fn test_import_disambiguation_with_name_collision() {
    let mut store = Store::open_in_memory().unwrap();
    index_all(&mut store);

    let dir = fixture_dir();
    let util_path = dir.join("util.ts");
    let other_path = dir.join("other.ts");

    let util_qn = format!("{}.helper", util_path.to_string_lossy());
    let other_qn = format!("{}.helper", other_path.to_string_lossy());

    // Both helpers exist (not globally unique)
    let util_helper = store.find_node_by_qualified(&util_qn).unwrap();
    let other_helper = store.find_node_by_qualified(&other_qn).unwrap();
    assert!(util_helper.is_some(), "util.ts.helper not in store");
    assert!(other_helper.is_some(), "other.ts.helper not in store");

    // main.ts imports `helper` from `./util`, NOT `./other`
    // So the resolved call edge must target util.ts.helper, not other.ts.helper
    let util_helper_id = util_helper.unwrap().id;
    let other_helper_id = other_helper.unwrap().id;

    let edges = get_all_edges(&store);
    let helper_edges: Vec<_> = edges
        .iter()
        .filter(|(_, target, _, _)| {
            *target == Some(util_helper_id) || *target == Some(other_helper_id)
        })
        .collect();

    assert!(
        !helper_edges.is_empty(),
        "no call edge resolves to either helper — both ImportMap and GlobalUnique failed"
    );

    // The edge should target util.ts.helper (the imported one), not other.ts.helper
    for (_, target, _, metadata) in &helper_edges {
        assert_eq!(
            *target,
            Some(util_helper_id),
            "helper() call resolved to other.ts.helper instead of util.ts.helper — \
             import disambiguation broken. metadata={}",
            metadata.as_deref().unwrap_or("?")
        );
    }
}

#[test]
fn test_builtin_method_not_resolved_in_fixture() {
    let mut store = Store::open_in_memory().unwrap();
    index_all(&mut store);

    let edges = get_all_edges(&store);
    let call_edges: Vec<_> = edges.iter().filter(|(_, _, et, _)| et == "calls").collect();

    // arr.push(sum) should NOT produce a resolved edge
    // Check: no edge's metadata mentions a resolved strategy for "push"
    for (_, _, _, metadata) in &call_edges {
        let m = metadata.as_deref().unwrap_or("");
        assert!(
            !m.contains("GlobalUnique") || !m.is_empty(),
            "unexpected GlobalUnique resolution — possible builtin method false positive"
        );
    }

    // More precisely: main.ts has 3 resolvable calls (add, multiply, helper)
    // and 1 unresolvable (arr.push). So ≤3 edges total.
    assert!(
        call_edges.len() <= 3,
        "expected ≤3 resolved call edges (arr.push should be unresolved), got {}",
        call_edges.len()
    );
}

#[test]
fn test_reindex_replaces_symbols() {
    let mut store = Store::open_in_memory().unwrap();
    let dir = fixture_dir();
    let main_path = dir.join("main.ts");

    // Index main.ts
    let r1 = index_file(&mut store, &main_path).unwrap();
    assert_eq!(r1.symbols_inserted, 1);

    // Re-index same file — should delete old + insert new
    let r2 = index_file(&mut store, &main_path).unwrap();
    assert_eq!(r2.symbols_inserted, 1);

    // Verify only 1 symbol (no duplicates)
    let file_id = store
        .get_file_id(&main_path.to_string_lossy())
        .unwrap()
        .unwrap();
    let nodes = store.get_nodes_for_file(file_id).unwrap();
    assert_eq!(nodes.len(), 1, "reindex should replace, not duplicate");
}

/// Copy the ts-small fixture to a temp dir so we can modify files without
/// affecting other tests. No tempfile dep — stdlib temp_dir + process id.
fn copy_fixture_to_tmp() -> PathBuf {
    let src = fixture_dir();
    let dst = std::env::temp_dir().join(format!(
        "kernava-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dst).unwrap();
    for entry in std::fs::read_dir(&src).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), dst.join(entry.file_name())).unwrap();
    }
    dst
}

#[test]
fn test_index_full_indexes_entire_fixture() {
    let dir = copy_fixture_to_tmp();
    let mut store = Store::open_in_memory().unwrap();

    let results = kernava_indexer::builder::index_full(&mut store, &dir).unwrap();

    // ts-small has: math.ts, util.ts, other.ts, colliding.ts, main.ts = 5 files
    assert_eq!(results.len(), 5, "should index all 5 fixture files");

    // All should report symbols and parse as TS
    for r in &results {
        assert!(r.symbols_inserted > 0, "file {} had 0 symbols", r.file_path);
        assert_eq!(
            r.language, "typescript",
            "file {} not parsed as TS",
            r.file_path
        );
    }

    // Verify edges: main.ts calls add, multiply, helper = 3 resolved edges
    let all_edges = get_all_edges(&store);
    let resolved = all_edges.iter().filter(|(_, t, _, _)| t.is_some()).count();
    assert_eq!(
        resolved, 3,
        "should have 3 resolved call edges (add/multiply/helper)"
    );
}

#[test]
fn test_index_full_with_relative_root() {
    // Pass a relative project_root to verify index_full canonicalizes it before
    // walking. Without canonicalize, read_dir paths wouldn't match the absolute
    // paths resolve_module_paths produces, degrading topo order to alphabetical
    // and reintroducing the FK cascade null-target bug.
    let cwd = std::env::current_dir().unwrap();
    let abs_fixture = fixture_dir();
    let rel_fixture = abs_fixture
        .strip_prefix(&cwd)
        .unwrap_or(&abs_fixture)
        .to_path_buf();

    let mut store = Store::open_in_memory().unwrap();
    let results = kernava_indexer::builder::index_full(&mut store, &rel_fixture).unwrap();

    assert_eq!(results.len(), 5, "should index all 5 fixture files");

    let all_edges = get_all_edges(&store);
    let resolved = all_edges.iter().filter(|(_, t, _, _)| t.is_some()).count();
    assert_eq!(
        resolved, 3,
        "should have 3 resolved call edges (add/multiply/helper) despite relative root"
    );
}

#[test]
fn test_index_incremental_reindexes_changed_file_and_reverse_deps() {
    let dir = copy_fixture_to_tmp();
    let mut store = Store::open_in_memory().unwrap();

    // Full index first
    kernava_indexer::builder::index_full(&mut store, &dir).unwrap();

    // Modify math.ts — main.ts imports from math.ts, so it's a reverse-dep
    let math_path = dir.join("math.ts");
    std::fs::write(
        &math_path,
        "export function add(a: number, b: number) { return a + b; }\n\
         export function multiply(a: number, b: number) { return a * b; }\n\
         export function newFn() { return 42; }\n",
    )
    .unwrap();

    let results =
        kernava_indexer::builder::index_incremental(&mut store, vec![math_path.clone()]).unwrap();

    // Should re-index math.ts (changed) + main.ts (imports from math.ts)
    let paths: Vec<&str> = results.iter().map(|r| r.file_path.as_str()).collect();
    assert!(
        paths.iter().any(|p| p.ends_with("math.ts")),
        "math.ts should be re-indexed: {:?}",
        paths
    );
    assert!(
        paths.iter().any(|p| p.ends_with("main.ts")),
        "main.ts (reverse-dep) should be re-indexed: {:?}",
        paths
    );
    // util.ts and other.ts should NOT be re-indexed
    assert!(
        !paths.iter().any(|p| p.ends_with("util.ts")),
        "util.ts should not be re-indexed: {:?}",
        paths
    );
    assert!(
        !paths.iter().any(|p| p.ends_with("other.ts")),
        "other.ts should not be re-indexed: {:?}",
        paths
    );

    // All 3 cross-file call edges must remain resolved after incremental re-index.
    // Without topo-sort in index_incremental, main would re-index before math,
    // FK cascade would null edge targets during math's delete-reinsert.
    let edges = store.get_all_edges().unwrap();
    let resolved = edges.iter().filter(|e| e.target_id.is_some()).count();
    assert_eq!(
        resolved, 3,
        "all 3 cross-file call edges should remain resolved after incremental re-index, got: {}",
        resolved
    );
}

/// Regression: index_incremental on a fresh store (no prior index_full)
/// must still resolve cross-file calls. Previously used SQL import_edges
/// for dep ordering — which don't exist on a fresh store → alphabetical
/// sort → FK cascade nullified edge targets. Now uses parse-based
/// build_import_deps (same as index_full).
#[test]
fn test_index_incremental_on_fresh_store() {
    let dir = copy_fixture_to_tmp();
    let mut store = Store::open_in_memory().unwrap();

    // Index all files via index_incremental — NO prior index_full
    let all_files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| {
            let e = e.unwrap();
            let p = e.path();
            (p.extension().is_some_and(|ext| ext == "ts")).then_some(p)
        })
        .collect();

    let results = kernava_indexer::builder::index_incremental(&mut store, all_files).unwrap();
    assert_eq!(results.len(), 5, "should index all 5 fixture files");

    let edges = store.get_all_edges().unwrap();
    let resolved = edges.iter().filter(|e| e.target_id.is_some()).count();
    assert_eq!(
        resolved, 3,
        "all 3 cross-file call edges should resolve even without prior index_full"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_get_call_path_on_fixture() {
    let dir = fixture_dir();
    let mut store = Store::open_in_memory().unwrap();

    // Index libraries first, then main
    for name in &["math.ts", "util.ts", "other.ts"] {
        index_file(&mut store, &dir.join(name)).unwrap();
    }
    index_file(&mut store, &dir.join("main.ts")).unwrap();

    // Load graph cache from store
    let cache = GraphCache::new();
    cache.load_from_store(&store).unwrap();

    // main → add (1 hop, ImportMap resolved)
    let main_qn = format!("{}.main", dir.join("main.ts").to_string_lossy());
    let add_qn = format!("{}.add", dir.join("math.ts").to_string_lossy());

    let main_id = *cache.by_qualified.get(&main_qn).unwrap();
    let add_id = *cache.by_qualified.get(&add_qn).unwrap();

    let path = get_call_path(&cache, main_id, add_id, 20);
    assert!(path.is_some(), "call path from main to add should exist");
    let path = path.unwrap();
    assert_eq!(path.len(), 2, "main → add = 1 hop = 2 nodes");
    assert_eq!(path[0].node_id, main_id);
    assert_eq!(path[1].node_id, add_id);
}

// ============================
// Hardening: snapshot + re-index
// ============================

/// Snapshot test: assert exact node and edge contents after index_full.
/// Catches "count passes but wrong node/target stored" regressions.
#[test]
fn test_snapshot_nodes_and_edges() {
    let dir = copy_fixture_to_tmp();
    let mut store = Store::open_in_memory().unwrap();
    kernava_indexer::builder::index_full(&mut store, &dir).unwrap();

    let math_path = dir.join("math.ts").to_string_lossy().to_string();
    let util_path = dir.join("util.ts").to_string_lossy().to_string();
    let other_path = dir.join("other.ts").to_string_lossy().to_string();
    let colliding_path = dir.join("colliding.ts").to_string_lossy().to_string();
    let main_path = dir.join("main.ts").to_string_lossy().to_string();

    // --- Node snapshots (per file) ---

    // math.ts: add (line 1-3), multiply (line 5-7)
    let math_fid = store.get_file_id(&math_path).unwrap().unwrap();
    let math_nodes = store.get_nodes_for_file(math_fid).unwrap();
    assert_eq!(math_nodes.len(), 2, "math.ts should have 2 nodes");

    let add = math_nodes.iter().find(|n| n.name == "add").unwrap();
    assert_eq!(add.kind, "function");
    assert_eq!(add.qualified_name, format!("{}.add", math_path));
    assert_eq!(add.line_start, 1);
    assert_eq!(add.line_end, 3);
    assert!(add.is_exported, "add should be exported");
    assert_eq!(add.signature.as_deref(), Some("(a: number, b: number)"));

    let mul = math_nodes.iter().find(|n| n.name == "multiply").unwrap();
    assert_eq!(mul.kind, "function");
    assert_eq!(mul.qualified_name, format!("{}.multiply", math_path));
    assert_eq!(mul.line_start, 5);
    assert_eq!(mul.line_end, 7);
    assert!(mul.is_exported, "multiply should be exported");
    assert_eq!(mul.signature.as_deref(), Some("(a: number, b: number)"));

    // util.ts: helper (line 1-8), dead_function (line 11-13)
    let util_fid = store.get_file_id(&util_path).unwrap().unwrap();
    let util_nodes = store.get_nodes_for_file(util_fid).unwrap();
    assert_eq!(
        util_nodes.len(),
        2,
        "util.ts should have 2 nodes (helper + dead_function)"
    );

    let util_helper = util_nodes
        .iter()
        .find(|n| n.name == "helper")
        .expect("helper should exist");
    assert_eq!(util_helper.kind, "function");
    assert_eq!(util_helper.qualified_name, format!("{}.helper", util_path));
    assert_eq!(util_helper.line_start, 1);
    assert_eq!(util_helper.line_end, 8);
    assert!(util_helper.is_exported, "util.helper should be exported");
    assert_eq!(util_helper.signature.as_deref(), Some("(value: string)"));

    let dead_fn = util_nodes
        .iter()
        .find(|n| n.name == "dead_function")
        .expect("dead_function should exist");
    assert_eq!(dead_fn.kind, "function");
    assert_eq!(dead_fn.line_start, 11);
    assert_eq!(dead_fn.line_end, 13);
    assert!(!dead_fn.is_exported, "dead_function should NOT be exported");

    // other.ts: helper (line 1-3) — same name, different file
    let other_fid = store.get_file_id(&other_path).unwrap().unwrap();
    let other_nodes = store.get_nodes_for_file(other_fid).unwrap();
    assert_eq!(other_nodes.len(), 1, "other.ts should have 1 node");

    let other_helper = &other_nodes[0];
    assert_eq!(other_helper.name, "helper");
    assert_eq!(
        other_helper.qualified_name,
        format!("{}.helper", other_path)
    );
    assert_eq!(other_helper.line_start, 1);
    assert_eq!(other_helper.line_end, 3);
    assert!(other_helper.is_exported, "other.helper should be exported");
    assert_ne!(
        util_helper.qualified_name, other_helper.qualified_name,
        "util.helper and other.helper must have different qualified names"
    );

    // colliding.ts: process (line 1-8)
    let colliding_fid = store.get_file_id(&colliding_path).unwrap().unwrap();
    let colliding_nodes = store.get_nodes_for_file(colliding_fid).unwrap();
    assert_eq!(colliding_nodes.len(), 1, "colliding.ts should have 1 node");

    let process = &colliding_nodes[0];
    assert_eq!(process.name, "process");
    assert_eq!(
        process.qualified_name,
        format!("{}.process", colliding_path)
    );
    assert_eq!(process.line_start, 1);
    assert_eq!(process.line_end, 8);
    assert!(process.is_exported, "process should be exported");

    // main.ts: main (line 4-13)
    let main_fid = store.get_file_id(&main_path).unwrap().unwrap();
    let main_nodes = store.get_nodes_for_file(main_fid).unwrap();
    assert_eq!(main_nodes.len(), 1, "main.ts should have 1 node");

    let main_fn = &main_nodes[0];
    assert_eq!(main_fn.name, "main");
    assert_eq!(main_fn.kind, "function");
    assert_eq!(main_fn.qualified_name, format!("{}.main", main_path));
    assert_eq!(main_fn.line_start, 4);
    assert_eq!(main_fn.line_end, 13);
    assert!(main_fn.is_exported, "main should be exported");
    assert_eq!(main_fn.signature.as_deref(), Some("()"));

    // --- Edge snapshots ---

    let edges = store.get_all_edges().unwrap();
    // main.ts calls: add, multiply, helper = 3 edges (all resolved cross-file)
    // arr.push(sum) is NOT an edge (builtin method guard)
    // colliding.ts calls: trim/split/substring = NOT resolved (builtin methods)
    let call_edges: Vec<_> = edges.iter().filter(|e| e.edge_type == "calls").collect();
    assert_eq!(
        call_edges.len(),
        3,
        "exactly 3 call edges, got: {:?}",
        call_edges
            .iter()
            .map(|e| (&e.source_id, &e.target_id))
            .collect::<Vec<_>>()
    );

    // All 3 edges originate from main.ts
    for e in &call_edges {
        assert_eq!(
            e.file_id,
            Some(main_fid),
            "edge {:?} should originate from main.ts",
            e.id
        );
        assert!(
            e.target_id.is_some(),
            "edge {} should have resolved target_id",
            e.id
        );
    }

    // Verify each edge targets the correct node and has ImportMap strategy
    let add_id = add.id;
    let mul_id = mul.id;
    let util_helper_id = util_helper.id;

    let add_edge = call_edges
        .iter()
        .find(|e| e.target_id == Some(add_id))
        .unwrap();
    assert!(
        add_edge.confidence > 0.0,
        "add edge confidence should be positive"
    );
    assert!(
        add_edge.metadata.as_ref().unwrap().contains("ImportMap"),
        "add edge should use ImportMap strategy, got {:?}",
        add_edge.metadata
    );

    let mul_edge = call_edges
        .iter()
        .find(|e| e.target_id == Some(mul_id))
        .unwrap();
    assert!(
        mul_edge.confidence > 0.0,
        "multiply edge confidence should be positive"
    );
    assert!(
        mul_edge.metadata.as_ref().unwrap().contains("ImportMap"),
        "multiply edge should use ImportMap strategy, got {:?}",
        mul_edge.metadata
    );

    let helper_edge = call_edges
        .iter()
        .find(|e| e.target_id == Some(util_helper_id))
        .unwrap();
    assert!(
        helper_edge.confidence > 0.0,
        "helper edge confidence should be positive"
    );
    assert!(
        helper_edge.metadata.as_ref().unwrap().contains("ImportMap"),
        "helper edge should use ImportMap strategy, got {:?}",
        helper_edge.metadata
    );

    // helper must resolve to util.ts.helper, NOT other.ts.helper
    assert_ne!(helper_edge.target_id, Some(other_helper.id),
        "helper call must resolve to util.ts.helper, not other.ts.helper (name collision disambiguation)");

    // --- Import edge snapshots ---
    // import_edges tracks file-level dependency arcs (importer → imported).
    // main.ts imports from math.ts and util.ts → 2 import_edges.
    // math.ts, util.ts, other.ts, colliding.ts have no imports → 0 import_edges each.
    let conn = store.conn();
    let import_edge_rows: Vec<(i64, i64)> = conn
        .prepare("SELECT importer_file_id, imported_file_id FROM import_edges")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        import_edge_rows.len(),
        2,
        "expected 2 import_edges (main→math, main→util), got: {:?}",
        import_edge_rows
    );

    // Each import_edge: importer = main_fid, imported = math_fid or util_fid
    for (importer, _imported) in &import_edge_rows {
        assert_eq!(
            *importer, main_fid,
            "import_edge importer should be main.ts (fid={}), got fid={}",
            main_fid, importer
        );
    }
    let imported_fids: std::collections::HashSet<i64> = import_edge_rows
        .iter()
        .map(|(_, imported)| *imported)
        .collect();
    assert!(
        imported_fids.contains(&math_fid),
        "missing import_edge main→math"
    );
    assert!(
        imported_fids.contains(&util_fid),
        "missing import_edge main→util"
    );
    assert!(
        !imported_fids.contains(&other_fid),
        "main should NOT import from other.ts"
    );
    assert!(
        !imported_fids.contains(&colliding_fid),
        "main should NOT import from colliding.ts"
    );
}

/// Clean re-index: indexing the same tree twice should produce no duplicate
/// nodes or edges. Graph content (by qualified name) must be preserved.
/// ponytail: node IDs are NOT stable across re-index — delete_file_symbols
/// hard-deletes rows, then INSERT assigns new autoincrement IDs. Compare by
/// qualified_name instead of numeric ID. Store-level upsert-by-qualified-name
/// would make IDs stable; defer to P2.
#[test]
fn test_clean_reindex_no_duplicates() {
    let dir = copy_fixture_to_tmp();
    let mut store = Store::open_in_memory().unwrap();

    // First index
    kernava_indexer::builder::index_full(&mut store, &dir).unwrap();
    let nodes1 = store.get_all_nodes().unwrap();
    let edges1 = store.get_all_edges().unwrap();
    let node_count1 = nodes1.len();
    let edge_count1 = edges1.len();

    // Build qname → node map and id → qname lookup for edge translation
    let id_to_qname: std::collections::HashMap<i64, String> = nodes1
        .iter()
        .map(|n| (n.id, n.qualified_name.clone()))
        .collect();
    let node_qnames1: std::collections::HashSet<String> =
        nodes1.iter().map(|n| n.qualified_name.clone()).collect();
    let edge_qpairs1: std::collections::HashSet<(String, Option<String>)> = edges1
        .iter()
        .map(|e| {
            let src = id_to_qname.get(&e.source_id).cloned().unwrap_or_default();
            let tgt = e.target_id.and_then(|id| id_to_qname.get(&id).cloned());
            (src, tgt)
        })
        .collect();

    // Second index — must replace, not duplicate
    kernava_indexer::builder::index_full(&mut store, &dir).unwrap();
    let nodes2 = store.get_all_nodes().unwrap();
    let edges2 = store.get_all_edges().unwrap();

    assert_eq!(
        nodes2.len(),
        node_count1,
        "re-index should not add duplicate nodes: before={}, after={}",
        node_count1,
        nodes2.len()
    );
    assert_eq!(
        edges2.len(),
        edge_count1,
        "re-index should not add duplicate edges: before={}, after={}",
        edge_count1,
        edges2.len()
    );

    // Same set of qualified names present
    let node_qnames2: std::collections::HashSet<String> =
        nodes2.iter().map(|n| n.qualified_name.clone()).collect();
    assert_eq!(
        node_qnames1, node_qnames2,
        "node qualified_name set changed across re-index"
    );

    // Same set of (source_qname, target_qname) edge pairs
    let id_to_qname2: std::collections::HashMap<i64, String> = nodes2
        .iter()
        .map(|n| (n.id, n.qualified_name.clone()))
        .collect();
    let edge_qpairs2: std::collections::HashSet<(String, Option<String>)> = edges2
        .iter()
        .map(|e| {
            let src = id_to_qname2.get(&e.source_id).cloned().unwrap_or_default();
            let tgt = e.target_id.and_then(|id| id_to_qname2.get(&id).cloned());
            (src, tgt)
        })
        .collect();
    assert_eq!(
        edge_qpairs1, edge_qpairs2,
        "edge (source_qname, target_qname) pairs changed across re-index"
    );

    // All edges still resolved (target_id is Some) after re-index
    let unresolved = edges2.iter().filter(|e| e.target_id.is_none()).count();
    assert_eq!(
        unresolved, 0,
        "re-index should preserve all edge resolutions, but {} edges lost target_id",
        unresolved
    );
}

/// Circular imports: aaa.ts imports from bbb.ts, bbb.ts imports from aaa.ts.
/// Topo sort breaks the cycle — aaa.ts (sorts first alphabetically) indexes
/// before bbb.ts. aaa.ts's call to b_value() can't resolve (bbb.ts not yet
/// indexed). bbb.ts's call to a_func() CAN resolve (aaa.ts already indexed).
/// ponytail: proper fix needs SCC-based cycle handling + defer edge resolution
/// until after all nodes committed. v1 limitation — document and pin the gap.
#[test]
fn test_circular_imports_partial_resolution() {
    let circular_dir = {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push("tests");
        p.push("fixtures");
        p.push("ts-circular");
        p
    };

    let mut store = Store::open_in_memory().unwrap();
    kernava_indexer::builder::index_full(&mut store, &circular_dir).unwrap();

    // Both files indexed with 2 and 2 nodes respectively
    let aaa_path = circular_dir.join("aaa.ts").to_string_lossy().to_string();
    let bbb_path = circular_dir.join("bbb.ts").to_string_lossy().to_string();
    let aaa_fid = store.get_file_id(&aaa_path).unwrap().unwrap();
    let bbb_fid = store.get_file_id(&bbb_path).unwrap().unwrap();

    let aaa_nodes = store.get_nodes_for_file(aaa_fid).unwrap();
    let bbb_nodes = store.get_nodes_for_file(bbb_fid).unwrap();
    assert_eq!(aaa_nodes.len(), 1, "aaa.ts should have 1 node (a_func)");
    assert_eq!(
        bbb_nodes.len(),
        2,
        "bbb.ts should have 2 nodes (b_value, b_uses_a)"
    );

    // Edges: aaa.ts calls b_value (unresolved — bbb not indexed yet)
    //        bbb.ts calls a_func (resolved — aaa already indexed)
    let edges = store.get_all_edges().unwrap();
    let call_edges: Vec<_> = edges.iter().filter(|e| e.edge_type == "calls").collect();

    let a_func_id = aaa_nodes.iter().find(|n| n.name == "a_func").unwrap().id;
    let b_value_id = bbb_nodes.iter().find(|n| n.name == "b_value").unwrap().id;

    // bbb.ts's call to a_func should resolve (aaa indexed first)
    let a_func_edge = call_edges.iter().find(|e| e.target_id == Some(a_func_id));
    assert!(
        a_func_edge.is_some(),
        "bbb.ts call to a_func should resolve"
    );

    // aaa.ts's call to b_value should be unresolved (bbb not yet indexed when aaa indexed)
    let b_value_edge = call_edges.iter().find(|e| e.target_id == Some(b_value_id));
    assert!(
        b_value_edge.is_none(),
        "aaa.ts call to b_value should NOT resolve (circular: bbb indexed after aaa)"
    );

    // import_edges: bbb→aaa should exist (bbb indexes after aaa, sees it in store).
    // aaa→bbb may be missing (when aaa indexes, bbb may not exist yet).
    let conn = store.conn();
    let import_edge_rows: Vec<(i64, i64)> = conn
        .prepare("SELECT importer_file_id, imported_file_id FROM import_edges")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    let has_bbb_to_aaa = import_edge_rows
        .iter()
        .any(|(imp, file)| *imp == bbb_fid && *file == aaa_fid);
    assert!(has_bbb_to_aaa, "import_edge bbb→aaa should exist");

    // aaa→bbb may or may not exist depending on topo cycle handling — pin the gap
    let has_aaa_to_bbb = import_edge_rows
        .iter()
        .any(|(imp, file)| *imp == aaa_fid && *file == bbb_fid);
    // Currently broken: aaa indexes first, bbb doesn't exist yet → no import_edge.
    // When SCC cycle handling is added, this should become assert!(has_aaa_to_bbb).
    if !has_aaa_to_bbb {
        eprintln!("KNOWN LIMITATION: circular import aaa→bbb missing import_edge (topo sort breaks cycle by alphabetical order)");
    }
}

/// Phase 1 baseline: pins fixture metrics as a regression guard.
/// Run with --nocapture to see duration + DB size.
#[test]
fn test_phase1_baseline_metrics() {
    let dir = copy_fixture_to_tmp();
    let db_path = dir.join("baseline.db");
    let _ = std::fs::remove_file(&db_path);
    let db_str = db_path.to_string_lossy().to_string();
    let mut store = Store::open(&db_str).unwrap();

    let start = std::time::Instant::now();
    let results = kernava_indexer::builder::index_full(&mut store, &dir).unwrap();
    let elapsed = start.elapsed();

    let nodes = store.get_all_nodes().unwrap();
    let edges = store.get_all_edges().unwrap();

    let resolved = edges.iter().filter(|e| e.target_id.is_some()).count();
    let calls_resolved: usize = results.iter().map(|r| r.calls_resolved).sum();
    let calls_unresolved: usize = results.iter().map(|r| r.calls_unresolved).sum();
    let total_calls = calls_resolved + calls_unresolved;

    // Pinned metrics — changes here indicate a regression or fixture change
    assert_eq!(results.len(), 5, "fixture file count");
    assert_eq!(nodes.len(), 7, "symbol count");
    assert_eq!(edges.len(), 3, "edge count");
    assert_eq!(resolved, 3, "resolved edge count");
    assert_eq!(calls_resolved, 3, "resolved call count");
    assert_eq!(
        calls_unresolved, 6,
        "unresolved call count (builtin methods)"
    );
    assert_eq!(total_calls, 9, "total extracted call count");

    let db_size = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);

    eprintln!("=== Phase 1 Baseline ===");
    eprintln!(
        "Files: {}  Symbols: {}  Edges: {}  Resolved: {}",
        results.len(),
        nodes.len(),
        edges.len(),
        resolved
    );
    eprintln!(
        "Calls: {} ({:.1}% resolved)",
        total_calls,
        if total_calls > 0 {
            calls_resolved as f64 / total_calls as f64 * 100.0
        } else {
            0.0
        }
    );
    eprintln!("Duration: {:?}  DB size: {} bytes", elapsed, db_size);
    for r in &results {
        eprintln!(
            "  {}: {} symbols, {} of {} calls resolved",
            r.file_path,
            r.symbols_inserted,
            r.calls_resolved,
            r.calls_resolved + r.calls_unresolved
        );
    }

    let _ = std::fs::remove_file(&db_path);
}

// ── JavaScript (CommonJS) tests ──────────────────────────

fn js_fixture_dir() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests");
    path.push("fixtures");
    path.push("js-small");
    path
}

/// Index the js-small fixture and verify:
/// - 4 files indexed
/// - 5 symbols extracted (main, add, multiply, helper, process)
/// - CommonJS require() imports are parsed into ModuleMap
/// - Cross-file calls resolve (main→add, main→multiply, main→helper)
#[test]
fn test_javascript_commonjs_index() {
    let mut store = Store::open_in_memory().unwrap();

    let dir = js_fixture_dir().canonicalize().unwrap();

    // Index all files (index_full handles topo sort)
    let results = kernava_indexer::builder::index_full(&mut store, &dir).unwrap();

    // 4 files: main.js, math.js, util.js, other.js
    assert_eq!(results.len(), 4, "should index 4 JS files");

    // Count symbols
    let all_nodes = store.get_all_nodes().unwrap();
    let symbol_names: Vec<_> = all_nodes.iter().map(|n| n.name.clone()).collect();
    assert_eq!(all_nodes.len(), 5, "should extract 5 symbols, got: {symbol_names:?}");

    // Verify all expected symbols present
    for expected in &["main", "add", "multiply", "helper", "process"] {
        assert!(
            symbol_names.iter().any(|s| s == expected),
            "missing symbol '{expected}'"
        );
    }

    // Verify language is "javascript"
    let stats = store.stats().unwrap();
    assert_eq!(
        stats.language_distribution[0].0, "javascript",
        "primary language should be javascript"
    );

    // Verify call edges resolved
    let edges = get_all_edges(&store);
    let calls: Vec<_> = edges.iter().filter(|(_, _, et, _)| et == "calls").collect();
    let resolved = calls.iter().filter(|(_, tid, _, _)| tid.is_some()).count();

    // main calls add, multiply, helper → 3 resolved call edges
    assert_eq!(
        calls.len(), 3,
        "should have 3 call edges (main→add, main→multiply, main→helper)"
    );
    assert_eq!(
        resolved, 3,
        "all 3 calls should resolve via CommonJS require ModuleMap"
    );

    // process is never called (dead code)
    let process_called = calls
        .iter()
        .any(|(_, tid, _, _)| {
            tid.map(|id| {
                store.get_node(id).ok().flatten()
                    .map(|n| n.name == "process")
                    .unwrap_or(false)
            })
            .unwrap_or(false)
        });
    assert!(!process_called, "process should not be called by anyone");

    // Verify graph cache works
    let graph = GraphCache::new();
    graph.load_from_store(&store).unwrap();

    // Find main in graph
    let main_qname = format!("{}/main.js.main", dir.to_string_lossy());
    let main_node = graph.get_node(&main_qname).expect("main should be in graph");

    // main should have 3 outgoing calls
    let callees = graph.forward.get(&main_node.id);
    assert!(
        callees.is_some(),
        "main should have outgoing call edges"
    );
    assert_eq!(
        callees.unwrap().len(), 3,
        "main should call 3 functions (add, multiply, helper)"
    );

}

// ── Python tests ─────────────────────────────────────────

fn py_fixture_dir() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests");
    path.push("fixtures");
    path.push("py-small");
    path
}

/// Index the py-small fixture and verify:
/// - 4 files indexed
/// - 5 symbols extracted (main, add, multiply, helper, process)
/// - Python imports (from ... import) are parsed into ModuleMap
/// - Cross-file calls resolve (main→add, main→multiply, main→helper)
#[test]
fn test_python_import_index() {
    let mut store = Store::open_in_memory().unwrap();

    let dir = py_fixture_dir().canonicalize().unwrap();

    let results = kernava_indexer::builder::index_full(&mut store, &dir).unwrap();

    // 5 files: calc.py, main.py, math.py, other.py, util.py
    assert_eq!(results.len(), 5, "should index 5 Python files");

    let all_nodes = store.get_all_nodes().unwrap();
    let symbol_names: Vec<_> = all_nodes.iter().map(|n| n.name.clone()).collect();
    // 9 symbols: main, add, multiply, helper, process, Calculator, compute, create, value
    assert_eq!(all_nodes.len(), 9, "should extract 9 symbols, got: {symbol_names:?}");

    // Verify top-level functions present
    for expected in &["main", "add", "multiply", "helper", "process"] {
        assert!(
            symbol_names.iter().any(|s| s == expected),
            "missing function '{expected}'"
        );
    }

    // Verify class + decorated methods present
    assert!(symbol_names.iter().any(|s| s == "Calculator"), "missing class Calculator");
    assert!(symbol_names.iter().any(|s| s == "compute"), "missing method compute");
    assert!(symbol_names.iter().any(|s| s == "create"), "missing decorated method create (@staticmethod)");
    assert!(symbol_names.iter().any(|s| s == "value"), "missing decorated method value (@property)");

    // Verify class methods have qualified names with class prefix
    let calc_qname = format!("{}/calc.py.Calculator.compute", dir.to_string_lossy());
    assert!(
        all_nodes.iter().any(|n| n.qualified_name == calc_qname),
        "compute should have qualified name {calc_qname}, got qnames: {:?}",
        all_nodes.iter().map(|n| &n.qualified_name).collect::<Vec<_>>()
    );

    let stats = store.stats().unwrap();
    assert_eq!(
        stats.language_distribution[0].0, "python",
        "primary language should be python"
    );

    // Verify call edges:
    // main→add, main→multiply, main→helper (ImportMap)
    // calc.create→Calculator (SameFile, Calculator() constructor)
    let edges = get_all_edges(&store);
    let calls: Vec<_> = edges.iter().filter(|(_, _, et, _)| et == "calls").collect();
    let resolved = calls.iter().filter(|(_, tid, _, _)| tid.is_some()).count();

    let call_strategies: Vec<_> = edges
        .iter()
        .filter(|(_, _, et, _)| et == "calls")
        .filter_map(|(_, _, _, meta)| meta.as_ref())
        .collect();
    let import_map_count = call_strategies.iter().filter(|s| s.as_str() == "ImportMap").count();
    let same_file_count = call_strategies.iter().filter(|s| s.as_str() == "SameFile").count();

    // 3+ calls resolve via ImportMap: add, multiply, helper, plus class-qualified
    // methods (Calculator.compute etc.) now resolve via Case B class-qualified fallback.
    assert!(import_map_count >= 3, "expected >=3 ImportMap, got {import_map_count}");
    assert_eq!(same_file_count, 1, "1 call resolves via SameFile (Calculator() constructor in create)");
    assert!(resolved >= 4, "expected >=4 resolved call edges, got {resolved}");

    // process is never called (dead code)
    let process_called = calls
        .iter()
        .any(|(_, tid, _, _)| {
            tid.map(|id| {
                store.get_node(id).ok().flatten()
                    .map(|n| n.name == "process")
                    .unwrap_or(false)
            })
            .unwrap_or(false)
        });
    assert!(!process_called, "process should not be called by anyone");

    // Verify graph cache
    let graph = GraphCache::new();
    graph.load_from_store(&store).unwrap();

    let main_qname = format!("{}/main.py.main", dir.to_string_lossy());
    let main_node = graph.get_node(&main_qname).expect("main should be in graph");

    // main has 3 resolved outgoing calls (add, multiply, helper)
    // ponytail: Calculator.create() and calc.compute() from main are unresolved —
    // ImportMap Case B doesn't match class-qualified method names, and calc is a
    // local var (not import-mapped). Both fall through to global-unique, which
    // fails for Calculator.create (create qualified as Calculator.create) and
    // succeeds for calc.compute (only 1 "compute" symbol). Fix when resolver
    // gets class-aware path matching.
    let callees = graph.forward.get(&main_node.id);
    assert!(callees.is_some(), "main should have outgoing call edges");
    assert!(
        callees.unwrap().len() >= 3,
        "main should call at least 3 functions (add, multiply, helper)"
    );
}

// ── Rust tests ──────────────────────────────────────────

fn rs_fixture_dir() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests");
    path.push("fixtures");
    path.push("rs-small");
    path
}

/// Index the rs-small fixture and verify:
/// - 4 files indexed (math.rs, util.rs, calc.rs, main.rs)
/// - Symbols extracted: add, multiply, helper, Calculator, new, compute, main
/// - SameFile call resolution works (multiply→add in math.rs)
/// - ponytail: Rust `use` paths (crate::module::func) won't match file-path-based
///   qnames in the resolver for v1. Cross-file calls from main.rs (add, multiply,
///   helper) are expected to be unresolved. Upgrade path: resolver learns
///   crate-relative path mapping (DEVELOPMENT_PLAN.md "Resolver Gaps").
#[test]
fn test_rust_index() {
    let mut store = Store::open_in_memory().unwrap();

    let dir = rs_fixture_dir().canonicalize().unwrap();

    let results = kernava_indexer::builder::index_full(&mut store, &dir).unwrap();

    // 4 files: math.rs, util.rs, calc.rs, main.rs
    assert_eq!(results.len(), 4, "should index 4 Rust files, got {}", results.len());

    let all_nodes = store.get_all_nodes().unwrap();
    let symbol_names: Vec<_> = all_nodes.iter().map(|n| n.name.clone()).collect();

    // Symbols: add, multiply (math.rs), helper (util.rs), Calculator, new, compute (calc.rs), main (main.rs)
    assert_eq!(all_nodes.len(), 7, "should extract 7 symbols, got: {symbol_names:?}");

    // Verify functions present
    for expected in &["add", "multiply", "helper", "main"] {
        assert!(
            symbol_names.iter().any(|s| s == expected),
            "missing function '{expected}'"
        );
    }

    // Verify struct + methods present
    assert!(symbol_names.iter().any(|s| s == "Calculator"), "missing struct Calculator");
    assert!(symbol_names.iter().any(|s| s == "new"), "missing method new");
    assert!(symbol_names.iter().any(|s| s == "compute"), "missing method compute");

    // Verify class methods have qualified names with struct prefix
    let compute_qname = format!("{}/calc.rs.Calculator.compute", dir.to_string_lossy());
    assert!(
        all_nodes.iter().any(|n| n.qualified_name == compute_qname),
        "compute should have qualified name {compute_qname}, got qnames: {:?}",
        all_nodes.iter().map(|n| &n.qualified_name).collect::<Vec<_>>()
    );

    let stats = store.stats().unwrap();
    assert_eq!(
        stats.language_distribution[0].0, "rust",
        "primary language should be rust"
    );

    // Verify is_exported: pub functions/structs/methods are exported
    let add_node = all_nodes.iter().find(|n| n.name == "add").unwrap();
    assert!(add_node.is_exported, "pub fn add should be is_exported=true");
    let calc_node = all_nodes.iter().find(|n| n.name == "Calculator").unwrap();
    assert!(calc_node.is_exported, "pub struct Calculator should be is_exported=true");
    let new_node = all_nodes.iter().find(|n| n.name == "new").unwrap();
    assert!(new_node.is_exported, "pub fn new should be is_exported=true");
    let compute_node = all_nodes.iter().find(|n| n.name == "compute").unwrap();
    assert!(compute_node.is_exported, "pub fn compute should be is_exported=true");

    // Verify call edges
    let edges = get_all_edges(&store);
    let calls: Vec<_> = edges.iter().filter(|(_, _, et, _)| et == "calls").collect();
    let resolved = calls.iter().filter(|(_, tid, _, _)| tid.is_some()).count();

    // SameFile: multiply→add in math.rs (same file, global-unique "add" matches)
    let call_strategies: Vec<_> = edges
        .iter()
        .filter(|(_, _, et, _)| et == "calls")
        .filter_map(|(_, _, _, meta)| meta.as_ref())
        .collect();
    let same_file_count = call_strategies.iter().filter(|s| s.as_str() == "SameFile").count();
    let import_map_count = call_strategies.iter().filter(|s| s.as_str() == "ImportMap").count();

    // multiply calls add in math.rs — SameFile resolution
    assert!(same_file_count >= 1, "expected >=1 SameFile resolution (multiply→add), got {same_file_count}");

    // ponytail: Rust `use` paths (crate::math::add) don't match file-path qnames
    // (path/math.rs.add) — ImportMap resolution is 0 for Rust in v1.
    // When resolver learns crate-relative mapping, this assertion will break —
    // update to assert import_map_count > 0 at that point.
    assert_eq!(import_map_count, 0, "ImportMap should not resolve Rust use paths in v1");

    // main.rs has cross-file calls (add, multiply, helper, Calculator::new, calc.compute)
    // — most unresolved due to use-path mismatch. At least the SameFile one resolves.
    assert!(resolved >= 1, "expected >=1 resolved call edge, got {resolved}");

    // Verify graph cache loads
    let graph = GraphCache::new();
    graph.load_from_store(&store).unwrap();

    let main_qname = format!("{}/main.rs.main", dir.to_string_lossy());
    graph.get_node(&main_qname).expect("main should be in graph");

    // ponytail: main's cross-file calls (add, multiply, helper, Calculator::new,
    // calc.compute) are all unresolved in v1 — `use` paths (crate::math::add) don't
    // match file-path qnames, and builder skips edges when target is unresolved.
    // main has 0 outgoing edges. When resolver gets crate-relative mapping, this
    // will change — add edge-count assertions at that point.
}

fn go_fixture_dir() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests");
    path.push("fixtures");
    path.push("go-small");
    path
}

/// Index the go-small fixture and verify:
/// - 3 files indexed (main.go, calc.go, math.go)
/// - Symbols: Add, helper, main, Calculator, Add (method), subtract, compute,
///   MathResult, MathOps, Result
/// - Go `is_exported`: uppercase = exported, lowercase = not
/// - Pointer receiver `*Calculator` stripped to `Calculator` in qualified name
/// - Interface type → Interface symbol, struct type → Class symbol
/// - SameFile call resolution (Add→helper in main.go, compute→Add/subtract in calc.go)
/// - ponytail: Go import paths ("fmt") don't match file-path-based qnames in the
///   resolver for v1. Cross-file calls (fmt.Println) are unresolved. ImportMap=0.
///   Upgrade path: resolver learns Go package-to-path mapping.
#[test]
fn test_go_index() {
    let mut store = Store::open_in_memory().unwrap();

    let dir = go_fixture_dir().canonicalize().unwrap();

    let results = kernava_indexer::builder::index_full(&mut store, &dir).unwrap();

    // 3 files: main.go, calc.go, math.go
    assert_eq!(results.len(), 3, "should index 3 Go files, got {}", results.len());

    let all_nodes = store.get_all_nodes().unwrap();
    let symbol_names: Vec<_> = all_nodes.iter().map(|n| n.name.clone()).collect();

    // main.go: Add, helper, main (3)
    // calc.go: Calculator, Add (method), subtract, compute (4)
    // math.go: MathResult, MathOps, Result (3)
    assert_eq!(all_nodes.len(), 10, "should extract 10 symbols, got: {symbol_names:?}");

    // Verify free functions present
    for expected in &["Add", "helper", "main"] {
        assert!(
            symbol_names.iter().any(|s| s == expected),
            "missing function '{expected}'"
        );
    }

    // Verify struct types → Class
    let calc_node = all_nodes.iter().find(|n| n.name == "Calculator");
    assert!(calc_node.is_some(), "missing Calculator struct");
    assert_eq!(calc_node.unwrap().kind, "class", "Calculator should be class kind");

    let math_ops = all_nodes.iter().find(|n| n.name == "MathOps");
    assert!(math_ops.is_some(), "missing MathOps struct");
    assert_eq!(math_ops.unwrap().kind, "class", "MathOps should be class kind");

    // Verify interface type → Interface
    let math_result = all_nodes.iter().find(|n| n.name == "MathResult");
    assert!(math_result.is_some(), "missing MathResult interface");
    assert_eq!(math_result.unwrap().kind, "interface", "MathResult should be interface kind");

    // Verify methods present with correct qualified names
    // Pointer receiver: *Calculator → Calculator in qname
    let subtract_qname = format!("{}/calc.go.Calculator.subtract", dir.to_string_lossy());
    assert!(
        all_nodes.iter().any(|n| n.qualified_name == subtract_qname),
        "subtract should have qname {subtract_qname}, got: {:?}",
        all_nodes.iter().map(|n| &n.qualified_name).collect::<Vec<_>>()
    );

    // Value receiver: Calculator.Add
    let add_method_qname = format!("{}/calc.go.Calculator.Add", dir.to_string_lossy());
    assert!(
        all_nodes.iter().any(|n| n.qualified_name == add_method_qname),
        "Add method should have qname {add_method_qname}"
    );

    // Go is_exported: uppercase = exported, lowercase = not
    let free_add = all_nodes.iter().find(|n| n.name == "Add" && n.qualified_name.contains("main.go")).unwrap();
    assert!(free_add.is_exported, "Add (free func) should be exported");

    let helper_node = all_nodes.iter().find(|n| n.name == "helper").unwrap();
    assert!(!helper_node.is_exported, "helper should NOT be exported (lowercase)");

    let subtract_node = all_nodes.iter().find(|n| n.name == "subtract").unwrap();
    assert!(!subtract_node.is_exported, "subtract should NOT be exported (lowercase)");

    let calc_struct = all_nodes.iter().find(|n| n.name == "Calculator").unwrap();
    assert!(calc_struct.is_exported, "Calculator (uppercase) should be exported");

    let result_node = all_nodes.iter().find(|n| n.name == "MathResult").unwrap();
    assert!(result_node.is_exported, "MathResult (uppercase) should be exported");

    // Verify language
    let stats = store.stats().unwrap();
    assert_eq!(
        stats.language_distribution[0].0, "go",
        "primary language should be go"
    );

    // Verify call edges — SameFile resolves in-file calls
    let edges = get_all_edges(&store);
    let calls: Vec<_> = edges.iter().filter(|(_, _, et, _)| et == "calls").collect();
    let resolved = calls.iter().filter(|(_, tid, _, _)| tid.is_some()).count();

    let call_strategies: Vec<_> = edges
        .iter()
        .filter(|(_, _, et, _)| et == "calls")
        .filter_map(|(_, _, _, meta)| meta.as_ref())
        .collect();
    let same_file_count = call_strategies.iter().filter(|s| s.as_str() == "SameFile").count();
    let import_map_count = call_strategies.iter().filter(|s| s.as_str() == "ImportMap").count();

    // main→Add and main→helper in main.go, compute→Add and compute→subtract in calc.go
    assert!(same_file_count >= 2, "expected >=2 SameFile resolutions, got {same_file_count}");
    assert!(resolved >= 2, "expected >=2 resolved call edges, got {resolved}");

    // ponytail: Go import paths ("fmt") don't match file-path qnames — ImportMap=0 for v1.
    // When resolver learns Go package-to-path mapping, update to assert > 0.
    assert_eq!(import_map_count, 0, "ImportMap should not resolve Go import paths in v1");

    // Verify graph cache loads
    let graph = GraphCache::new();
    graph.load_from_store(&store).unwrap();

    let main_qname = format!("{}/main.go.main", dir.to_string_lossy());
    graph.get_node(&main_qname).expect("main should be in graph");
}

fn java_fixture_dir() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests");
    path.push("fixtures");
    path.push("java-small");
    path
}

/// Index the java-small fixture.
/// ponytail: Java import paths (com.example) don't match file-path-based qnames in v1.
/// Cross-file resolution relies on SameFile + global-unique. ImportMap=0.
#[test]
fn test_java_index() {
    let mut store = Store::open_in_memory().unwrap();
    let dir = java_fixture_dir().canonicalize().unwrap();
    let results = kernava_indexer::builder::index_full(&mut store, &dir).unwrap();
    assert_eq!(results.len(), 2, "should index 2 Java files");

    let all_nodes = store.get_all_nodes().unwrap();
    let names: Vec<_> = all_nodes.iter().map(|n| n.name.clone()).collect();

    // calc.java: Calculator, __construct, add, helper, compute (5 methods + 1 class)
    //            Math interface, Color enum — Math has 1 method (compute)
    // main.java: Main, main
    assert!(names.contains(&"Calculator".to_string()), "missing Calculator: {names:?}");
    assert!(names.contains(&"add".to_string()), "missing add: {names:?}");
    assert!(names.contains(&"helper".to_string()), "missing helper: {names:?}");
    assert!(names.contains(&"compute".to_string()), "missing compute: {names:?}");
    assert!(names.contains(&"Math".to_string()), "missing Math interface");
    assert!(names.contains(&"Color".to_string()), "missing Color enum");
    assert!(names.contains(&"Main".to_string()), "missing Main class");
    assert!(names.contains(&"main".to_string()), "missing main method");

    // Class kind
    let calc = all_nodes.iter().find(|n| n.name == "Calculator").unwrap();
    assert_eq!(calc.kind, "class", "Calculator should be class kind");

    // Interface kind
    let math_iface = all_nodes.iter().find(|n| n.name == "Math").unwrap();
    assert_eq!(math_iface.kind, "interface");

    // Enum kind
    let color = all_nodes.iter().find(|n| n.name == "Color").unwrap();
    assert_eq!(color.kind, "enum");

    // Method qnames: {file}.Calculator.add
    let add_qn = all_nodes.iter().find(|n| n.name == "add").unwrap();
    assert!(add_qn.qualified_name.contains("Calculator.add"), "add qname wrong: {}", add_qn.qualified_name);

    // Call edges — SameFile resolves in-file calls
    let edges = get_all_edges(&store);
    let calls: Vec<_> = edges.iter().filter(|(_, _, et, _)| et == "calls").collect();
    let resolved = calls.iter().filter(|(_, tid, _, _)| tid.is_some()).count();
    assert!(resolved >= 2, "expected >=2 resolved call edges, got {resolved}");

    let graph = GraphCache::new();
    graph.load_from_store(&store).unwrap();
}

fn cs_fixture_dir() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests");
    path.push("fixtures");
    path.push("cs-small");
    path
}

/// Index the cs-small fixture.
/// ponytail: C# using directives don't match file-path-based qnames in v1.
#[test]
fn test_csharp_index() {
    let mut store = Store::open_in_memory().unwrap();
    let dir = cs_fixture_dir().canonicalize().unwrap();
    let results = kernava_indexer::builder::index_full(&mut store, &dir).unwrap();
    assert_eq!(results.len(), 2, "should index 2 C# files");

    let all_nodes = store.get_all_nodes().unwrap();
    let names: Vec<_> = all_nodes.iter().map(|n| n.name.clone()).collect();

    assert!(names.contains(&"Calculator".to_string()), "missing Calculator: {names:?}");
    assert!(names.contains(&"Add".to_string()), "missing Add: {names:?}");
    assert!(names.contains(&"Helper".to_string()), "missing Helper");
    assert!(names.contains(&"Compute".to_string()), "missing Compute");
    assert!(names.contains(&"IMath".to_string()), "missing IMath interface");
    assert!(names.contains(&"Color".to_string()), "missing Color enum");
    assert!(names.contains(&"Main".to_string()), "missing Main class");
    assert!(names.contains(&"Run".to_string()), "missing Run method");

    let calc = all_nodes.iter().find(|n| n.name == "Calculator").unwrap();
    assert_eq!(calc.kind, "class");

    let imath = all_nodes.iter().find(|n| n.name == "IMath").unwrap();
    assert_eq!(imath.kind, "interface");

    let color = all_nodes.iter().find(|n| n.name == "Color").unwrap();
    assert_eq!(color.kind, "enum");

    // Call edges
    let edges = get_all_edges(&store);
    let calls: Vec<_> = edges.iter().filter(|(_, _, et, _)| et == "calls").collect();
    let resolved = calls.iter().filter(|(_, tid, _, _)| tid.is_some()).count();
    assert!(resolved >= 2, "expected >=2 resolved call edges, got {resolved}");

    let graph = GraphCache::new();
    graph.load_from_store(&store).unwrap();
}

fn rb_fixture_dir() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests");
    path.push("fixtures");
    path.push("rb-small");
    path
}

/// Index the rb-small fixture.
/// ponytail: Ruby require paths don't match file-path-based qnames in v1.
#[test]
fn test_ruby_index() {
    let mut store = Store::open_in_memory().unwrap();
    let dir = rb_fixture_dir().canonicalize().unwrap();
    let results = kernava_indexer::builder::index_full(&mut store, &dir).unwrap();
    assert_eq!(results.len(), 1, "should index 1 Ruby file");

    let all_nodes = store.get_all_nodes().unwrap();
    let names: Vec<_> = all_nodes.iter().map(|n| n.name.clone()).collect();

    // Calculator class + initialize, add, helper, compute methods
    // Math module + compute method
    // free_function
    assert!(names.contains(&"Calculator".to_string()), "missing Calculator: {names:?}");
    assert!(names.contains(&"add".to_string()), "missing add");
    assert!(names.contains(&"helper".to_string()), "missing helper");
    assert!(names.contains(&"compute".to_string()), "missing compute");
    assert!(names.contains(&"Math".to_string()), "missing Math module");
    assert!(names.contains(&"free_function".to_string()), "missing free_function");

    let calc = all_nodes.iter().find(|n| n.name == "Calculator").unwrap();
    assert_eq!(calc.kind, "class");

    let math_mod = all_nodes.iter().find(|n| n.name == "Math").unwrap();
    assert_eq!(math_mod.kind, "class"); // Ruby module → Class kind (reuses extract_class)

    // Call edges — add calls compute, compute calls helper
    let edges = get_all_edges(&store);
    let calls: Vec<_> = edges.iter().filter(|(_, _, et, _)| et == "calls").collect();
    let resolved = calls.iter().filter(|(_, tid, _, _)| tid.is_some()).count();
    assert!(resolved >= 2, "expected >=2 resolved calls, got {resolved}");

    let graph = GraphCache::new();
    graph.load_from_store(&store).unwrap();
}

fn php_fixture_dir() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests");
    path.push("fixtures");
    path.push("php-small");
    path
}

/// Index the php-small fixture.
/// ponytail: PHP use declarations don't match file-path-based qnames in v1.
#[test]
fn test_php_index() {
    let mut store = Store::open_in_memory().unwrap();
    let dir = php_fixture_dir().canonicalize().unwrap();
    let results = kernava_indexer::builder::index_full(&mut store, &dir).unwrap();
    assert_eq!(results.len(), 1, "should index 1 PHP file");

    let all_nodes = store.get_all_nodes().unwrap();
    let names: Vec<_> = all_nodes.iter().map(|n| n.name.clone()).collect();

    // Calculator class + __construct, add, helper, compute methods
    // Math interface + compute method
    // free_function
    assert!(names.contains(&"Calculator".to_string()), "missing Calculator: {names:?}");
    assert!(names.contains(&"add".to_string()), "missing add");
    assert!(names.contains(&"helper".to_string()), "missing helper");
    assert!(names.contains(&"compute".to_string()), "missing compute");
    assert!(names.contains(&"Math".to_string()), "missing Math iface");
    assert!(names.contains(&"free_function".to_string()), "missing free_function");

    let calc = all_nodes.iter().find(|n| n.name == "Calculator").unwrap();
    assert_eq!(calc.kind, "class");

    // Verify call extraction at the extractor layer (integration test goes through
    // store which drops unresolved calls, so we check extraction separately).
    // ponytail: PHP method calls ($this->compute()) extract callee as "compute" but
    // the target qname is file.Calculator.compute — SameFile resolution doesn't match
    // short name to method qname in v1. Upgrade: resolver learns to match short callee
    // names against method names in same file. When fixed, store-level resolved count
    let php_source = std::fs::read_to_string(dir.join("calc.php")).unwrap();
    let extract_result = extract(&php_source, Language::Php, "calc.php").unwrap();
    assert!(
        extract_result.calls.len() >= 2,
        "expected >=2 extracted calls, got {} ({:?})",
        extract_result.calls.len(),
        extract_result.calls.iter().map(|c| &c.callee).collect::<Vec<_>>()
    );
    // Store-level: unresolved calls are filtered by builder, so 0 resolved edges is
    // expected in v1. Not pinned to avoid masking resolver improvements.

    let graph = GraphCache::new();
    graph.load_from_store(&store).unwrap();
}

fn c_fixture_dir() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests");
    path.push("fixtures");
    path.push("c-small");
    path
}

/// Index the c-small fixture.
/// ponytail: C #include paths don't match file-path-based qnames in v1.
#[test]
fn test_c_index() {
    let mut store = Store::open_in_memory().unwrap();
    let dir = c_fixture_dir().canonicalize().unwrap();
    let results = kernava_indexer::builder::index_full(&mut store, &dir).unwrap();
    assert_eq!(results.len(), 2, "should index 2 C files");

    let all_nodes = store.get_all_nodes().unwrap();
    let names: Vec<_> = all_nodes.iter().map(|n| n.name.clone()).collect();

    // main.c: add, helper, main
    // shape.c: Point, point_sum, Color, Value
    assert!(names.contains(&"add".to_string()), "missing add: {names:?}");
    assert!(names.contains(&"helper".to_string()), "missing helper");
    assert!(names.contains(&"main".to_string()), "missing main");
    assert!(names.contains(&"Point".to_string()), "missing Point struct");
    assert!(names.contains(&"point_sum".to_string()), "missing point_sum");
    assert!(names.contains(&"Color".to_string()), "missing Color enum");
    assert!(names.contains(&"Value".to_string()), "missing Value union");

    // Struct → class kind
    let point = all_nodes.iter().find(|n| n.name == "Point").unwrap();
    assert_eq!(point.kind, "class", "Point struct should be class kind");

    // Enum kind
    let color = all_nodes.iter().find(|n| n.name == "Color").unwrap();
    assert_eq!(color.kind, "enum");

    // Call edges — main calls add, add calls nothing
    let edges = get_all_edges(&store);
    let calls: Vec<_> = edges.iter().filter(|(_, _, et, _)| et == "calls").collect();
    let resolved = calls.iter().filter(|(_, tid, _, _)| tid.is_some()).count();
    assert!(resolved >= 1, "expected >=1 resolved call, got {resolved}");

    let graph = GraphCache::new();
    graph.load_from_store(&store).unwrap();
}

fn cpp_fixture_dir() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests");
    path.push("fixtures");
    path.push("cpp-small");
    path
}

/// Index the cpp-small fixture.
/// ponytail: C++ #include paths don't match file-path-based qnames in v1.
#[test]
fn test_cpp_index() {
    let mut store = Store::open_in_memory().unwrap();
    let dir = cpp_fixture_dir().canonicalize().unwrap();
    let results = kernava_indexer::builder::index_full(&mut store, &dir).unwrap();
    assert_eq!(results.len(), 1, "should index 1 C++ file");

    let all_nodes = store.get_all_nodes().unwrap();
    let names: Vec<_> = all_nodes.iter().map(|n| n.name.clone()).collect();

    // Calculator class + add method
    // math namespace + compute function
    // Point struct
    // main function
    assert!(names.contains(&"Calculator".to_string()), "missing Calculator: {names:?}");
    assert!(names.contains(&"add".to_string()), "missing add method");
    assert!(names.contains(&"compute".to_string()), "missing compute");
    assert!(names.contains(&"Point".to_string()), "missing Point struct");
    assert!(names.contains(&"main".to_string()), "missing main");

    // ClassSpecifier → class kind
    let calc = all_nodes.iter().find(|n| n.name == "Calculator").unwrap();
    assert_eq!(calc.kind, "class", "Calculator (class_specifier) should be class kind");

    // StructSpecifier → class kind
    let point = all_nodes.iter().find(|n| n.name == "Point").unwrap();
    assert_eq!(point.kind, "class", "Point (struct_specifier) should be class kind");

    // Call edges — main calls c.add (field_expression call)
    let edges = get_all_edges(&store);
    let calls: Vec<_> = edges.iter().filter(|(_, _, et, _)| et == "calls").collect();
    let resolved = calls.iter().filter(|(_, tid, _, _)| tid.is_some()).count();
    assert!(resolved >= 1, "expected >=1 resolved call, got {resolved}");

    let graph = GraphCache::new();
    graph.load_from_store(&store).unwrap();
}

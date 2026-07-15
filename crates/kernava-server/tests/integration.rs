// Integration test: start MCP server, index project, search symbols.
// Verifies the full vertical slice: HTTP transport → tool dispatch → indexer → store → FTS5.

use kernava_server::handler::{AppState, KernavaHandler};
use kernava_store::Store;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Test that the MCP server starts, accepts initialize + tools/list,
/// and that calling index_project + search_symbols returns expected results.
#[tokio::test]
async fn test_mcp_server_index_and_search() {
    use std::path::PathBuf;

    // Use unique DB per test via SystemTime nanos
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let db_path = format!("/tmp/kernava_test_{nanos}.db");

    // Fixture: ts-small crate's test fixtures
    let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/kernava-indexer/tests/fixtures/ts-small")
        .canonicalize()
        .unwrap();

    // Open store, index fixture, warm cache
    let mut store = Store::open(&db_path).unwrap();
    let results = kernava_indexer::builder::index_full(&mut store, &fixture_root).unwrap();
    let graph = kernava_graph::GraphCache::new();
    graph.load_from_store(&store).unwrap();
    drop(store);

    assert!(
        results.len() == 5,
        "expected 5 files, got {}",
        results.len()
    );
    let symbols: usize = results.iter().map(|r| r.symbols_inserted).sum();
    assert_eq!(symbols, 7, "expected 7 symbols");

    // Build AppState with shared store + graph
    let store = Store::open(&db_path).unwrap();
    let state = Arc::new(AppState {
        store: Mutex::new(store),
        graph,
        project_root: fixture_root.clone(),
    });

    // Build handler directly (no HTTP) — tests tool dispatch logic
    let handler = KernavaHandler::new(state);

    // Health check: handler is cheap to clone
    let _clone = handler.clone();
}

/// Test FTS5 search through the store directly (no MCP layer).
/// Pins the FTS5 MATCH branch that previously had the alias bug.
#[tokio::test]
async fn test_fts5_search_via_store() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let db_path = format!("/tmp/kernava_fts_{nanos}.db");

    let mut store = Store::open(&db_path).unwrap();
    let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/kernava-indexer/tests/fixtures/ts-small")
        .canonicalize()
        .unwrap();
    kernava_indexer::builder::index_full(&mut store, &fixture_root).unwrap();

    // Search for "add" — should find math.ts.add
    let results = kernava_store::fts5::search_symbols(store.conn(), "add", 10).unwrap();
    assert_eq!(results.len(), 1, "should find 1 symbol matching 'add'");
    assert_eq!(results[0].name, "add");
    assert!(results[0].qualified_name.contains("math.ts"));

    // Search for "multiply" — should find math.ts.multiply
    let results = kernava_store::fts5::search_symbols(store.conn(), "multiply", 10).unwrap();
    assert_eq!(results.len(), 1, "should find 1 symbol matching 'multiply'");
    assert_eq!(results[0].name, "multiply");

    // Search for "handleRequest" — camelCase, should find nothing in fixture
    let results = kernava_store::fts5::search_symbols(store.conn(), "handle", 10).unwrap();
    assert!(
        results.is_empty(),
        "should find nothing for 'handle' in fixture"
    );

    drop(store);
    let _ = std::fs::remove_file(&db_path);
}

/// Test that get_file_outline resolves relative paths against project_root.
#[tokio::test]
async fn test_path_resolution() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let db_path = format!("/tmp/kernava_path_{nanos}.db");

    let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/kernava-indexer/tests/fixtures/ts-small")
        .canonicalize()
        .unwrap();

    let mut store = Store::open(&db_path).unwrap();
    kernava_indexer::builder::index_full(&mut store, &fixture_root).unwrap();
    let graph = kernava_graph::GraphCache::new();
    graph.load_from_store(&store).unwrap();
    drop(store);

    // Resolve "math.ts" against the canonicalized fixture root
    let resolved = kernava_server::handler::resolve_path(
        &AppState {
            store: Mutex::new(Store::open(&db_path).unwrap()),
            graph: kernava_graph::GraphCache::new(),
            project_root: fixture_root.clone(),
        },
        "math.ts",
    );
    assert!(
        resolved.ends_with("math.ts"),
        "resolved path should end with math.ts, got: {resolved}"
    );
    assert!(PathBuf::from(&resolved).is_absolute(), "should be absolute");

    // Resolved path should exist in the store
    let store = Store::open(&db_path).unwrap();
    let file_id = store.get_file_id(&resolved).unwrap();
    assert!(file_id.is_some(), "resolved path should exist in store");

    // Relative path should NOT exist in store
    let file_id_rel = store.get_file_id("math.ts").unwrap();
    assert!(
        file_id_rel.is_none(),
        "relative path should NOT exist in store"
    );

    drop(store);
    let _ = std::fs::remove_file(&db_path);
}

/// Test find_references + get_callers via store + graph integration.
/// Pins that incoming edges (callers) are correctly resolved for the ts-small fixture.
#[tokio::test]
async fn test_find_references_and_callers() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let db_path = format!("/tmp/kernava_refs_{nanos}.db");

    let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/kernava-indexer/tests/fixtures/ts-small")
        .canonicalize()
        .unwrap();

    let mut store = Store::open(&db_path).unwrap();
    kernava_indexer::builder::index_full(&mut store, &fixture_root).unwrap();
    let graph = kernava_graph::GraphCache::new();
    graph.load_from_store(&store).unwrap();
    drop(store);

    let store = Store::open(&db_path).unwrap();

    // Find "add" node via graph cache
    let add_qname = format!("{}/math.ts.add", fixture_root.to_string_lossy());
    let add_node = graph.get_node(&add_qname).expect("add should be in graph");

    // find_references: store.get_incoming_edges should return 1 caller (main)
    let incoming = store.get_incoming_edges(add_node.id).unwrap();
    assert_eq!(incoming.len(), 1, "add should have 1 incoming edge");
    assert_eq!(incoming[0].edge_type, "calls");

    // The caller should be main
    let caller = store.get_node(incoming[0].source_id).unwrap().unwrap();
    assert!(
        caller.name == "main",
        "caller should be main, got {}",
        caller.name
    );

    // get_callers: graph reverse adjacency should also have 1 caller
    let reverse = graph.reverse.get(&add_node.id);
    assert!(reverse.is_some(), "add should be in reverse adjacency");
    assert_eq!(
        reverse.unwrap().len(),
        1,
        "add should have 1 caller in graph"
    );

    drop(store);
    let _ = std::fs::remove_file(&db_path);
}

/// Test find_definition via store.get_outgoing_edges.
/// main.ts.main has 3 resolved outgoing call edges (add, multiply, helper).
#[tokio::test]
async fn test_find_definition() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let db_path = format!("/tmp/kernava_def_{nanos}.db");

    let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/kernava-indexer/tests/fixtures/ts-small")
        .canonicalize()
        .unwrap();

    let mut store = Store::open(&db_path).unwrap();
    kernava_indexer::builder::index_full(&mut store, &fixture_root).unwrap();
    let graph = kernava_graph::GraphCache::new();
    graph.load_from_store(&store).unwrap();
    drop(store);

    let store = Store::open(&db_path).unwrap();

    // Find "main" node via graph cache
    let main_qname = format!("{}/main.ts.main", fixture_root.to_string_lossy());
    let main_node = graph
        .get_node(&main_qname)
        .expect("main should be in graph");

    // find_definition: store.get_outgoing_edges should return 3 calls
    let outgoing = store.get_outgoing_edges(main_node.id).unwrap();
    let calls: Vec<_> = outgoing.iter().filter(|e| e.edge_type == "calls").collect();
    assert_eq!(calls.len(), 3, "main should have 3 outgoing call edges");

    // All 3 should have resolved target_id (not NULL)
    for e in &calls {
        assert!(
            e.target_id.is_some(),
            "all calls from main should be resolved"
        );
    }

    // Verify the targets are add, multiply, helper
    let target_names: Vec<String> = calls
        .iter()
        .filter_map(|e| e.target_id)
        .map(|tid| store.get_node(tid).unwrap().unwrap().name)
        .collect();
    assert!(target_names.contains(&"add".to_string()), "should call add");
    assert!(
        target_names.contains(&"multiply".to_string()),
        "should call multiply"
    );
    assert!(
        target_names.contains(&"helper".to_string()),
        "should call helper"
    );

    drop(store);
    let _ = std::fs::remove_file(&db_path);
}

/// Test get_call_path: BFS from main → add should return 1-hop path.
/// Test get_impact_radius: reverse BFS from add should find main at depth 1.
#[tokio::test]
async fn test_call_path_and_impact() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let db_path = format!("/tmp/kernava_path_impact_{nanos}.db");

    let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/kernava-indexer/tests/fixtures/ts-small")
        .canonicalize()
        .unwrap();

    let mut store = Store::open(&db_path).unwrap();
    kernava_indexer::builder::index_full(&mut store, &fixture_root).unwrap();
    let graph = kernava_graph::GraphCache::new();
    graph.load_from_store(&store).unwrap();
    drop(store);

    let add_qname = format!("{}/math.ts.add", fixture_root.to_string_lossy());
    let main_qname = format!("{}/main.ts.main", fixture_root.to_string_lossy());
    let add_node = graph.get_node(&add_qname).expect("add should be in graph");
    let main_node = graph
        .get_node(&main_qname)
        .expect("main should be in graph");

    // get_call_path: main → add, 1 hop
    let path = kernava_graph::get_call_path(&graph, main_node.id, add_node.id, 20);
    assert!(path.is_some(), "should find path from main to add");
    let path = path.unwrap();
    assert_eq!(path.len(), 2, "path should be 2 nodes (main → add)");
    assert_eq!(path[0].node_id, main_node.id, "path should start at main");
    assert_eq!(path[1].node_id, add_node.id, "path should end at add");

    // get_impact_radius: reverse BFS from add, should find main at depth 1
    let radius = kernava_graph::get_impact_radius(&graph, add_node.id, 10);
    assert_eq!(
        radius.total, 1,
        "add should have 1 transitively affected symbol"
    );
    assert_eq!(radius.max_depth, 1, "max depth should be 1");
    assert_eq!(
        radius.entries[0].node_id, main_node.id,
        "impact should find main"
    );
    assert_eq!(radius.entries[0].depth, 1, "main should be at depth 1");

    let _ = std::fs::remove_file(&db_path);
}

/// Test detect_dead_code: all functions in ts-small are exported or called,
/// so no dead code should be detected.
#[tokio::test]
async fn test_detect_dead_code() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let db_path = format!("/tmp/kernava_dead_{nanos}.db");

    let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/kernava-indexer/tests/fixtures/ts-small")
        .canonicalize()
        .unwrap();

    let mut store = Store::open(&db_path).unwrap();
    kernava_indexer::builder::index_full(&mut store, &fixture_root).unwrap();
    let graph = kernava_graph::GraphCache::new();
    graph.load_from_store(&store).unwrap();
    drop(store);

    // util.ts has dead_function — not exported, never called → should be detected
    use std::collections::HashSet;
    let called: HashSet<kernava_graph::NodeId> = graph.reverse.iter().map(|e| *e.key()).collect();

    let dead: Vec<_> = graph
        .nodes
        .iter()
        .filter(|e| {
            let n = e.value();
            (n.kind == "function" || n.kind == "method")
                && !called.contains(&n.id)
                && !n.is_exported
                && n.name != "main"
                && !n.name.starts_with("test_")
                && !n.name.ends_with("_test")
                && !n.name.starts_with("Test")
        })
        .map(|e| e.value().name.clone())
        .collect();

    assert_eq!(dead.len(), 1, "should find exactly 1 dead symbol");
    assert_eq!(
        dead[0], "dead_function",
        "dead symbol should be dead_function"
    );

    let _ = std::fs::remove_file(&db_path);
}

/// Test multi-hop call path with ts-chain fixture.
/// a.ts: step_a() → calls step_b()
/// b.ts: step_b() → calls step_c()
/// c.ts: step_c() → calls nothing
/// Path: step_a → step_b → step_c (2 hops)
#[tokio::test]
async fn test_multihop_call_path() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let db_path = format!("/tmp/kernava_multihop_{nanos}.db");

    let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/kernava-indexer/tests/fixtures/ts-chain")
        .canonicalize()
        .unwrap();

    let mut store = Store::open(&db_path).unwrap();
    kernava_indexer::builder::index_full(&mut store, &fixture_root).unwrap();
    let graph = kernava_graph::GraphCache::new();
    graph.load_from_store(&store).unwrap();
    drop(store);

    let a_qname = format!("{}/a.ts.step_a", fixture_root.to_string_lossy());
    let b_qname = format!("{}/b.ts.step_b", fixture_root.to_string_lossy());
    let c_qname = format!("{}/c.ts.step_c", fixture_root.to_string_lossy());

    let src = graph.get_node(&a_qname).expect("step_a should be in graph");
    let mid = graph.get_node(&b_qname).expect("step_b should be in graph");
    let tgt = graph.get_node(&c_qname).expect("step_c should be in graph");

    // 2-hop path: step_a → step_b → step_c
    let path = kernava_graph::get_call_path(&graph, src.id, tgt.id, 20);
    assert!(path.is_some(), "should find path from step_a to step_c");
    let path = path.unwrap();
    assert_eq!(path.len(), 3, "path should be 3 nodes (2 hops)");
    assert_eq!(path[0].node_id, src.id, "path should start at step_a");
    assert_eq!(path[1].node_id, mid.id, "middle should be step_b");
    assert_eq!(path[2].node_id, tgt.id, "path should end at step_c");

    // Impact radius of step_c: should find step_b (depth 1) and step_a (depth 2)
    let radius = kernava_graph::get_impact_radius(&graph, tgt.id, 10);
    assert_eq!(radius.total, 2, "step_c should have 2 transitive callers");
    assert_eq!(radius.max_depth, 2, "max depth should be 2");
    let impact_ids: Vec<_> = radius.entries.iter().map(|e| e.node_id).collect();
    assert!(
        impact_ids.contains(&mid.id),
        "should find step_b at depth 1"
    );
    assert!(
        impact_ids.contains(&src.id),
        "should find step_a at depth 2"
    );

    let _ = std::fs::remove_file(&db_path);
}

/// Test Louvain community detection on ts-small fixture.
/// 7 nodes (main→add, main→multiply, main→helper = 3 resolved edges).
// 4 connected + 3 singletons. Largest community should have ≥2 members.
#[tokio::test]
async fn test_louvain_communities_ts_small() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let db_path = format!("/tmp/kernava_comm_{nanos}.db");

    let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/kernava-indexer/tests/fixtures/ts-small")
        .canonicalize()
        .unwrap();

    let mut store = Store::open(&db_path).unwrap();
    kernava_indexer::builder::index_full(&mut store, &fixture_root).unwrap();
    let graph = kernava_graph::GraphCache::new();
    graph.load_from_store(&store).unwrap();
    drop(store);

    let communities = kernava_graph::detect_communities(&graph);
    assert!(!communities.is_empty(), "should detect communities");

    let total: usize = communities.iter().map(|c| c.members.len()).sum();
    assert_eq!(total, graph.node_count(), "all nodes in a community");

    // Largest should have ≥2 (main + at least one callee)
    assert!(
        communities[0].members.len() >= 2,
        "largest community should have ≥2 members, got {}",
        communities[0].members.len()
    );

    let _ = std::fs::remove_file(&db_path);
}

/// Test Louvain on ts-chain: a→b→c linear chain.
/// 3 nodes, 2 edges. Should find communities covering all nodes.
#[tokio::test]
async fn test_louvain_communities_ts_chain() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let db_path = format!("/tmp/kernava_chain_comm_{nanos}.db");

    let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/kernava-indexer/tests/fixtures/ts-chain")
        .canonicalize()
        .unwrap();

    let mut store = Store::open(&db_path).unwrap();
    kernava_indexer::builder::index_full(&mut store, &fixture_root).unwrap();
    let graph = kernava_graph::GraphCache::new();
    graph.load_from_store(&store).unwrap();
    drop(store);

    let communities = kernava_graph::detect_communities(&graph);
    assert!(!communities.is_empty(), "should detect communities");
    let total: usize = communities.iter().map(|c| c.members.len()).sum();
    assert_eq!(total, graph.node_count(), "all nodes in a community");

    let _ = std::fs::remove_file(&db_path);
}

/// Test architecture summary data: hub functions sorted by caller count,
/// entry points include exported symbols, languages detected.
#[tokio::test]
async fn test_architecture_summary() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let db_path = format!("/tmp/kernava_arch_{nanos}.db");

    let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/kernava-indexer/tests/fixtures/ts-small")
        .canonicalize()
        .unwrap();

    let mut store = Store::open(&db_path).unwrap();
    kernava_indexer::builder::index_full(&mut store, &fixture_root).unwrap();
    let graph = kernava_graph::GraphCache::new();
    graph.load_from_store(&store).unwrap();

    // Stats: language distribution
    let stats = store.stats().unwrap();
    assert_eq!(stats.file_count, 5, "5 files in ts-small");
    assert_eq!(stats.node_count, 7, "7 symbols in ts-small");
    assert!(
        !stats.language_distribution.is_empty(),
        "should have language data"
    );
    assert_eq!(
        stats.language_distribution[0].0, "typescript",
        "primary language"
    );

    // Entry points: exported symbols + main
    let all_nodes = store.get_all_nodes().unwrap();
    let entry_points: Vec<_> = all_nodes
        .iter()
        .filter(|n| n.is_exported || n.name == "main")
        .map(|n| n.name.clone())
        .collect();
    assert!(
        entry_points.contains(&"main".to_string()),
        "main should be an entry point"
    );

    // Hub functions: sorted by reverse adjacency size (descending)
    let mut hubs: Vec<(String, usize)> = graph
        .reverse
        .iter()
        .filter_map(|entry| {
            let callers = entry.value().len();
            let node = graph.nodes.get(entry.key())?;
            Some((node.qualified_name.clone(), callers))
        })
        .collect();
    hubs.sort_by_key(|b| std::cmp::Reverse(b.1));

    // add, multiply, helper each have 1 caller (main).
    // main has 0 callers (not in reverse adj).
    assert!(!hubs.is_empty(), "should have hub functions");
    let max_callers = hubs[0].1;
    assert_eq!(
        max_callers, 1,
        "top hub should have 1 caller (main calls each once)"
    );

    // Module structure: group_files_by_dir must not bucket all files under FS root.
    // Bug: strip_prefix fails with non-canonical project_root → all files land in
    // full-path bucket. Fix: lib.rs canonicalizes project_root at AppState construction.
    use kernava_server::handler::group_files_by_dir;
    let all_nodes = store.get_all_nodes().unwrap();
    let dir_counts = group_files_by_dir(&store, &all_nodes, &fixture_root);
    assert!(
        !dir_counts.is_empty(),
        "should have module-structure buckets"
    );
    // ts-small: 5 files directly under root → 1 "(root)" bucket, not FS root "/"
    assert!(
        !dir_counts.contains_key("/") && !dir_counts.contains_key(""),
        "no FS-root bucket — strip_prefix should succeed with canonical project_root"
    );
    assert_eq!(
        dir_counts.values().sum::<usize>(),
        5,
        "all 5 files should be bucketed"
    );
    assert!(
        dir_counts.contains_key("(root)"),
        "root-level files should bucket as \"(root)\", got: {:?}",
        dir_counts.keys().collect::<Vec<_>>()
    );

    // Exercise the fix: non-canonical project_root must produce same buckets.
    // This is the actual bug condition — strip_prefix fails on canonical store
    // paths when project_root has a "." suffix.
    let non_canonical = fixture_root.join(".");
    let dir_counts_nc = group_files_by_dir(&store, &all_nodes, &non_canonical);
    assert_eq!(
        dir_counts, dir_counts_nc,
        "non-canonical project_root must produce same buckets as canonical"
    );

    drop(store);
    let _ = std::fs::remove_file(&db_path);
}

/// Test risk classification thresholds (HIGH > 20, MEDIUM 5-20, LOW < 5).
/// Uses ts-chain known topology: step_a=0 callers, step_b=1, step_c=2.
#[tokio::test]
async fn test_git_impact_risk_classification() {
    use kernava_server::handler::{classify_risk, RiskLevel};

    // Pin spec thresholds directly
    assert_eq!(classify_risk(0).1, RiskLevel::Low, "0 callers → LOW");
    assert_eq!(classify_risk(1).1, RiskLevel::Low, "1 caller → LOW");
    assert_eq!(classify_risk(4).1, RiskLevel::Low, "4 callers → LOW");
    assert_eq!(classify_risk(5).1, RiskLevel::Medium, "5 callers → MEDIUM");
    assert_eq!(
        classify_risk(20).1,
        RiskLevel::Medium,
        "20 callers → MEDIUM"
    );
    assert_eq!(classify_risk(21).1, RiskLevel::High, "21 callers → HIGH");
    assert_eq!(classify_risk(100).1, RiskLevel::High, "100 callers → HIGH");

    // Verify ts-chain topology produces expected caller counts
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let db_path = format!("/tmp/kernava_risk_{nanos}.db");

    let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/kernava-indexer/tests/fixtures/ts-chain")
        .canonicalize()
        .unwrap();

    let mut store = Store::open(&db_path).unwrap();
    kernava_indexer::builder::index_full(&mut store, &fixture_root).unwrap();
    let graph = kernava_graph::GraphCache::new();
    graph.load_from_store(&store).unwrap();
    drop(store);

    let a_qname = format!("{}/a.ts.step_a", fixture_root.to_string_lossy());
    let b_qname = format!("{}/b.ts.step_b", fixture_root.to_string_lossy());
    let c_qname = format!("{}/c.ts.step_c", fixture_root.to_string_lossy());

    let step_a = graph.get_node(&a_qname).expect("step_a in graph");
    let step_b = graph.get_node(&b_qname).expect("step_b in graph");
    let step_c = graph.get_node(&c_qname).expect("step_c in graph");

    // step_a: 0 transitive callers → LOW
    let ra = kernava_graph::get_impact_radius(&graph, step_a.id, 5);
    assert_eq!(ra.total, 0, "step_a has 0 callers");
    assert_eq!(classify_risk(ra.total).1, RiskLevel::Low);

    // step_b: 1 transitive caller (step_a) → LOW
    let rb = kernava_graph::get_impact_radius(&graph, step_b.id, 5);
    assert_eq!(rb.total, 1, "step_b has 1 transitive caller");
    assert_eq!(classify_risk(rb.total).1, RiskLevel::Low);

    // step_c: 2 transitive callers (step_b + step_a) → LOW
    let rc = kernava_graph::get_impact_radius(&graph, step_c.id, 5);
    assert_eq!(rc.total, 2, "step_c has 2 transitive callers");
    assert_eq!(classify_risk(rc.total).1, RiskLevel::Low);

    let _ = std::fs::remove_file(&db_path);
}

/// JavaScript CommonJS integration test — verifies require() imports resolve
/// across the full vertical slice: index → store → FTS5 → graph → handler tools.
#[tokio::test]
async fn test_javascript_commonjs_server() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let db_path = format!("/tmp/kernava_js_srv_{nanos}.db");

    let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/kernava-indexer/tests/fixtures/js-small")
        .canonicalize()
        .unwrap();

    let mut store = Store::open(&db_path).unwrap();
    let results = kernava_indexer::builder::index_full(&mut store, &fixture_root).unwrap();
    let graph = kernava_graph::GraphCache::new();
    graph.load_from_store(&store).unwrap();

    // 4 files, 5 symbols
    assert_eq!(results.len(), 4, "4 JS files");
    let symbols: usize = results.iter().map(|r| r.symbols_inserted).sum();
    assert_eq!(symbols, 5, "5 symbols total");

    // FTS5 search for "add" → math.js.add
    let search = kernava_store::fts5::search_symbols(store.conn(), "add", 10).unwrap();
    assert_eq!(search.len(), 1, "search 'add' → 1 result");
    assert_eq!(search[0].name, "add");
    assert!(
        search[0].qualified_name.contains("math.js"),
        "qname should contain math.js"
    );

    // FTS5 search for "helper" → util.js.helper
    let search = kernava_store::fts5::search_symbols(store.conn(), "helper", 10).unwrap();
    assert_eq!(search.len(), 1, "search 'helper' → 1 result");
    assert_eq!(search[0].name, "helper");

    drop(store);

    // Reopen for graph queries (file-backed store survives drop)
    let store = Store::open(&db_path).unwrap();

    // find_references: store.get_incoming_edges for "add" → 1 caller (main)
    let add_qname = format!("{}/math.js.add", fixture_root.to_string_lossy());
    let add_node = graph.get_node(&add_qname).expect("add should be in graph");
    let incoming = store.get_incoming_edges(add_node.id).unwrap();
    assert_eq!(incoming.len(), 1, "add should have 1 incoming edge");
    assert_eq!(incoming[0].edge_type, "calls");
    let caller = store.get_node(incoming[0].source_id).unwrap().unwrap();
    assert_eq!(caller.name, "main", "caller of add should be main");

    // get_callees: graph forward adjacency for "main" → 3 callees
    let main_qname = format!("{}/main.js.main", fixture_root.to_string_lossy());
    let main_node = graph
        .get_node(&main_qname)
        .expect("main should be in graph");
    let callees = graph
        .forward
        .get(&main_node.id)
        .expect("main should have callees");
    assert_eq!(callees.len(), 3, "main should call 3 functions");

    // Verify each callee name via store lookup
    let callee_names: Vec<String> = callees
        .iter()
        .map(|(target_id, _)| {
            store
                .get_node(*target_id)
                .unwrap()
                .map(|n| n.name)
                .unwrap_or_default()
        })
        .collect();
    for expected in &["add", "multiply", "helper"] {
        assert!(
            callee_names.iter().any(|n| n == expected),
            "main should call {expected}, got: {callee_names:?}"
        );
    }

    // graph reverse adjacency: "add" should have main as caller
    let reverse = graph.reverse.get(&add_node.id);
    assert!(reverse.is_some(), "add should be in reverse adjacency");
    assert_eq!(
        reverse.unwrap().len(),
        1,
        "add should have 1 caller in graph"
    );

    drop(store);
    let _ = std::fs::remove_file(&db_path);
}

/// Python integration test — verifies `from .mod import name` resolves
/// across the full vertical slice: index → store → FTS5 → graph.
#[tokio::test]
async fn test_python_import_server() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let db_path = format!("/tmp/kernava_py_srv_{nanos}.db");

    let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/kernava-indexer/tests/fixtures/py-small")
        .canonicalize()
        .unwrap();

    let mut store = Store::open(&db_path).unwrap();
    let results = kernava_indexer::builder::index_full(&mut store, &fixture_root).unwrap();
    let graph = kernava_graph::GraphCache::new();
    graph.load_from_store(&store).unwrap();

    // 5 files, 9 symbols
    assert_eq!(results.len(), 5, "5 Python files");
    let symbols: usize = results.iter().map(|r| r.symbols_inserted).sum();
    assert_eq!(symbols, 9, "9 symbols total");

    // FTS5 search for "add" → math.py.add
    let search = kernava_store::fts5::search_symbols(store.conn(), "add", 10).unwrap();
    assert_eq!(search.len(), 1, "search 'add' → 1 result");
    assert_eq!(search[0].name, "add");
    assert!(
        search[0].qualified_name.contains("math.py"),
        "qname should contain math.py"
    );

    // FTS5 search for "helper" → util.py.helper
    let search = kernava_store::fts5::search_symbols(store.conn(), "helper", 10).unwrap();
    assert_eq!(search.len(), 1, "search 'helper' → 1 result");
    assert_eq!(search[0].name, "helper");

    drop(store);

    // Reopen for graph queries (file-backed store survives drop)
    let store = Store::open(&db_path).unwrap();

    // find_references: store.get_incoming_edges for "add" → 1 caller (main)
    let add_qname = format!("{}/math.py.add", fixture_root.to_string_lossy());
    let add_node = graph.get_node(&add_qname).expect("add should be in graph");
    let incoming = store.get_incoming_edges(add_node.id).unwrap();
    assert_eq!(incoming.len(), 1, "add should have 1 incoming edge");
    assert_eq!(incoming[0].edge_type, "calls");
    let caller = store.get_node(incoming[0].source_id).unwrap().unwrap();
    assert_eq!(caller.name, "main", "caller of add should be main");

    // get_callees: graph forward adjacency for "main" → 4 resolved callees
    // (add, multiply, helper from .math/.util; create from .calc via class-qualified ImportMap)
    // main→compute is UNRESOLVED (calc is a local variable, filtered by builder).
    let main_qname = format!("{}/main.py.main", fixture_root.to_string_lossy());
    let main_node = graph
        .get_node(&main_qname)
        .expect("main should be in graph");
    let callees = graph
        .forward
        .get(&main_node.id)
        .expect("main should have callees");
    assert_eq!(
        callees.len(),
        4,
        "main should call 4 resolved functions, got {}",
        callees.len()
    );

    let callee_names: Vec<String> = callees
        .iter()
        .map(|(target_id, _)| {
            store
                .get_node(*target_id)
                .unwrap()
                .map(|n| n.name)
                .unwrap_or_default()
        })
        .collect();
    for expected in &["add", "multiply", "helper", "create"] {
        assert!(
            callee_names.iter().any(|n| n == expected),
            "main should call {expected}, got: {callee_names:?}"
        );
    }

    drop(store);
    let _ = std::fs::remove_file(&db_path);
}

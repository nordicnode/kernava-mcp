// kernava-graph: graph traversal algorithms
// P2 task 2.5: BFS shortest path, forward/reverse adjacency iteration.

use crate::cache::GraphCache;
use crate::model::NodeId;
use std::collections::{HashSet, VecDeque};

/// A single hop in a call path.
#[derive(Debug, Clone)]
pub struct PathHop {
    pub node_id: NodeId,
    pub confidence: f64,
}

/// Find the shortest call path from `source` to `target` via BFS over forward call edges.
/// Returns `None` if unreachable within `max_depth` hops.
/// The path includes both endpoints. Each hop's confidence is the edge confidence from the previous node.
pub fn get_call_path(
    cache: &GraphCache,
    source: NodeId,
    target: NodeId,
    max_depth: usize,
) -> Option<Vec<PathHop>> {
    if source == target {
        return Some(vec![PathHop {
            node_id: source,
            confidence: 1.0,
        }]);
    }

    // BFS: queue holds (current_node, path_so_far)
    let mut visited: HashSet<NodeId> = HashSet::new();
    visited.insert(source);
    let mut queue: VecDeque<(NodeId, Vec<PathHop>)> = VecDeque::new();
    queue.push_back((
        source,
        vec![PathHop {
            node_id: source,
            confidence: 1.0,
        }],
    ));

    while let Some((node, path)) = queue.pop_front() {
        if path.len() - 1 >= max_depth {
            continue;
        }
        for (callee, conf) in cache.get_callees(node) {
            if visited.contains(&callee) {
                continue;
            }
            let mut new_path = path.clone();
            new_path.push(PathHop {
                node_id: callee,
                confidence: conf,
            });
            if callee == target {
                return Some(new_path);
            }
            visited.insert(callee);
            queue.push_back((callee, new_path));
        }
    }

    None
}

/// Iterate forward adjacency: all nodes reachable from `source` via forward call edges,
/// up to `max_depth` hops. Returns flattened list of (node_id, depth, confidence).
pub fn forward_reachable(
    cache: &GraphCache,
    source: NodeId,
    max_depth: usize,
) -> Vec<(NodeId, usize, f64)> {
    let mut visited: HashSet<NodeId> = HashSet::new();
    let mut result: Vec<(NodeId, usize, f64)> = Vec::new();
    let mut queue: VecDeque<(NodeId, usize, f64)> = VecDeque::new();
    queue.push_back((source, 0, 1.0));
    visited.insert(source);

    while let Some((node, depth, conf)) = queue.pop_front() {
        if depth >= max_depth {
            continue;
        }
        for (callee, edge_conf) in cache.get_callees(node) {
            if visited.contains(&callee) {
                continue;
            }
            visited.insert(callee);
            let combined_conf = conf * edge_conf;
            result.push((callee, depth + 1, combined_conf));
            queue.push_back((callee, depth + 1, combined_conf));
        }
    }

    result
}

/// Iterate reverse adjacency: all nodes that can reach `target` via reverse call edges (callers),
/// up to `max_depth` hops. Returns flattened list of (node_id, depth, confidence).
pub fn reverse_reachable(
    cache: &GraphCache,
    target: NodeId,
    max_depth: usize,
) -> Vec<(NodeId, usize, f64)> {
    let mut visited: HashSet<NodeId> = HashSet::new();
    let mut result: Vec<(NodeId, usize, f64)> = Vec::new();
    let mut queue: VecDeque<(NodeId, usize, f64)> = VecDeque::new();
    queue.push_back((target, 0, 1.0));
    visited.insert(target);

    while let Some((node, depth, conf)) = queue.pop_front() {
        if depth >= max_depth {
            continue;
        }
        for (caller, edge_conf) in cache.get_callers(node) {
            if visited.contains(&caller) {
                continue;
            }
            visited.insert(caller);
            let combined_conf = conf * edge_conf;
            result.push((caller, depth + 1, combined_conf));
            queue.push_back((caller, depth + 1, combined_conf));
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernava_store::{EdgeRecord, FileRecord, NodeRecord, Store};

    fn make_cache_with_chain() -> (Store, GraphCache, i64) {
        let store = Store::open_in_memory().unwrap();
        let file_id = store
            .upsert_file(&FileRecord {
                path: "src/chain.ts".into(),
                language: "typescript".into(),
                content_hash: vec![0; 16],
                mtime: 0,
                size: 100,
            })
            .unwrap();

        let mk = |name: &str, qn: &str, line_start: i32, exported: bool| -> i64 {
            store
                .insert_node(&NodeRecord {
                    kind: "function".into(),
                    name: name.into(),
                    qualified_name: qn.into(),
                    file_id,
                    line_start,
                    line_end: line_start + 5,
                    col_start: None,
                    signature: None,
                    return_type: None,
                    receiver_type: None,
                    is_exported: exported,
                    complexity: 1,
                    decorators: None,
                    metadata: None,
                })
                .unwrap()
        };

        let a = mk("a", "src/chain.a", 1, true);
        let b = mk("b", "src/chain.b", 6, false);
        let c = mk("c", "src/chain.c", 12, false);
        let d = mk("d", "src/chain.d", 18, true);

        // a → b → c → d  (call chain)
        store
            .insert_edge(&EdgeRecord {
                source_id: a,
                target_id: Some(b),
                edge_type: "calls".into(),
                confidence: 0.9,
                file_id: Some(file_id),
                line: Some(2),
                metadata: None,
            })
            .unwrap();
        store
            .insert_edge(&EdgeRecord {
                source_id: b,
                target_id: Some(c),
                edge_type: "calls".into(),
                confidence: 0.8,
                file_id: Some(file_id),
                line: Some(7),
                metadata: None,
            })
            .unwrap();
        store
            .insert_edge(&EdgeRecord {
                source_id: c,
                target_id: Some(d),
                edge_type: "calls".into(),
                confidence: 0.7,
                file_id: Some(file_id),
                line: Some(13),
                metadata: None,
            })
            .unwrap();

        let cache = GraphCache::new();
        cache.load_from_store(&store).unwrap();
        (store, cache, a)
    }

    #[test]
    fn test_bfs_shortest_path() {
        let (_store, cache, a_id) = make_cache_with_chain();
        let d_id = *cache.by_qualified.get("src/chain.d").unwrap();

        let path = get_call_path(&cache, a_id, d_id, 20).unwrap();
        assert_eq!(path.len(), 4); // a → b → c → d
        assert_eq!(path[0].node_id, a_id);
        assert_eq!(path[3].node_id, d_id);
        assert_eq!(path[1].confidence, 0.9);
    }

    #[test]
    fn test_bfs_no_path() {
        let (_store, cache, a_id) = make_cache_with_chain();
        let b_id = *cache.by_qualified.get("src/chain.b").unwrap();

        // b cannot reach a (reverse direction)
        assert!(get_call_path(&cache, b_id, a_id, 20).is_none());
    }

    #[test]
    fn test_bfs_same_node() {
        let (_store, cache, a_id) = make_cache_with_chain();
        let path = get_call_path(&cache, a_id, a_id, 20).unwrap();
        assert_eq!(path.len(), 1);
        assert_eq!(path[0].node_id, a_id);
    }

    #[test]
    fn test_bfs_max_depth_cutoff() {
        let (_store, cache, a_id) = make_cache_with_chain();
        let d_id = *cache.by_qualified.get("src/chain.d").unwrap();

        // Path is 3 hops (a→b→c→d), depth 2 cuts it off
        assert!(get_call_path(&cache, a_id, d_id, 2).is_none());
        // depth 3 reaches it
        assert!(get_call_path(&cache, a_id, d_id, 3).is_some());
    }

    #[test]
    fn test_forward_reachable() {
        let (_store, cache, a_id) = make_cache_with_chain();
        let reachable = forward_reachable(&cache, a_id, 10);
        assert_eq!(reachable.len(), 3); // b, c, d
                                        // First reachable is b at depth 1
        assert_eq!(reachable[0].1, 1);
    }

    #[test]
    fn test_reverse_reachable() {
        let (_store, cache, _a_id) = make_cache_with_chain();
        let d_id = *cache.by_qualified.get("src/chain.d").unwrap();
        let reachable = reverse_reachable(&cache, d_id, 10);
        assert_eq!(reachable.len(), 3); // c, b, a
                                        // First reachable is c at depth 1
        assert_eq!(reachable[0].1, 1);
    }
}

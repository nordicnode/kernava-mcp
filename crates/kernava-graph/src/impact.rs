// kernava-graph: impact analysis
// P2 task 2.6: reverse BFS collecting transitively affected symbols with depth + confidence.

use crate::cache::GraphCache;
use crate::model::NodeId;
use std::collections::{HashMap, HashSet, VecDeque};

/// A node in the impact radius, grouped by BFS depth from the source.
#[derive(Debug, Clone)]
pub struct ImpactEntry {
    pub node_id: NodeId,
    pub depth: usize,
    /// Product of edge confidences along the path from source to this node.
    pub confidence: f64,
    /// Risk weight: confidence × 1/depth. Higher = more direct + reliable impact.
    pub risk_score: f64,
}

/// Result of impact analysis, grouped by depth.
#[derive(Debug, Clone)]
pub struct ImpactRadius {
    /// All affected symbols, sorted by depth then descending risk_score.
    pub entries: Vec<ImpactEntry>,
    /// Total count of affected symbols (excluding the source).
    pub total: usize,
    /// Max depth reached.
    pub max_depth: usize,
}

impl ImpactRadius {
    /// Group entries by depth, returning Vec of (depth, entries).
    pub fn grouped_by_depth(&self) -> Vec<(usize, Vec<&ImpactEntry>)> {
        let mut groups: HashMap<usize, Vec<&ImpactEntry>> = HashMap::new();
        for e in &self.entries {
            groups.entry(e.depth).or_default().push(e);
        }
        let mut sorted: Vec<(usize, Vec<&ImpactEntry>)> = groups.into_iter().collect();
        sorted.sort_by_key(|(d, _)| *d);
        sorted
    }
}

/// Compute the impact radius of `source` — all transitive callers via reverse BFS.
/// `max_depth` caps the traversal (default 10 per plan).
/// Returns entries sorted by depth (ascending), then descending risk_score.
pub fn get_impact_radius(cache: &GraphCache, source: NodeId, max_depth: usize) -> ImpactRadius {
    let mut visited: HashSet<NodeId> = HashSet::new();
    let mut entries: Vec<ImpactEntry> = Vec::new();
    let mut queue: VecDeque<(NodeId, usize, f64)> = VecDeque::new();
    queue.push_back((source, 0, 1.0));
    visited.insert(source);

    let mut max_depth_reached = 0;

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
            let d = depth + 1;
            let risk = combined_conf / d as f64;
            entries.push(ImpactEntry {
                node_id: caller,
                depth: d,
                confidence: combined_conf,
                risk_score: risk,
            });
            if d > max_depth_reached {
                max_depth_reached = d;
            }
            queue.push_back((caller, d, combined_conf));
        }
    }

    // Sort by depth ascending, then risk_score descending
    entries.sort_by(|a, b| {
        a.depth.cmp(&b.depth).then(
            b.risk_score
                .partial_cmp(&a.risk_score)
                .unwrap_or(std::cmp::Ordering::Equal),
        )
    });

    let total = entries.len();
    ImpactRadius {
        entries,
        total,
        max_depth: max_depth_reached,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernava_store::{EdgeRecord, FileRecord, NodeRecord, Store};

    fn make_cache_with_callers() -> (Store, GraphCache, i64) {
        let store = Store::open_in_memory().unwrap();
        let file_id = store
            .upsert_file(&FileRecord {
                path: "src/impact.ts".into(),
                language: "typescript".into(),
                content_hash: vec![0; 16],
                mtime: 0,
                size: 100,
            })
            .unwrap();

        let mk = |name: &str, qn: &str, line_start: i32| -> i64 {
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
                    is_exported: false,
                    complexity: 1,
                    decorators: None,
                    metadata: None,
                })
                .unwrap()
        };

        // target ← b ← a   (a calls b calls target)
        // target ← c       (c calls target directly)
        let target = mk("target", "src/impact.target", 1);
        let b = mk("b", "src/impact.b", 6);
        let a = mk("a", "src/impact.a", 12);
        let c = mk("c", "src/impact.c", 18);

        let mk_edge = |src: i64, tgt: i64, conf: f64| {
            store
                .insert_edge(&EdgeRecord {
                    source_id: src,
                    target_id: Some(tgt),
                    edge_type: "calls".into(),
                    confidence: conf,
                    file_id: Some(file_id),
                    line: Some(1),
                    metadata: None,
                })
                .unwrap();
        };

        mk_edge(b, target, 0.9); // b → target
        mk_edge(a, b, 0.8); // a → b
        mk_edge(c, target, 0.7); // c → target

        let cache = GraphCache::new();
        cache.load_from_store(&store).unwrap();
        (store, cache, target)
    }

    #[test]
    fn test_impact_radius_direct_and_transitive() {
        let (_store, cache, target_id) = make_cache_with_callers();
        let radius = get_impact_radius(&cache, target_id, 10);

        // depth 1: b (conf 0.9), c (conf 0.7)
        // depth 2: a (conf 0.9*0.8=0.72)
        assert_eq!(radius.total, 3);
        assert_eq!(radius.max_depth, 2);

        // Sorted by depth then risk desc
        assert_eq!(radius.entries[0].depth, 1); // b or c
        assert_eq!(radius.entries[1].depth, 1);
        assert_eq!(radius.entries[2].depth, 2); // a

        // b has higher risk than c (0.9/1 > 0.7/1)
        assert!(radius.entries[0].risk_score > radius.entries[1].risk_score);
    }

    #[test]
    fn test_impact_radius_max_depth() {
        let (_store, cache, target_id) = make_cache_with_callers();
        let radius = get_impact_radius(&cache, target_id, 1);

        // Only depth 1 callers: b, c
        assert_eq!(radius.total, 2);
        assert_eq!(radius.max_depth, 1);
    }

    #[test]
    fn test_impact_radius_grouped() {
        let (_store, cache, target_id) = make_cache_with_callers();
        let radius = get_impact_radius(&cache, target_id, 10);
        let groups = radius.grouped_by_depth();

        assert_eq!(groups.len(), 2); // depth 1 and depth 2
        assert_eq!(groups[0].0, 1);
        assert_eq!(groups[0].1.len(), 2); // b, c
        assert_eq!(groups[1].0, 2);
        assert_eq!(groups[1].1.len(), 1); // a
    }

    #[test]
    fn test_impact_radius_no_callers() {
        let store = Store::open_in_memory().unwrap();
        let file_id = store
            .upsert_file(&FileRecord {
                path: "solo.ts".into(),
                language: "typescript".into(),
                content_hash: vec![0; 16],
                mtime: 0,
                size: 10,
            })
            .unwrap();
        let node = store
            .insert_node(&NodeRecord {
                kind: "function".into(),
                name: "lonely".into(),
                qualified_name: "solo.lonely".into(),
                file_id,
                line_start: 1,
                line_end: 5,
                col_start: None,
                signature: None,
                return_type: None,
                receiver_type: None,
                is_exported: false,
                complexity: 1,
                decorators: None,
                metadata: None,
            })
            .unwrap();
        let cache = GraphCache::new();
        cache.load_from_store(&store).unwrap();

        let radius = get_impact_radius(&cache, node, 10);
        assert_eq!(radius.total, 0);
    }
}

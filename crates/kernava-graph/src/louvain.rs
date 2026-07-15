// kernava-graph: Louvain community detection
// P4 task 4.7: Modularity optimization over symmetrized CALLS edges.
//
// Algorithm: standard Louvain (Blondel et al. 2008), two-phase iterative.
//   Phase 1: each node in its own community. Repeatedly move nodes to neighbor
//            communities that maximize modularity gain. Repeat until no gain.
//   Phase 2: merge nodes in same community into supernodes, rebuild graph,
//            repeat Phase 1 on the coarsened graph.
//   Stop when modularity stops improving (ΔQ < 1e-6) or community count stops shrinking.
//
// Weighted: edge.confidence is the weight. Weighted modularity:
//   Q = (1/(2m)) Σ_ij [A_ij - k_i·k_j/(2m)] δ(c_i, c_j)
//   where m = Σ all weights, k_i = weighted degree of node i.
//
// Undirected: directed CALLS edges symmetrized — A_ij = A_ji = weight.
// This is a "module boundary" heuristic, not precise directed community structure.
// Directed Louvain (Leicht-Newman) deferred: YAGNI v1 — symmetrization is
// standard practice for call-graph community detection.
//
// Representative: argmax weighted degree within community.
//
// GraphCache is read-only here — we build a local adjacency, never mutate DashMaps.

use crate::cache::GraphCache;
use crate::model::NodeId;
use std::collections::{HashMap, HashSet};

/// A detected community of symbols.
#[derive(Debug, Clone)]
pub struct Community {
    pub members: Vec<NodeId>,
    /// Node ID of the most-connected member (argmax weighted degree).
    pub representative: NodeId,
    /// Sum of edge weights internal to this community.
    pub internal_edges: f64,
    /// Sum of edge weights crossing to other communities.
    pub external_edges: f64,
}

// ── Public entry point ──────────────────────────────────────────────────

/// Run Louvain community detection on the call graph.
/// Returns communities sorted by size (descending).
/// Nodes with no edges form singleton communities.
pub fn detect_communities(cache: &GraphCache) -> Vec<Community> {
    // 1. Build symmetrized weighted adjacency from GraphCache.
    let (adj, node_set, two_m) = build_adjacency(cache);

    if node_set.is_empty() {
        return Vec::new();
    }

    // 2. No edges → all singletons
    if two_m < 1e-10 {
        return build_singleton_communities(&node_set);
    }

    // 3. Map original NodeIds to compact usize indices for the whole pipeline.
    let nodes: Vec<NodeId> = node_set.iter().copied().collect();
    let id_to_idx: HashMap<NodeId, usize> = nodes.iter().enumerate().map(|(i, &n)| (n, i)).collect();
    let n = nodes.len();

    // Build usize-keyed adjacency + degrees
    let mut adj_u: HashMap<usize, Vec<(usize, f64)>> = HashMap::new();
    for (&node_id, neighbors) in &adj {
        let i = id_to_idx[&node_id];
        for (nbr, w) in neighbors {
            let j = id_to_idx[nbr];
            adj_u.entry(i).or_default().push((j, *w));
        }
    }
    let k_u: Vec<f64> = (0..n)
        .map(|i| adj_u.get(&i).map(|ns| ns.iter().map(|(_, w)| *w).sum()).unwrap_or(0.0))
        .collect();
    let idx_nodes: Vec<usize> = (0..n).collect();

    // 4. Phase 1 (local moving) on the original graph
    let mut comm: Vec<usize> = phase1(&adj_u, &k_u, &idx_nodes, two_m);

    // 5. Multi-level: Phase 1 + Phase 2 iterations
    const MAX_LEVELS: usize = 10;
    const Q_THRESHOLD: f64 = 1e-6;

    let mut prev_q = compute_modularity(&comm, &adj_u, two_m);

    for _level in 0..MAX_LEVELS {
        // Phase 2: aggregate into supernodes
        let (super_adj, super_k, super_ids, old_to_new) = phase2(&adj_u, &comm, &k_u);

        if super_ids.len() == idx_nodes.len() || super_ids.len() <= 1 {
            break; // no coarsening
        }

        let super_two_m: f64 = super_adj
            .values()
            .flat_map(|ns| ns.iter().map(|(_, w)| *w))
            .sum();

        // Phase 1 on coarsened graph
        let super_comm = phase1(&super_adj, &super_k, &super_ids, super_two_m);

        // Compose: relabel original nodes through old_to_new → super_comm
        for &i in &idx_nodes {
            let super_id = old_to_new[i];
            comm[i] = super_comm[super_id];
        }

        // Relabel communities to compact range for next phase2
        comm = compact_labels(&comm);

        let new_q = compute_modularity(&comm, &adj_u, two_m);
        if (new_q - prev_q).abs() < Q_THRESHOLD {
            break;
        }
        prev_q = new_q;
    }

    // 6. Build output: map usize comm → original NodeIds
    build_communities(&comm, &nodes, &adj, two_m)
}

// ── Internal helpers ────────────────────────────────────────────────────

/// Build symmetrized, deduplicated, weighted adjacency from GraphCache.forward.
/// Returns (adjacency keyed by NodeId, node_set, two_m = Σ A_ij).
fn build_adjacency(
    cache: &GraphCache,
) -> (HashMap<NodeId, Vec<(NodeId, f64)>>, HashSet<NodeId>, f64) {
    let mut deduped: HashMap<NodeId, HashMap<NodeId, f64>> = HashMap::new();
    let mut node_set: HashSet<NodeId> = HashSet::new();

    for entry in cache.forward.iter() {
        let src = *entry.key();
        node_set.insert(src);
        for (tgt, conf) in entry.value().iter() {
            node_set.insert(*tgt);
            let w = conf.max(0.0);
            // Symmetrized: A_ij = A_ji = weight (max if duplicate from both directions)
            *deduped.entry(src).or_default().entry(*tgt).or_default() = w;
            *deduped.entry(*tgt).or_default().entry(src).or_default() = w;
        }
    }
    for entry in cache.nodes.iter() {
        node_set.insert(*entry.key());
    }

    let adj: HashMap<NodeId, Vec<(NodeId, f64)>> = deduped
        .into_iter()
        .map(|(src, map)| (src, map.into_iter().collect::<Vec<_>>()))
    .collect();

    let two_m: f64 = adj
        .values()
        .flat_map(|ns| ns.iter().map(|(_, w)| *w))
        .sum();

    (adj, node_set, two_m)
}

/// Phase 1: local moving. Each node starts in its own community.
/// Repeatedly move nodes to neighbor communities that maximize modularity gain.
/// Uses incremental `sigma_tot` — no O(n²) scans.
/// Returns `comm: Vec<usize>` where `comm[i]` = community label of node `i`.
fn phase1(
    adj: &HashMap<usize, Vec<(usize, f64)>>,
    k: &[f64],
    nodes: &[usize],
    two_m: f64,
) -> Vec<usize> {
    let n = nodes.len();
    if two_m < 1e-10 || n == 0 {
        return (0..n).map(|i| i).collect();
    }

    let idx_of: HashMap<usize, usize> = nodes.iter().enumerate().map(|(i, &v)| (v, i)).collect();
    let mut comm: Vec<usize> = (0..n).collect();

    // Incremental sigma_tot: total weighted degree of each community.
    let mut sigma_tot: HashMap<usize, f64> = HashMap::new();
    for &v in nodes {
        sigma_tot.insert(v, k[idx_of[&v]]);
    }

    let mut improved = true;
    while improved {
        improved = false;
        for &v in nodes {
            let i = idx_of[&v];
            let current_comm = comm[i];
            let k_v = k[i];

            // Σ weight from v to each community
            let mut comm_weight: HashMap<usize, f64> = HashMap::new();
            if let Some(neighbors) = adj.get(&v) {
                for (nbr, w) in neighbors {
                    if *nbr == v {
                        continue;
                    }
                    let nbr_comm = comm[idx_of[nbr]];
                    *comm_weight.entry(nbr_comm).or_default() += w;
                }
            }

            // Σ weight from v to its current community
            let k_i_in = comm_weight.get(&current_comm).copied().unwrap_or(0.0);

            // Remove v from current community (incremental sigma_tot update)
            *sigma_tot.get_mut(&current_comm).unwrap() -= k_v;

            let stay_gain = k_i_in - sigma_tot[&current_comm] * k_v / two_m;

            let mut best_comm = current_comm;
            let mut best_gain = 0.0_f64;

            for (&c, &weight) in &comm_weight {
                if c == current_comm {
                    continue;
                }
                let st = sigma_tot.get(&c).copied().unwrap_or(0.0);
                let move_gain = weight - st * k_v / two_m;
                let delta = move_gain - stay_gain;
                if delta > best_gain {
                    best_gain = delta;
                    best_comm = c;
                }
            }

            // Assign v to best community
            *sigma_tot.get_mut(&best_comm).unwrap() += k_v;
            if best_comm != current_comm {
                comm[i] = best_comm;
                improved = true;
            }
        }
    }

    comm
}

/// Phase 2: aggregate communities into supernodes, rebuild graph.
/// Returns (super_adj, super_k, super_ids, old_to_new) where
/// `old_to_new[i]` maps old node index → supernode index.
fn phase2(
    adj: &HashMap<usize, Vec<(usize, f64)>>,
    comm: &[usize],
    _k: &[f64],
) -> (
    HashMap<usize, Vec<(usize, f64)>>,
    Vec<f64>,
    Vec<usize>,
    Vec<usize>,
) {
    // Map old community labels → compact supernode ids
    let mut label_to_super: HashMap<usize, usize> = HashMap::new();
    for &label in comm {
        let len = label_to_super.len();
        label_to_super.entry(label).or_insert(len);
    }
    let old_to_new: Vec<usize> = comm.iter().map(|&c| label_to_super[&c]).collect();
    let num_supers = label_to_super.len();

    // Build supernode adjacency: merge edges between communities.
    // Self-loops = internal edges (summed from both directions → already 2× internal).
    let mut super_adj_map: HashMap<usize, HashMap<usize, f64>> = HashMap::new();
    for (&v, neighbors) in adj {
        let sv = old_to_new[v];
        for (nbr, w) in neighbors {
            let sn = old_to_new[*nbr];
            *super_adj_map.entry(sv).or_default().entry(sn).or_default() += w;
        }
    }

    let super_adj: HashMap<usize, Vec<(usize, f64)>> = super_adj_map
        .into_iter()
        .map(|(s, m)| (s, m.into_iter().collect::<Vec<_>>()))
        .collect();

    let super_k: Vec<f64> = (0..num_supers)
        .map(|s| {
            super_adj
                .get(&s)
                .map(|ns| ns.iter().map(|(_, w)| *w).sum::<f64>())
                .unwrap_or(0.0)
        })
        .collect();

    let super_ids: Vec<usize> = (0..num_supers).collect();

    (super_adj, super_k, super_ids, old_to_new)
}

/// Relabel community assignments to a compact [0..num_comms) range.
fn compact_labels(comm: &[usize]) -> Vec<usize> {
    let mut label_map: HashMap<usize, usize> = HashMap::new();
    for &c in comm {
        let len = label_map.len();
        label_map.entry(c).or_insert(len);
    }
    comm.iter().map(|&c| label_map[&c]).collect()
}

/// Compute weighted undirected modularity Q.
/// Q = Σ_c [ Σ_in_c/(2m) - (Σ_tot_c/(2m))² ]
fn compute_modularity(
    comm: &[usize],
    adj: &HashMap<usize, Vec<(usize, f64)>>,
    two_m: f64,
) -> f64 {
    if two_m < 1e-10 {
        return 0.0;
    }
    let n = comm.len();
    let mut comm_internal: HashMap<usize, f64> = HashMap::new();
    let mut comm_total: HashMap<usize, f64> = HashMap::new();
    for i in 0..n {
        let ci = comm[i];
        if let Some(neighbors) = adj.get(&i) {
            let kn: f64 = neighbors.iter().map(|(_, w)| *w).sum();
            *comm_total.entry(ci).or_default() += kn;
            for (nbr, w) in neighbors {
                if comm[*nbr] == ci {
                    *comm_internal.entry(ci).or_default() += w;
                }
            }
        }
    }
    // internal counted twice (both directions); Σ_in = internal/2
    let mut q = 0.0_f64;
    for (c, internal) in &comm_internal {
        let tot = comm_total.get(c).copied().unwrap_or(0.0);
        q += internal / (2.0 * two_m) - (tot / two_m).powi(2);
    }
    q
}

/// Build final Community results from comm assignment + original adjacency.
fn build_communities(
    comm: &[usize],
    nodes: &[NodeId],
    adj: &HashMap<NodeId, Vec<(NodeId, f64)>>,
    two_m: f64,
) -> Vec<Community> {
    let _ = two_m;
    let mut comm_members: HashMap<usize, Vec<NodeId>> = HashMap::new();
    for (i, &c) in comm.iter().enumerate() {
        comm_members.entry(c).or_default().push(nodes[i]);
    }

    // Weighted degree per original node
    let k: HashMap<NodeId, f64> = nodes
        .iter()
        .map(|&n| {
            let deg = adj
                .get(&n)
                .map(|ns| ns.iter().map(|(_, w)| *w).sum::<f64>())
                .unwrap_or(0.0);
            (n, deg)
        })
        .collect();

    let mut communities = Vec::with_capacity(comm_members.len());
    for (_, members) in comm_members {
        let rep = members
            .iter()
            .copied()
            .max_by(|a, b| {
                let ka = k.get(a).copied().unwrap_or(0.0);
                let kb = k.get(b).copied().unwrap_or(0.0);
                ka.partial_cmp(&kb).unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap_or(members[0]);

        let member_set: HashSet<NodeId> = members.iter().copied().collect();
        let mut internal = 0.0_f64;
        let mut external = 0.0_f64;
        for &n in &members {
            if let Some(neighbors) = adj.get(&n) {
                for (nbr, w) in neighbors {
                    if member_set.contains(nbr) {
                        internal += w;
                    } else {
                        external += w;
                    }
                }
            }
        }
        internal /= 2.0; // undirected: counted from both directions

        communities.push(Community {
            members,
            representative: rep,
            internal_edges: internal,
            external_edges: external,
        });
    }

    communities.sort_by(|a, b| b.members.len().cmp(&a.members.len()));
    communities
}

fn build_singleton_communities(node_set: &HashSet<NodeId>) -> Vec<Community> {
    let mut communities: Vec<Community> = node_set
        .iter()
        .map(|&n| Community {
            members: vec![n],
            representative: n,
            internal_edges: 0.0,
            external_edges: 0.0,
        })
        .collect();
    communities.sort_by(|a, b| b.members.len().cmp(&a.members.len()));
    communities
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernava_store::Store;

    fn make_cache_from_fixture(fixture_name: &str) -> (GraphCache, String) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let db_path = format!("/tmp/kernava_louvain_{nanos}.db");
        let fixture_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../../crates/kernava-indexer/tests/fixtures/{fixture_name}"))
            .canonicalize()
            .unwrap();
        let mut store = Store::open(&db_path).unwrap();
        kernava_indexer::builder::index_full(&mut store, &fixture_root).unwrap();
        let cache = GraphCache::new();
        cache.load_from_store(&store).unwrap();
        drop(store);
        let _ = std::fs::remove_file(&db_path);
        (cache, fixture_root.to_string_lossy().into_owned())
    }

    #[test]
    fn test_louvain_ts_chain() {
        // ts-chain: a→b→c, linear chain. Louvain should find communities.
        // With 3 nodes and 2 edges, likely 1-2 communities.
        let (cache, _) = make_cache_from_fixture("ts-chain");
        let communities = detect_communities(&cache);
        assert!(!communities.is_empty(), "should detect communities");
        // Total members should equal node count
        let total: usize = communities.iter().map(|c| c.members.len()).sum();
        assert_eq!(total, cache.node_count(), "all nodes should be in a community");
    }

    #[test]
    fn test_louvain_ts_small() {
        // ts-small: 7 nodes, 3 resolved edges (main→add, main→multiply, main→helper)
        // main is a hub connecting to all three libraries.
        let (cache, _) = make_cache_from_fixture("ts-small");
        let communities = detect_communities(&cache);
        assert!(!communities.is_empty(), "should detect communities");
        let total: usize = communities.iter().map(|c| c.members.len()).sum();
        assert_eq!(total, cache.node_count(), "all nodes should be in a community");
        // With 4 connected nodes (main, add, multiply, helper) + 3 singletons
        // (process, other.helper, dead_function), the connected component should
        // likely form 1-2 communities.
        let largest = &communities[0];
        assert!(largest.members.len() >= 2, "largest community should have ≥2 members");
    }

    #[test]
    fn test_louvain_empty_graph() {
        let cache = GraphCache::new();
        let communities = detect_communities(&cache);
        assert!(communities.is_empty(), "empty graph should have no communities");
    }

    #[test]
    fn test_louvain_no_edges() {
        // Nodes with no edges → all singletons
        let cache = GraphCache::new();
        cache.nodes.insert(1, crate::model::Node {
            id: 1, kind: "function".into(), name: "a".into(),
            qualified_name: "a.ts.a".into(), file_id: 1,
            line_start: 1, line_end: 1,
            signature: None, return_type: None, receiver_type: None,
            is_exported: false, complexity: 1,
        });
        cache.nodes.insert(2, crate::model::Node {
            id: 2, kind: "function".into(), name: "b".into(),
            qualified_name: "b.ts.b".into(), file_id: 2,
            line_start: 1, line_end: 1,
            signature: None, return_type: None, receiver_type: None,
            is_exported: false, complexity: 1,
        });
        let communities = detect_communities(&cache);
        assert_eq!(communities.len(), 2, "should have 2 singleton communities");
        for c in &communities {
            assert_eq!(c.members.len(), 1, "each community should be a singleton");
            assert_eq!(c.internal_edges, 0.0);
        }
    }
}

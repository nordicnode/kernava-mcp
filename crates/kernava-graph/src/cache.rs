// kernava-graph: in-RAM graph cache
// P2 task 2.2-2.3: DashMap-backed adjacency + bulk load from SQLite.

use crate::model::{Edge, FileId, Node, NodeId};
use dashmap::DashMap;
use kernava_store::Store;

/// In-RAM graph cache. Four DashMaps for O(1) lookups.
/// Read path is lock-free per-shard; mutations flow through a single writer
/// (sync_upsert_node / sync_delete_file) to avoid concurrent read-during-write races.
pub struct GraphCache {
    /// name → list of node IDs (multiple symbols can share a name)
    pub by_name: DashMap<String, Vec<NodeId>>,
    /// qualified_name → node ID (unique)
    pub by_qualified: DashMap<String, NodeId>,
    /// caller → [(callee, confidence)] (forward adjacency)
    pub forward: DashMap<NodeId, Vec<(NodeId, f64)>>,
    /// callee → [(caller, confidence)] (reverse adjacency)
    pub reverse: DashMap<NodeId, Vec<(NodeId, f64)>>,
    /// node_id → Node (full metadata for tool responses)
    pub nodes: DashMap<NodeId, Node>,
    /// file_id → set of node_ids in that file (for sync_delete_file)
    pub file_nodes: DashMap<i64, Vec<NodeId>>,
}

impl GraphCache {
    pub fn new() -> Self {
        Self {
            by_name: DashMap::new(),
            by_qualified: DashMap::new(),
            forward: DashMap::new(),
            reverse: DashMap::new(),
            nodes: DashMap::new(),
            file_nodes: DashMap::new(),
        }
    }

    /// Bulk load all nodes and edges from SQLite into DashMaps.
    /// Called once on server startup (or after a full reindex).
    pub fn load_from_store(&self, store: &Store) -> anyhow::Result<()> {
        // Load all nodes
        let node_rows = store.get_all_nodes()?;
        for row in node_rows {
            let node: Node = row.into();
            self.nodes.insert(node.id, node.clone());
            self.by_name
                .entry(node.name.clone())
                .or_default()
                .push(node.id);
            self.by_qualified
                .insert(node.qualified_name.clone(), node.id);
            self.file_nodes
                .entry(node.file_id)
                .or_default()
                .push(node.id);
        }

        // Load all edges — only "calls" edges populate call adjacency maps.
        // ponytail: filter on edge_type when a second edge type appears in builder.
        let edge_rows = store.get_all_edges()?;
        for row in edge_rows {
            let edge: Edge = row.into();
            if edge.edge_type.to_lowercase() != "calls" {
                continue;
            }
            if let Some(target) = edge.target {
                // Forward: caller → [(callee, confidence)]
                self.forward
                    .entry(edge.source)
                    .or_default()
                    .push((target, edge.confidence));
                // Reverse: callee → [(caller, confidence)]
                self.reverse
                    .entry(target)
                    .or_default()
                    .push((edge.source, edge.confidence));
            }
        }

        Ok(())
    }

    /// Number of nodes cached.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Number of resolved call edges cached (forward adjacency entries).
    /// Excludes unresolved edges (NULL target) and non-calls edge types.
    pub fn edge_count(&self) -> usize {
        self.forward.iter().map(|e| e.value().len()).sum()
    }

    /// Look up a node by qualified name.
    pub fn get_node(&self, qualified_name: &str) -> Option<Node> {
        let id = *self.by_qualified.get(qualified_name)?;
        self.nodes.get(&id).map(|n| n.value().clone())
    }

    /// Look up node IDs by simple name.
    pub fn get_by_name(&self, name: &str) -> Vec<NodeId> {
        self.by_name
            .get(name)
            .map(|v| v.value().clone())
            .unwrap_or_default()
    }

    /// Get all callees of a node: [(target_id, confidence)].
    pub fn get_callees(&self, node_id: NodeId) -> Vec<(NodeId, f64)> {
        self.forward
            .get(&node_id)
            .map(|v| v.value().clone())
            .unwrap_or_default()
    }

    /// Get all callers of a node: [(source_id, confidence)].
    pub fn get_callers(&self, node_id: NodeId) -> Vec<(NodeId, f64)> {
        self.reverse
            .get(&node_id)
            .map(|v| v.value().clone())
            .unwrap_or_default()
    }

    /// Bulk-replace a file's nodes/edges in the cache after re-indexing.
    /// Called by watcher after builder::index_file completes for a changed file.
    /// Removes old entries for these file_ids first, then inserts the new ones.
    pub fn sync_upsert_file(&self, file_id: FileId, nodes: Vec<Node>, edges: Vec<Edge>) {
        // Evict old entries for this file
        self.sync_delete_file(file_id);

        // Insert new nodes
        for node in &nodes {
            self.by_name
                .entry(node.name.clone())
                .or_default()
                .push(node.id);
            self.by_qualified
                .insert(node.qualified_name.clone(), node.id);
            self.file_nodes.entry(file_id).or_default().push(node.id);
            self.nodes.insert(node.id, node.clone());
        }

        // Insert new call edges (forward + reverse adjacency)
        for edge in &edges {
            if edge.edge_type.to_lowercase() != "calls" {
                continue;
            }
            if let Some(target) = edge.target {
                self.forward
                    .entry(edge.source)
                    .or_default()
                    .push((target, edge.confidence));
                self.reverse
                    .entry(target)
                    .or_default()
                    .push((edge.source, edge.confidence));
            }
        }
    }

    /// Evict all of a file's nodes/edges from the cache.
    /// Called before re-indexing a file, or when a file is deleted.
    // SAFETY: caller must serialize via single writer (task 3.3) — concurrent
    // sync_delete_file calls risk cross-map deadlock on forward/reverse locks.
    pub fn sync_delete_file(&self, file_id: FileId) {
        // Get the node IDs for this file
        let node_ids = match self.file_nodes.get(&file_id) {
            Some(entry) => entry.value().clone(),
            None => return,
        };

        for node_id in &node_ids {
            // Remove outgoing edges (forward) and their reverse counterparts
            if let Some(outgoing) = self.forward.get(node_id) {
                for (target, _) in outgoing.value().iter() {
                    if let Some(mut rev) = self.reverse.get_mut(target) {
                        rev.value_mut().retain(|(src, _)| src != node_id);
                        if rev.value().is_empty() {
                            drop(rev);
                            self.reverse.remove(target);
                        }
                    }
                }
                drop(outgoing);
            }
            self.forward.remove(node_id);

            // Remove incoming edges (reverse) and their forward counterparts
            if let Some(incoming) = self.reverse.get(node_id) {
                for (source, _) in incoming.value().iter() {
                    if let Some(mut fwd) = self.forward.get_mut(source) {
                        fwd.value_mut().retain(|(tgt, _)| tgt != node_id);
                        if fwd.value().is_empty() {
                            drop(fwd);
                            self.forward.remove(source);
                        }
                    }
                }
                drop(incoming);
            }
            self.reverse.remove(node_id);

            // Remove from nodes, by_qualified, by_name in one read
            if let Some(node) = self.nodes.get(node_id) {
                let qn = node.value().qualified_name.clone();
                let name = node.value().name.clone();
                drop(node);
                self.by_qualified.remove(&qn);
                if let Some(mut entry) = self.by_name.get_mut(&name) {
                    entry.value_mut().retain(|id| id != node_id);
                    if entry.value().is_empty() {
                        drop(entry);
                        self.by_name.remove(&name);
                    }
                }
            }

            // Remove from nodes
            self.nodes.remove(node_id);
        }

        // Clear file_nodes entry
        self.file_nodes.remove(&file_id);
    }
}

impl Default for GraphCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernava_store::{EdgeRecord, FileRecord, NodeRecord, Store};

    fn make_store_with_data() -> Store {
        let store = Store::open_in_memory().unwrap();

        let file_id = store
            .upsert_file(&FileRecord {
                path: "src/main.ts".into(),
                language: "typescript".into(),
                content_hash: vec![0; 16],
                mtime: 0,
                size: 100,
            })
            .unwrap();

        let node_a = store
            .insert_node(&NodeRecord {
                kind: "function".into(),
                name: "foo".into(),
                qualified_name: "src/main.foo".into(),
                file_id,
                line_start: 1,
                line_end: 5,
                col_start: None,
                signature: None,
                return_type: None,
                receiver_type: None,
                is_exported: true,
                complexity: 1,
                decorators: None,
                metadata: None,
            })
            .unwrap();
        let node_b = store
            .insert_node(&NodeRecord {
                kind: "function".into(),
                name: "bar".into(),
                qualified_name: "src/main.bar".into(),
                file_id,
                line_start: 6,
                line_end: 10,
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

        store
            .insert_edge(&EdgeRecord {
                source_id: node_a,
                target_id: Some(node_b),
                edge_type: "calls".into(),
                confidence: 0.95,
                file_id: Some(file_id),
                line: Some(3),
                metadata: Some("ImportMap".into()),
            })
            .unwrap();

        // Non-calls edge — should be filtered out by load_from_store
        let node_c = store
            .insert_node(&NodeRecord {
                kind: "function".into(),
                name: "baz".into(),
                qualified_name: "src/main.baz".into(),
                file_id,
                line_start: 11,
                line_end: 15,
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
        store
            .insert_edge(&EdgeRecord {
                source_id: node_a,
                target_id: Some(node_c),
                edge_type: "references".into(),
                confidence: 0.5,
                file_id: Some(file_id),
                line: Some(8),
                metadata: None,
            })
            .unwrap();

        store
    }

    #[test]
    fn test_load_from_store() {
        let store = make_store_with_data();
        let cache = GraphCache::new();
        cache.load_from_store(&store).unwrap();

        assert_eq!(cache.node_count(), 3);
        assert_eq!(cache.edge_count(), 1);
    }

    #[test]
    fn test_lookup_by_qualified_name() {
        let store = make_store_with_data();
        let cache = GraphCache::new();
        cache.load_from_store(&store).unwrap();

        let node = cache.get_node("src/main.foo").unwrap();
        assert_eq!(node.name, "foo");
        assert!(node.is_exported);
    }

    #[test]
    fn test_lookup_by_name() {
        let store = make_store_with_data();
        let cache = GraphCache::new();
        cache.load_from_store(&store).unwrap();

        let ids = cache.get_by_name("bar");
        assert_eq!(ids.len(), 1);
    }

    #[test]
    fn test_forward_and_reverse_adjacency() {
        let store = make_store_with_data();
        let cache = GraphCache::new();
        cache.load_from_store(&store).unwrap();

        // foo calls bar
        let bar_id = *cache.by_qualified.get("src/main.bar").unwrap();
        let foo_id = *cache.by_qualified.get("src/main.foo").unwrap();

        let callees = cache.get_callees(foo_id);
        assert_eq!(callees.len(), 1);
        assert_eq!(callees[0].0, bar_id);

        let callers = cache.get_callers(bar_id);
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].0, foo_id);
    }

    #[test]
    fn test_sync_delete_file() {
        let store = make_store_with_data();
        let file_id = store.get_file_id("src/main.ts").unwrap().unwrap();
        let cache = GraphCache::new();
        cache.load_from_store(&store).unwrap();
        assert_eq!(cache.node_count(), 3);

        cache.sync_delete_file(file_id);

        assert_eq!(cache.node_count(), 0);
        assert!(cache.get_node("src/main.foo").is_none());
        assert!(cache.get_node("src/main.bar").is_none());
        assert!(cache.get_node("src/main.baz").is_none());
    }

    #[test]
    fn test_sync_upsert_file() {
        let store = make_store_with_data();
        let file_id = store.get_file_id("src/main.ts").unwrap().unwrap();
        let cache = GraphCache::new();
        cache.load_from_store(&store).unwrap();
        assert_eq!(cache.node_count(), 3);

        // Evict old, then upsert one new node
        let new_node = Node {
            id: 99,
            kind: "function".into(),
            name: "new_f".into(),
            qualified_name: "src/main.new_f".into(),
            file_id,
            line_start: 1,
            line_end: 1,
            signature: None,
            return_type: None,
            receiver_type: None,
            is_exported: false,
            complexity: 0,
        };
        cache.sync_upsert_file(file_id, vec![new_node], vec![]);

        assert_eq!(cache.node_count(), 1);
        assert_eq!(cache.get_node("src/main.new_f").unwrap().name, "new_f");
        assert!(cache.get_node("src/main.foo").is_none());
    }
}

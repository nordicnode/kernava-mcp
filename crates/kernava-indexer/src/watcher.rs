use anyhow::Result;
use kernava_graph::GraphCache;
use kernava_store::Store;
use notify::{RecursiveMode, Watcher as _};
/// File watcher: notify-based event loop with XXH3 content-hash dedup.
/// On content change, calls `index_incremental` for the changed file,
/// then syncs the GraphCache. On file deletion, removes the file's
/// symbols from store + cache.
/// ponytail: synchronous poll loop (no tokio). P3 server actor wraps this
/// in a tokio task and owns the single-writer channel to GraphCache.
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;
use tracing::debug;

use crate::builder::{index_incremental_with_config, xxhash128_bytes};
use crate::config::IndexerConfig;
use crate::parser::Language;

pub struct Watcher {
    _watcher: notify::RecommendedWatcher,
    rx: mpsc::Receiver<notify::Result<notify::Event>>,
}

impl Watcher {
    pub fn new(project_root: &Path) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                let _ = tx.send(res);
            })?;
        watcher.watch(project_root, RecursiveMode::Recursive)?;
        Ok(Self {
            _watcher: watcher,
            rx,
        })
    }

    /// Drain pending filesystem events into raw candidate sets (no store access).
    /// Returns (created_or_modified, removed) Sets of source-file paths.
    /// Blocks up to 150ms on `recv_timeout` — this is the debounce window.
    /// Does NOT touch the store — safe to call without holding any lock.
    pub fn drain_events(&self) -> (HashSet<PathBuf>, HashSet<PathBuf>) {
        let mut candidates: HashSet<PathBuf> = HashSet::new();
        let mut deleted_candidates: HashSet<PathBuf> = HashSet::new();
        while let Ok(ev) = self.rx.recv_timeout(Duration::from_millis(150)) {
            if let Ok(notify::Event { kind, paths, .. }) = ev {
                if matches!(kind, notify::event::EventKind::Remove(_)) {
                    for p in &paths {
                        if Language::from_path(p).is_some() {
                            deleted_candidates.insert(p.clone());
                        }
                    }
                } else if matches!(
                    kind,
                    notify::event::EventKind::Create(_) | notify::event::EventKind::Modify(_)
                ) {
                    for p in paths {
                        if Language::from_path(&p).is_some() {
                            candidates.insert(p);
                        }
                    }
                }
            }
        }
        (candidates, deleted_candidates)
    }

    /// Filter raw candidates against the store: hash-dedup for changed,
    /// existence-check for deleted. Requires store access (short critical section).
    /// Returns (changed, deleted) Vecs ready for `process`.
    pub fn filter_changes(
        &self,
        candidates: HashSet<PathBuf>,
        deleted_candidates: HashSet<PathBuf>,
        store: &Store,
    ) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
        let mut deleted = Vec::new();
        for p in deleted_candidates {
            if store.get_file_id(&p.to_string_lossy())?.is_some() {
                deleted.push(p);
            }
        }
        let changed = self.dedup(candidates, store)?;
        Ok((changed, deleted))
    }

    /// Block until file changes are detected. Returns (changed, deleted) paths.
    /// Convenience method combining `drain_events` + `filter_changes`.
    /// ponytail: 150ms drain window acts as debounce. Non-source files skipped.
    pub fn poll_changes(&self, store: &Store) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
        let (candidates, deleted_candidates) = self.drain_events();
        self.filter_changes(candidates, deleted_candidates, store)
    }

    /// Filter candidates by content hash — only files whose hash actually changed.
    fn dedup(&self, candidates: HashSet<PathBuf>, store: &Store) -> Result<Vec<PathBuf>> {
        let mut changed = Vec::new();
        for path in candidates {
            let Ok(content) = std::fs::read(&path) else {
                debug!("file vanished: {:?}", path);
                continue;
            };
            let current_hash = xxhash128_bytes(&content);
            let stored = store.get_file_hash(&path.to_string_lossy())?;
            match stored {
                Some(prev) if prev == current_hash => {
                    debug!("hash unchanged, skipping: {:?}", path);
                }
                _ => {
                    debug!("content changed: {:?}", path);
                    changed.push(path);
                }
            }
        }
        Ok(changed)
    }

    /// Delete a file from store + cache atomically.
    /// Ordering: get file_id → txn { delete_file_symbols (FTS5+nodes+edges)
    /// → delete_file_row (files row) → commit } → cache.sync_delete_file.
    /// Mirrors builder's atomic-per-file contract. No partial store state on crash.
    /// SAFETY: caller must ensure single-writer access to Store and GraphCache
    /// (same constraint as sync_delete_file — it takes forward+reverse locks).
    fn delete_file(store: &mut Store, cache: &GraphCache, path: &Path) -> Result<()> {
        let path_str = path.to_string_lossy();
        let file_id = match store.get_file_id(&path_str)? {
            Some(fid) => fid,
            None => return Ok(()), // already gone — nothing to do
        };
        // Atomic: FTS5 + nodes + edges + import_edges + files row in one txn
        let txn = store.transaction()?;
        txn.delete_file_symbols(file_id)?;
        txn.delete_file_row(file_id)?;
        txn.commit()?;
        // Evict from in-RAM graph (after commit, so cache reflects committed state)
        cache.sync_delete_file(file_id);
        Ok(())
    }

    /// Index changed files + sync GraphCache. Delete removed files + sync cache.
    /// ponytail: caller must ensure single-writer access to Store and GraphCache.
    pub fn process(
        changed: Vec<PathBuf>,
        deleted: Vec<PathBuf>,
        store: &mut Store,
        cache: &GraphCache,
        config: &IndexerConfig,
    ) -> Result<()> {
        // Handle deletions first (so reverse-deps re-index sees updated graph)
        for path in &deleted {
            Self::delete_file(store, cache, path)?;
        }

        if changed.is_empty() && deleted.is_empty() {
            return Ok(());
        }

        let results = index_incremental_with_config(store, changed, config)?;

        for r in &results {
            if let Some(fid) = store.get_file_id(&r.file_path)? {
                let nodes = store.get_nodes_for_file(fid)?;
                let edges: Vec<kernava_store::EdgeRow> = store
                    .get_all_edges()?
                    .into_iter()
                    .filter(|e: &kernava_store::EdgeRow| e.file_id == Some(fid))
                    .collect();

                let graph_nodes: Vec<kernava_graph::Node> =
                    nodes.into_iter().map(kernava_graph::Node::from).collect();
                let graph_edges: Vec<kernava_graph::Edge> =
                    edges.into_iter().map(kernava_graph::Edge::from).collect();

                cache.sync_upsert_file(fid, graph_nodes, graph_edges);
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn copy_fixture_to_tmp() -> PathBuf {
        let src = {
            let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            p.push("tests");
            p.push("fixtures");
            p.push("ts-small");
            p
        };
        let dst = std::env::temp_dir().join(format!(
            "kernava-watch-{}",
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dst).unwrap();
        for entry in std::fs::read_dir(&src).unwrap() {
            let entry = entry.unwrap();
            std::fs::copy(entry.path(), dst.join(entry.file_name())).unwrap();
        }
        dst.canonicalize().unwrap()
    }

    #[test]
    fn test_watcher_dedup_skips_unchanged() {
        let dir = copy_fixture_to_tmp();
        let mut store = Store::open_in_memory().unwrap();
        crate::builder::index_full(&mut store, &dir).unwrap();

        let watcher = Watcher::new(&dir).unwrap();
        let candidates = HashSet::new();
        let changed = watcher.dedup(candidates, &store).unwrap();
        assert!(changed.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_watcher_dedup_detects_changed() {
        let dir = copy_fixture_to_tmp();
        let mut store = Store::open_in_memory().unwrap();
        crate::builder::index_full(&mut store, &dir).unwrap();

        // Modify math.ts
        let math_path = dir.join("math.ts");
        std::fs::write(&math_path, "export function changed() { return 42; }\n").unwrap();

        let watcher = Watcher::new(&dir).unwrap();
        let mut candidates = HashSet::new();
        candidates.insert(math_path.clone());
        let changed = watcher.dedup(candidates, &store).unwrap();
        assert_eq!(changed.len(), 1);
        assert!(changed[0].ends_with("math.ts"));

        // Unchanged file should be filtered out
        let mut candidates2 = HashSet::new();
        candidates2.insert(dir.join("util.ts"));
        let changed2 = watcher.dedup(candidates2, &store).unwrap();
        assert!(
            changed2.is_empty(),
            "util.ts unchanged → should be filtered"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_watcher_process_syncs_cache() {
        let dir = copy_fixture_to_tmp();
        let mut store = Store::open_in_memory().unwrap();
        crate::builder::index_full(&mut store, &dir).unwrap();

        // Load cache from store (simulates server startup)
        let cache = GraphCache::new();
        cache.load_from_store(&store).unwrap();
        assert_eq!(cache.node_count(), 7, "initial cache should have 7 nodes");
        assert_eq!(
            cache.edge_count(),
            3,
            "initial cache should have 3 resolved edges"
        );

        // Modify math.ts: replace add/multiply with a single new function
        let math_path = dir.join("math.ts");
        std::fs::write(
            &math_path,
            "export function newfunc(): number { return 42; }\n",
        )
        .unwrap();

        // Simulate watcher detecting the change
        let watcher = Watcher::new(&dir).unwrap();
        let mut candidates = HashSet::new();
        candidates.insert(math_path);
        let changed = watcher.dedup(candidates, &store).unwrap();
        assert_eq!(changed.len(), 1);

        // Process: index_incremental + sync cache
        Watcher::process(
            changed,
            vec![], // no deletions in this test
            &mut store,
            &cache,
            &crate::config::IndexerConfig::default(),
        )
        .unwrap();

        // newfunc should be in cache
        let qname = format!("{}.newfunc", dir.join("math.ts").to_string_lossy());
        assert!(
            cache.get_node(&qname).is_some(),
            "newfunc should be in cache after process"
        );

        // old functions (add, multiply) should be gone from cache
        let add_qname = format!("{}.add", dir.join("math.ts").to_string_lossy());
        assert!(
            cache.get_node(&add_qname).is_none(),
            "old add should be evicted from cache"
        );

        // main.ts was reverse-dep of math.ts → re-indexed. Its edges to add/multiply
        // are now unresolved (targets gone). cache edge_count reflects only resolved edges.
        // The exact count depends on whether main's edges to add/multiply got NULL target.
        // Just verify cache is consistent with store.
        let store_edges = store.get_all_edges().unwrap();
        let store_resolved = store_edges.iter().filter(|e| e.target_id.is_some()).count();
        assert_eq!(
            cache.edge_count(),
            store_resolved,
            "cache edge count should match store resolved edge count"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_watcher_process_deletes_file() {
        let dir = copy_fixture_to_tmp();
        let mut store = Store::open_in_memory().unwrap();
        crate::builder::index_full(&mut store, &dir).unwrap();

        let cache = GraphCache::new();
        cache.load_from_store(&store).unwrap();
        let initial_nodes = cache.node_count();
        assert!(initial_nodes > 0);

        // Delete math.ts from disk
        let math_path = dir.join("math.ts");
        let math_qname_prefix = format!("{}.", math_path.to_string_lossy());
        std::fs::remove_file(&math_path).unwrap();

        // Process: delete math.ts from store + cache
        Watcher::process(
            vec![],
            vec![math_path.clone()],
            &mut store,
            &cache,
            &crate::config::IndexerConfig::default(),
        )
        .unwrap();

        // math.ts symbols gone from store
        assert!(
            store
                .get_file_id(&math_path.to_string_lossy())
                .unwrap()
                .is_none(),
            "deleted file should be gone from store"
        );
        let remaining_nodes = store.get_all_edges().unwrap();
        let _ = remaining_nodes; // store still functional

        // math.ts symbols gone from cache
        for entry in cache.by_qualified.iter() {
            let qn = entry.key();
            assert!(
                !qn.starts_with(&math_qname_prefix),
                "math.ts symbol should be evicted from cache: {qn}"
            );
        }
        assert!(
            cache.node_count() < initial_nodes,
            "cache node count should decrease after deletion"
        );

        // FTS5 should not return deleted symbols — check by qualified_name prefix
        let fts_results = kernava_store::fts5::search_symbols(store.conn(), "add", 10).unwrap();
        for r in &fts_results {
            assert!(
                !r.qualified_name.starts_with(&math_qname_prefix),
                "FTS5 should not return deleted file's symbols: {}",
                r.qualified_name
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}

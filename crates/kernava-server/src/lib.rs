// kernava-server: library crate exposing MCP handler + server startup logic.

pub mod handler;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use handler::{AppState, KernavaHandler};
use kernava_graph::GraphCache;
use kernava_store::Store;
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use tokio_util::sync::CancellationToken;
use tracing::info;

/// Load `kernava.toml` from project root if present, else return defaults.
pub fn load_config(project_root: &str) -> anyhow::Result<kernava_indexer::IndexerConfig> {
    let root = PathBuf::from(project_root)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(project_root));
    let config_path = root.join("kernava.toml");
    if config_path.exists() {
        let text = std::fs::read_to_string(&config_path)?;
        let config: kernava_indexer::IndexerConfig = toml::from_str(&text)?;
        tracing::info!("Loaded config from {}", config_path.display());
        Ok(config)
    } else {
        Ok(kernava_indexer::IndexerConfig::default())
    }
}

/// Spawn a background file-watcher thread that keeps store + GraphCache fresh.
/// Uses std::thread (not spawn_blocking) — the watcher is long-lived and uses
/// blocking mpsc + file I/O. The 150ms `recv_timeout` in `drain_events` paces
/// the loop; cancellation is checked after each poll cycle via `ct.is_cancelled()`.
/// SAFETY: watcher holds `state.store.lock()` only during `filter_changes` +
/// `process` (short critical section). Event drain happens WITHOUT the lock.
/// GraphCache writes happen under the store lock — single-writer invariant
/// preserved (same mutex tool handlers use).
fn spawn_watcher(
    state: Arc<AppState>,
    project_root: PathBuf,
    ct: CancellationToken,
) -> anyhow::Result<JoinHandle<()>> {
    let watcher = kernava_indexer::watcher::Watcher::new(&project_root)?;
    let handle = std::thread::spawn(move || {
        loop {
            if ct.is_cancelled() {
                break;
            }
            // Drain events WITHOUT the store lock — 150ms debounce window.
            // Idle server never touches the mutex → tool handlers never stall.
            let (candidates, deleted_candidates) = watcher.drain_events();
            if candidates.is_empty() && deleted_candidates.is_empty() {
                continue;
            }
            // Single critical section: filter (short store reads) + process (writes).
            // The mutex serializes with tool-handler index_project — no interleaving.
            let mut store = match state.store.lock() {
                Ok(g) => g,
                Err(_) => break, // poisoned
            };
            let (changed, deleted) =
                match watcher.filter_changes(candidates, deleted_candidates, &store) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("watcher filter error: {e}");
                        continue;
                    }
                };
            if !changed.is_empty() || !deleted.is_empty() {
                if let Err(e) = kernava_indexer::watcher::Watcher::process(
                    changed,
                    deleted,
                    &mut store,
                    &state.graph,
                    &state.config,
                ) {
                    tracing::warn!("watcher process error: {e}");
                }
            }
        }
        tracing::info!("Watcher thread exiting.");
    });
    Ok(handle)
}

/// Build shared AppState (store + graph + config) and spawn the file-watcher
/// thread. Used by both the streamable-HTTP and stdio server transports so the
/// in-RAM cache + live file watching work identically either way.
///
/// Returns `(state, cancellation_token, watcher_handle)`. If the watcher fails
/// to start the server runs without live file watching (non-fatal) and the
/// handle is `None`; the caller still owns `ct` and should cancel it on shutdown.
fn build_state_and_watcher(
    db_path: &str,
    project_root: &str,
) -> anyhow::Result<(Arc<AppState>, CancellationToken, Option<JoinHandle<()>>)> {
    info!("Opening database at {db_path}");
    let store = Store::open(db_path)?;

    // If DB has existing data, warm the cache
    let graph = GraphCache::new();
    let stats = store.stats()?;
    if stats.node_count > 0 {
        info!(
            "Warming graph cache: {} nodes, {} edges",
            stats.node_count, stats.edge_count
        );
        graph.load_from_store(&store)?;
    }

    let config = Arc::new(load_config(project_root)?);
    let state = Arc::new(AppState {
        store: Mutex::new(store),
        graph,
        project_root: PathBuf::from(project_root)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(project_root)),
        config,
    });

    let ct = CancellationToken::new();

    // Spawn file watcher thread — keeps store + GraphCache fresh on disk changes.
    // If watcher fails to start, server runs without live file watching (non-fatal).
    let watcher_handle = match spawn_watcher(state.clone(), state.project_root.clone(), ct.clone())
    {
        Ok(h) => Some(h),
        Err(e) => {
            tracing::warn!(
                "File watcher failed to start: {e}. Server will run without live file watching."
            );
            None
        }
    };

    Ok((state, ct, watcher_handle))
}

/// Start the MCP server (streamable HTTP) on the given port with the given DB
/// path and project root.
pub async fn serve_async(port: u16, db_path: &str, project_root: &str) -> anyhow::Result<()> {
    let (state, ct, watcher_handle) = build_state_and_watcher(db_path, project_root)?;

    let service = StreamableHttpService::new(
        move || Ok(KernavaHandler::new(state.clone())),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_cancellation_token(ct.child_token())
            .with_allowed_hosts(vec![
                "localhost".to_string(),
                "127.0.0.1".to_string(),
                "0.0.0.0".to_string(),
                "::1".to_string(),
            ]),
    );

    let router = axum::Router::new().nest_service("/mcp", service);
    let addr = format!("0.0.0.0:{port}");
    info!("Kernava MCP server listening on {addr} (POST to http://localhost:{port}/mcp)");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            // Handle both SIGINT (Ctrl+C) and SIGTERM (Docker/systemd)
            let ctrl_c = tokio::signal::ctrl_c();

            #[cfg(unix)]
            let terminate = async {
                use tokio::signal::unix::{signal, SignalKind};
                if let Ok(mut s) = signal(SignalKind::terminate()) {
                    s.recv().await;
                } else {
                    std::future::pending::<()>().await;
                }
            };

            #[cfg(not(unix))]
            let terminate = std::future::pending::<()>();

            tokio::select! {
                _ = ctrl_c => info!("Received SIGINT, shutting down..."),
                _ = terminate => info!("Received SIGTERM, shutting down..."),
            }
            ct.cancel();
            // Wait for watcher thread to exit — it checks ct.is_cancelled()
            // after each 150ms drain cycle. Brief blocking join is fine:
            // server is already shutting down.
            if let Some(h) = watcher_handle {
                let _ = h.join();
            }
        })
        .await?;

    info!("Shutdown complete.");
    Ok(())
}

/// Start the MCP server over stdio (stdin/stdout JSON-RPC). This is the
/// transport MCP clients like jcode/Claude Code use when spawning the server
/// as a child process — no HTTP listener, no port. The server reads
/// JSON-RPC requests from stdin and writes responses to stdout; logging goes
/// to stderr so it doesn't corrupt the protocol stream.
///
/// Shuts down when the client closes stdin (EOF) or sends a transport
/// cancellation, then joins the watcher thread.
pub async fn serve_stdio(db_path: &str, project_root: &str) -> anyhow::Result<()> {
    let (state, ct, watcher_handle) = build_state_and_watcher(db_path, project_root)?;

    info!(
        "Kernava MCP server starting in stdio mode (db={}, project={})",
        db_path, project_root
    );

    let handler = KernavaHandler::new(state);
    // (tokio::io::stdin(), tokio::io::stdout()) is the canonical rmcp stdio
    // transport — jcode spawns this binary as a child and talks JSON-RPC over
    // the child's stdin/stdout. serve_server_with_ct performs the MCP
    // initialize handshake and returns once the transport closes (stdin EOF or
    // cancellation token fire).
    let transport = (tokio::io::stdin(), tokio::io::stdout());
    let running = rmcp::service::serve_server_with_ct(handler, transport, ct.clone())
        .await
        .map_err(|e| anyhow::anyhow!("MCP stdio initialize failed: {e}"))?;

    // Block until the client disconnects (stdin closes) or the cancellation
    // token fires. Either way the service task exits and waiting() resolves.
    let _ = running.waiting().await;

    ct.cancel();
    if let Some(h) = watcher_handle {
        let _ = h.join();
    }
    info!("Shutdown complete.");
    Ok(())
}

/// Index a project from CLI.
/// Runs on a dedicated thread with a 256 MiB stack — C/C++ preprocessor-heavy
/// headers produce tree-sitter ASTs hundreds of levels deep, overflowing the
/// default 8 MiB main-thread stack during recursive `walk()` in extractor.
/// ponytail: proper fix is converting walk() to an iterative work-stack.
/// Upgrade path: VecDeque<Node> loop instead of recursion in extractor.rs.
pub fn index_cmd(path: &str, db_path: &str) -> anyhow::Result<()> {
    let root = PathBuf::from(path);
    let config = load_config(path)?;

    let db_path = db_path.to_string();
    std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024) // 256 MiB
        .spawn(move || -> anyhow::Result<()> {
            let mut store = Store::open(&db_path)?;
            let results = kernava_indexer::builder::index_full_with_config(
                &mut store, &root, &config,
            )?;
            let files = results.len();
            let symbols: usize = results.iter().map(|r| r.symbols_inserted).sum();
            let resolved: usize = results.iter().map(|r| r.calls_resolved).sum();
            let unresolved: usize = results.iter().map(|r| r.calls_unresolved).sum();
            println!(
                "Indexed {files} files: {symbols} symbols, {resolved} resolved, {unresolved} unresolved."
            );
            Ok(())
        })?
        .join()
        .expect("index thread panicked")?;

    Ok(())
}

/// Print index statistics from CLI.
pub fn stats_cmd(db_path: &str) -> anyhow::Result<()> {
    let store = Store::open(db_path)?;
    let stats = store.stats()?;
    println!("Files: {}", stats.file_count);
    println!("Symbols: {}", stats.node_count);
    println!("Edges: {}", stats.edge_count);
    println!("Import edges: {}", stats.import_edge_count);
    println!("Indexed at: {}", stats.indexed_at.unwrap_or_default());
    println!(
        "Schema version: {}",
        stats.schema_version.unwrap_or_default()
    );
    if !stats.language_distribution.is_empty() {
        println!("Languages:");
        for (lang, count) in &stats.language_distribution {
            println!("  {lang}: {count} files");
        }
    }
    Ok(())
}

/// Run a single query tool from CLI.
pub fn query_cmd(
    tool: &str,
    db_path: &str,
    project_root: &str,
    args: &Option<String>,
) -> anyhow::Result<()> {
    let store = Store::open(db_path)?;
    let graph = GraphCache::new();
    let stats = store.stats()?;
    if stats.node_count > 0 {
        graph.load_from_store(&store)?;
    }
    let root = PathBuf::from(project_root)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(project_root));

    let config = Arc::new(load_config(project_root)?);
    let state = Arc::new(AppState {
        store: Mutex::new(store),
        graph,
        project_root: root,
        config,
    });
    let handler = KernavaHandler::new(state);

    let args_json: serde_json::Value = match args {
        Some(a) => serde_json::from_str(a)?,
        None => serde_json::json!({}),
    };

    match handler.query(tool, args_json) {
        Ok(result) => println!("{result}"),
        Err(e) => anyhow::bail!("{e}"),
    }
    Ok(())
}

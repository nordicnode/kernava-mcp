// kernava-server: library crate exposing MCP handler + server startup logic.

pub mod handler;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use handler::{AppState, KernavaHandler};
use kernava_graph::GraphCache;
use kernava_store::Store;
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use tokio_util::sync::CancellationToken;
use tracing::info;

/// Start the MCP server on the given port with the given DB path and project root.
pub async fn serve_async(port: u16, db_path: &str, project_root: &str) -> anyhow::Result<()> {
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

    let state = Arc::new(AppState {
        store: Mutex::new(store),
        graph,
        project_root: PathBuf::from(project_root)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(project_root)),
    });

    let ct = CancellationToken::new();
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
        })
        .await?;

    info!("Shutdown complete.");
    Ok(())
}

/// Index a project from CLI.
pub fn index_cmd(path: &str, db_path: &str) -> anyhow::Result<()> {
    let mut store = Store::open(db_path)?;
    let root = PathBuf::from(path);
    let results = kernava_indexer::builder::index_full(&mut store, &root)?;
    let files = results.len();
    let symbols: usize = results.iter().map(|r| r.symbols_inserted).sum();
    let resolved: usize = results.iter().map(|r| r.calls_resolved).sum();
    let unresolved: usize = results.iter().map(|r| r.calls_unresolved).sum();
    println!(
        "Indexed {files} files: {symbols} symbols, {resolved} resolved, {unresolved} unresolved."
    );
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

    let state = Arc::new(AppState {
        store: Mutex::new(store),
        graph,
        project_root: root,
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

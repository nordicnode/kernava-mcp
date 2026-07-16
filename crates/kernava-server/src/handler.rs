// kernava-server: MCP tool handler — tools-only server via #[tool_router(server_handler)]
//
// Architecture:
//   AppState is Arc-wrapped, shared across all MCP sessions.
//   Store is behind Mutex (single SQLite Connection — Send but not Sync).
//   GraphCache is DashMap-backed — natively Send + Sync, no lock needed.
//   The rmcp factory closure clones Arc<AppState> per session — cheap, zero-copy.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use kernava_graph::{detect_communities, get_call_path, get_impact_radius, GraphCache, NodeId};
use kernava_indexer::builder::index_full_with_config;
use kernava_store::Store;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::schemars;
use rmcp::tool;
use rmcp::tool_router;
use serde::Deserialize;

/// Deserialize an optional i32 from either a JSON integer or a numeric string.
/// Some MCP clients send integers as strings; without this, `call_line: Option<i32>`
/// fails with "invalid type: string \"96\", expected i32".
fn flexible_opt_i32<'de, D>(deserializer: D) -> Result<Option<i32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let opt: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    match opt {
        None => Ok(None),
        Some(serde_json::Value::Number(n)) => n
            .as_i64()
            .map(|v| Some(v as i32))
            .ok_or_else(|| D::Error::custom("expected integer")),
        Some(serde_json::Value::String(s)) => s
            .parse::<i32>()
            .map(Some)
            .map_err(|_| D::Error::custom(format!("expected integer, got string {s:?}"))),
        Some(other) => Err(D::Error::custom(format!("expected integer, got {other:?}"))),
    }
}
use tracing::info_span;

/// Shared server state, cloned via Arc into every session.
// ponytail: Mutex<Store> serializes all DB access. Upgrade path: connection pool
// (N rusqlite Connections behind a pool) for read parallelism. Fine for v1 —
// SQLite queries are sub-ms and MCP traffic is low-concurrency.
pub struct AppState {
    pub store: Mutex<Store>,
    pub graph: GraphCache,
    pub project_root: PathBuf,
    pub config: Arc<kernava_indexer::IndexerConfig>,
}

pub type SharedState = Arc<AppState>;

/// MCP handler. Clones SharedState — cheap Arc clone.
#[derive(Clone)]
pub struct KernavaHandler {
    state: SharedState,
}

impl KernavaHandler {
    pub fn new(state: SharedState) -> Self {
        Self { state }
    }

    /// Dispatch a query by tool name — for CLI `kernava query` subcommand.
    /// Reuses the same handler logic as MCP tool calls without the rmcp layer.
    pub fn query(&self, tool: &str, args: serde_json::Value) -> Result<String, String> {
        match tool {
            "search_symbols" => {
                let params: SearchSymbolsParams = serde_json::from_value(args)
                    .map_err(|e| e.to_string())?;
                self.search_symbols(Parameters(params))
            }
            "get_symbol" => {
                let params: GetSymbolParams = serde_json::from_value(args)
                    .map_err(|e| e.to_string())?;
                self.get_symbol(Parameters(params))
            }
            "get_file_outline" => {
                let params: GetFileOutlineParams = serde_json::from_value(args)
                    .map_err(|e| e.to_string())?;
                self.get_file_outline(Parameters(params))
            }
            "find_references" => {
                let params: FindReferencesParams = serde_json::from_value(args)
                    .map_err(|e| e.to_string())?;
                self.find_references(Parameters(params))
            }
            "get_callers" => {
                let params: GraphTraversalParams = serde_json::from_value(args)
                    .map_err(|e| e.to_string())?;
                self.get_callers(Parameters(params))
            }
            "get_callees" => {
                let params: GraphTraversalParams = serde_json::from_value(args)
                    .map_err(|e| e.to_string())?;
                self.get_callees(Parameters(params))
            }
            "search_code" => {
                let params: SearchCodeParams = serde_json::from_value(args)
                    .map_err(|e| e.to_string())?;
                self.search_code(Parameters(params))
            }
            "find_definition" => {
                let params: FindDefinitionParams = serde_json::from_value(args)
                    .map_err(|e| e.to_string())?;
                self.find_definition(Parameters(params))
            }
            "get_call_path" => {
                let params: CallPathParams = serde_json::from_value(args)
                    .map_err(|e| e.to_string())?;
                self.get_call_path(Parameters(params))
            }
            "get_impact_radius" => {
                let params: ImpactRadiusParams = serde_json::from_value(args)
                    .map_err(|e| e.to_string())?;
                self.get_impact_radius_tool(Parameters(params))
            }
            "detect_dead_code" => self.detect_dead_code(),
            "get_index_status" => self.get_index_status(),
            "index_project" => {
                let params: IndexProjectParams = serde_json::from_value(args)
                    .map_err(|e| e.to_string())?;
                self.index_project(Parameters(params))
            }
            "get_communities" => self.get_communities(),
            "get_architecture" => self.get_architecture(),
            "get_git_impact" => {
                let params: GitImpactParams = serde_json::from_value(args)
                    .map_err(|e| e.to_string())?;
                self.get_git_impact(Parameters(params))
            }
            _ => Err(format!(
                "unknown tool: {tool}\navailable: index_project, get_index_status, search_symbols, get_symbol, get_file_outline, find_references, find_definition, search_code, get_callers, get_callees, get_call_path, get_impact_radius, detect_dead_code, get_communities, get_architecture, get_git_impact"
            )),
        }
    }
}

/// Resolve a user-supplied relative path to the canonical absolute path
/// the store uses. Joins with project_root, then canonicalizes.
pub fn resolve_path(state: &AppState, input: &str) -> String {
    let p = PathBuf::from(input);
    let joined = if p.is_absolute() {
        p
    } else {
        state.project_root.join(p)
    };
    joined
        .canonicalize()
        .unwrap_or(joined)
        .to_string_lossy()
        .into_owned()
}

/// Resolve a qualified name that may use a relative file prefix.
/// E.g. "math.ts.add" → "{canonical_project_root}/math.ts.add"
fn resolve_qname(state: &AppState, input: &str) -> String {
    let root = state
        .project_root
        .canonicalize()
        .unwrap_or_else(|_| state.project_root.clone());
    let root_str = root.to_string_lossy();
    if input.starts_with(&*root_str) {
        return input.to_string();
    }
    format!("{root_str}/{input}")
}

// ── Tool parameter types ──────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IndexProjectParams {
    /// Absolute or relative path to the project root to index.
    pub project_root: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchSymbolsParams {
    /// Symbol name or fragment to search for (e.g. "handleRequest", "add", "process").
    pub query: String,
    /// Maximum number of results to return. Default 20.
    #[serde(default = "default_limit")]
    pub limit: u32,
}

fn default_limit() -> u32 {
    20
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetSymbolParams {
    /// Fully qualified name of the symbol (e.g. "src/math.ts.add").
    pub qualified_name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetFileOutlineParams {
    /// File path relative to project root (e.g. "src/math.ts").
    pub file_path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindReferencesParams {
    /// Qualified name of the symbol to find references to.
    pub qualified_name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindDefinitionParams {
    /// Qualified name of the caller symbol that makes the call.
    pub caller_qualified_name: String,
    /// Optional line number of the call site. If omitted, returns all
    /// outbound definitions from the caller. Accepts both integer and
    /// numeric string (some MCP clients send integers as strings).
    #[serde(default, deserialize_with = "flexible_opt_i32")]
    #[schemars(with = "Option<i32>")]
    pub call_line: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchCodeParams {
    /// Regex pattern to search for in file contents.
    pub pattern: String,
    /// File path glob to limit search (e.g. "*.ts"). If omitted, searches all indexed files.
    pub file_glob: Option<String>,
    /// Maximum number of match results. Default 50.
    #[serde(default = "default_code_limit")]
    pub limit: u32,
}

fn default_code_limit() -> u32 {
    50
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GraphTraversalParams {
    /// Qualified name of the source symbol.
    pub source: String,
    /// Maximum traversal depth for get_callers/get_callees (1 = direct only).
    /// Also used as max hops for get_call_path. Default 20.
    #[serde(default = "default_depth")]
    pub max_depth: u32,
}

fn default_depth() -> u32 {
    20
}

/// Default depth for impact_radius — capped lower than traversal default
/// because impact radius grows exponentially (all callers-of-callers).
fn default_impact_depth() -> u32 {
    10
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ImpactRadiusParams {
    /// Qualified name of the source symbol.
    pub source: String,
    /// Maximum traversal depth. Default 10 (impact radius grows exponentially).
    #[serde(default = "default_impact_depth")]
    pub max_depth: u32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CallPathParams {
    /// Qualified name of the source symbol (caller side).
    pub source: String,
    /// Qualified name of the target symbol (callee side).
    pub target: String,
    /// Maximum path length. Default 20.
    #[serde(default = "default_depth")]
    pub max_depth: u32,
}

// ── Tool implementations ─────────────────────────────────

#[tool_router(server_handler)]
impl KernavaHandler {
    // ── Phase 3 Tools ──────────────────────────────────────

    /// Index a project: parse all source files, extract symbols, resolve calls,
    /// populate SQLite + warm the in-RAM graph cache.
    /// Returns file count, symbol count, and resolved call count.
    #[tool(
        name = "index_project",
        description = "Index a project: parse all source files, extract symbols and call graph, populate SQLite + warm the in-RAM graph cache. Returns index statistics."
    )]
    fn index_project(
        &self,
        Parameters(params): Parameters<IndexProjectParams>,
    ) -> Result<String, String> {
        let _span = info_span!("mcp_tool", name = "index_project").entered();
        let root = PathBuf::from(&params.project_root);
        let mut store = self.state.store.lock().map_err(|e| e.to_string())?;
        let results = index_full_with_config(&mut store, &root, &self.state.config)
            .map_err(|e| e.to_string())?;

        // Record when this index run completed — get_index_status reports this.
        use std::time::{SystemTime, UNIX_EPOCH};
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = store.set_meta("indexed_at", &format!("epoch:{ts}"));

        self.state
            .graph
            .load_from_store(&store)
            .map_err(|e| e.to_string())?;

        let files = results.len();
        let symbols: usize = results.iter().map(|r| r.symbols_inserted).sum();
        let resolved: usize = results.iter().map(|r| r.calls_resolved).sum();
        let unresolved: usize = results.iter().map(|r| r.calls_unresolved).sum();

        Ok(format!(
            "Indexed {files} files: {symbols} symbols, {resolved} resolved calls, {unresolved} unresolved calls."
        ))
    }

    /// Get current index statistics: file count, symbol count, edge count,
    /// language distribution.
    #[tool(
        name = "get_index_status",
        description = "Get current index statistics: file count, symbol count, edge count, resolved calls, and language distribution."
    )]
    fn get_index_status(&self) -> Result<String, String> {
        let _span = info_span!("mcp_tool", name = "get_index_status").entered();
        let store = self.state.store.lock().map_err(|e| e.to_string())?;
        let stats = store.stats().map_err(|e| e.to_string())?;
        let langs = stats
            .language_distribution
            .iter()
            .map(|(lang, count)| format!("  {lang}: {count} files"))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(format!(
            "Files: {}\nSymbols: {}\nEdges: {}\nImport edges: {}\nIndexed at: {}\nLanguages:\n{}",
            stats.file_count,
            stats.node_count,
            stats.edge_count,
            stats.import_edge_count,
            stats.indexed_at.unwrap_or_default(),
            if langs.is_empty() {
                "  (none)".into()
            } else {
                langs
            }
        ))
    }

    /// Search for symbols by name using full-text search (FTS5).
    #[tool(
        name = "search_symbols",
        description = "Search for symbols by name using full-text search. Matches camelCase, snake_case, and PascalCase tokens. Returns matching symbols with qualified name, kind, and location."
    )]
    fn search_symbols(
        &self,
        Parameters(params): Parameters<SearchSymbolsParams>,
    ) -> Result<String, String> {
        let _span = info_span!("mcp_tool", name = "search_symbols").entered();
        let query = params.query.trim();
        if query.is_empty() {
            return Ok("Query is empty. Provide a symbol name or fragment to search.".into());
        }
        let store = self.state.store.lock().map_err(|e| e.to_string())?;
        let nodes = kernava_store::fts5::search_symbols(store.conn(), query, params.limit as i64)
            .map_err(|e| e.to_string())?;
        if nodes.is_empty() {
            return Ok("No symbols found.".into());
        }
        let lines: Vec<String> = nodes
            .iter()
            .map(|n| format!("  {} {} (line {})", n.kind, n.qualified_name, n.line_start))
            .collect();
        Ok(format!(
            "Found {} symbols:\n{}",
            nodes.len(),
            lines.join("\n")
        ))
    }

    /// Get full metadata for a single symbol by its qualified name.
    #[tool(
        name = "get_symbol",
        description = "Get full metadata for a symbol by its qualified name: kind, signature, return type, location, complexity, export status, caller count, callee count."
    )]
    fn get_symbol(
        &self,
        Parameters(params): Parameters<GetSymbolParams>,
    ) -> Result<String, String> {
        let _span = info_span!("mcp_tool", name = "get_symbol").entered();
        let qname = resolve_qname(&self.state, &params.qualified_name);
        let node = self
            .state
            .graph
            .get_node(&qname)
            .or_else(|| self.state.graph.get_node(&params.qualified_name));
        match node {
            Some(n) => {
                // Count callers/callees from graph adjacency
                let callers = self
                    .state
                    .graph
                    .reverse
                    .get(&n.id)
                    .map(|v| v.len())
                    .unwrap_or(0);
                let callees = self
                    .state
                    .graph
                    .forward
                    .get(&n.id)
                    .map(|v| v.len())
                    .unwrap_or(0);
                let store = self.state.store.lock().map_err(|e| e.to_string())?;
                let file_path = store
                    .get_file_path(n.file_id)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| format!("file_id:{}", n.file_id));
                Ok(format!(
                    "Name: {}\nKind: {}\nQualified: {}\nFile: {}\nLines: {}-{}\nSignature: {}\nReturn type: {}\nExported: {}\nComplexity: {}\nCallers: {}\nCallees: {}",
                    n.name, n.kind, n.qualified_name, file_path,
                    n.line_start, n.line_end,
                    n.signature.unwrap_or_default(),
                    n.return_type.unwrap_or_default(),
                    n.is_exported, n.complexity,
                    callers, callees,
                ))
            }
            None => Ok(format!("Symbol '{}' not found.", params.qualified_name)),
        }
    }

    /// Get all symbols defined in a file, sorted by line number.
    #[tool(
        name = "get_file_outline",
        description = "Get all symbols defined in a file, sorted by line number. Returns the file's symbol outline."
    )]
    fn get_file_outline(
        &self,
        Parameters(params): Parameters<GetFileOutlineParams>,
    ) -> Result<String, String> {
        let _span = info_span!("mcp_tool", name = "get_file_outline").entered();
        let store = self.state.store.lock().map_err(|e| e.to_string())?;
        let abs_path = resolve_path(&self.state, &params.file_path);
        let file_id = store.get_file_id(&abs_path).map_err(|e| e.to_string())?;
        match file_id {
            Some(fid) => {
                let mut nodes = store.get_nodes_for_file(fid).map_err(|e| e.to_string())?;
                nodes.sort_by_key(|n| n.line_start);
                if nodes.is_empty() {
                    return Ok("No symbols in file.".into());
                }
                let lines: Vec<String> = nodes
                    .iter()
                    .map(|n| format!("  L{:4} {} {}", n.line_start, n.kind, n.name))
                    .collect();
                Ok(format!("{} symbols:\n{}", nodes.len(), lines.join("\n")))
            }
            None => Ok(format!("File '{}' not in index.", params.file_path)),
        }
    }

    /// Find all references to a symbol — every call site and reference across the codebase.
    #[tool(
        name = "find_references",
        description = "Find all references to a symbol: every call site and reference across the codebase with file, line, and calling symbol."
    )]
    fn find_references(
        &self,
        Parameters(params): Parameters<FindReferencesParams>,
    ) -> Result<String, String> {
        let _span = info_span!("mcp_tool", name = "find_references").entered();
        let store = self.state.store.lock().map_err(|e| e.to_string())?;
        let qname = resolve_qname(&self.state, &params.qualified_name);

        // Look up the target node ID via graph cache
        let node = self
            .state
            .graph
            .get_node(&qname)
            .or_else(|| self.state.graph.get_node(&params.qualified_name));
        let node = match node {
            Some(n) => n,
            None => return Ok(format!("Symbol '{}' not found.", params.qualified_name)),
        };

        // Query store for ALL incoming edges (both "calls" and "references")
        let edges = store
            .get_incoming_edges(node.id)
            .map_err(|e| e.to_string())?;
        if edges.is_empty() {
            return Ok(format!(
                "No references found for '{}'.",
                params.qualified_name
            ));
        }

        let mut lines = Vec::with_capacity(edges.len());
        for e in &edges {
            // Look up the calling symbol's name
            let caller = store.get_node(e.source_id).map_err(|e| e.to_string())?;
            let caller_name = caller
                .as_ref()
                .map(|c| c.qualified_name.clone())
                .unwrap_or_else(|| format!("node#{}", e.source_id));
            // Look up the file path
            let file = e
                .file_id
                .and_then(|fid| store.get_file_path(fid).ok().flatten())
                .unwrap_or_default();
            let line = e.line.unwrap_or(0);
            let edge_type = &e.edge_type;
            let conf = e.confidence;
            lines.push(format!(
                "  {edge_type} from {caller_name} at {file}:{line} (confidence {conf:.2})"
            ));
        }
        Ok(format!(
            "Found {} references to '{}':\n{}",
            edges.len(),
            params.qualified_name,
            lines.join("\n")
        ))
    }

    /// Find the definition of a symbol called from a given call site.
    /// Uses the resolved edge target_id from index time.
    #[tool(
        name = "find_definition",
        description = "Find the definition of a symbol called from a given call site. Takes the caller's qualified name and optional call line. Returns the definition node metadata, or 'unresolved' if the call was not resolved at index time."
    )]
    fn find_definition(
        &self,
        Parameters(params): Parameters<FindDefinitionParams>,
    ) -> Result<String, String> {
        let _span = info_span!("mcp_tool", name = "find_definition").entered();
        let store = self.state.store.lock().map_err(|e| e.to_string())?;
        let qname = resolve_qname(&self.state, &params.caller_qualified_name);

        // Look up the caller node
        let node = self
            .state
            .graph
            .get_node(&qname)
            .or_else(|| self.state.graph.get_node(&params.caller_qualified_name));
        let node = match node {
            Some(n) => n,
            None => {
                return Ok(format!(
                    "Caller symbol '{}' not found.",
                    params.caller_qualified_name
                ))
            }
        };

        // Get all outgoing edges from the caller
        let edges = store
            .get_outgoing_edges(node.id)
            .map_err(|e| e.to_string())?;

        // Filter by call_line if provided
        let matching: Vec<_> = edges
            .iter()
            .filter(|e| {
                e.edge_type == "calls" && params.call_line.is_none_or(|line| e.line == Some(line))
            })
            .collect();

        if matching.is_empty() {
            return Ok(format!(
                "No outgoing calls from '{}'{}.",
                params.caller_qualified_name,
                params
                    .call_line
                    .map(|l| format!(" at line {l}"))
                    .unwrap_or_default()
            ));
        }

        let mut lines = Vec::with_capacity(matching.len());
        for e in matching {
            match e.target_id {
                Some(target_id) => {
                    let target = store.get_node(target_id).map_err(|e| e.to_string())?;
                    match target {
                        Some(t) => lines.push(format!(
                            "  → {} {} (line {}) [confidence {:.2}]",
                            t.kind, t.qualified_name, t.line_start, e.confidence
                        )),
                        None => lines.push(format!(
                            "  → node#{target_id} (metadata missing) [confidence {:.2}]",
                            e.confidence
                        )),
                    }
                }
                None => lines.push(format!(
                    "  → unresolved call at line {} [confidence {:.2}]",
                    e.line.unwrap_or(0),
                    e.confidence
                )),
            }
        }
        Ok(format!(
            "Definition(s) from '{}':\n{}",
            params.caller_qualified_name,
            lines.join("\n")
        ))
    }

    /// Search file contents by regex pattern. Searches all indexed files.
    #[tool(
        name = "search_code",
        description = "Search file contents by regex pattern across all indexed files. Returns matching lines with file, line number, and containing symbol."
    )]
    fn search_code(
        &self,
        Parameters(params): Parameters<SearchCodeParams>,
    ) -> Result<String, String> {
        let _span = info_span!("mcp_tool", name = "search_code").entered();
        let re = regex::Regex::new(&params.pattern).map_err(|e| e.to_string())?;

        // Phase 1: fetch file list under lock, then drop lock for disk I/O
        let file_paths: Vec<String> = {
            let store = self.state.store.lock().map_err(|e| e.to_string())?;
            let mut stmt = store
                .conn()
                .prepare("SELECT path FROM files ORDER BY path")
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|e| e.to_string())?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?
        };
        // Lock dropped — disk I/O doesn't block other MCP tool calls

        // ponytail: suffix match handling common glob patterns without a glob crate.
        // Handles: "*.rs"→".rs", "**/*.rs"→".rs", "src/**/*.rs"→".rs",
        // "cache.rs"→exact suffix, None→match all.
        // Upgrade path: use `globset` crate for full recursive pattern matching.
        let file_filter = |path: &str| -> bool {
            match &params.file_glob {
                Some(g) => {
                    // Strip leading **/ prefixes
                    let stripped = g.trim_start_matches("**/");
                    // If glob contains **/ after first segment (e.g. "src/**/*.rs"),
                    // match the suffix part after **
                    if let Some(idx) = stripped.find("**/") {
                        let suffix = stripped[idx + 3..].trim_start_matches('*');
                        return path.ends_with(suffix);
                    }
                    // Strip leading * chars (e.g. "*.rs" → ".rs")
                    let filter = stripped.trim_start_matches('*');
                    path.ends_with(filter)
                }
                None => true,
            }
        };

        let limit = params.limit as usize;
        let mut matches: Vec<(String, usize, String)> = Vec::new();
        for path in &file_paths {
            if !file_filter(path) {
                continue;
            }
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            for (i, line) in content.lines().enumerate() {
                if re.is_match(line) {
                    matches.push((path.clone(), i + 1, line.trim().to_string()));
                    if matches.len() >= limit {
                        break;
                    }
                }
            }
            if matches.len() >= limit {
                break;
            }
        }

        if matches.is_empty() {
            return Ok("No matches found.".into());
        }

        // Phase 2: re-acquire lock to resolve containing symbols
        let store = self.state.store.lock().map_err(|e| e.to_string())?;
        let mut lines = Vec::with_capacity(matches.len());
        for (path, line_num, line_text) in &matches {
            let symbol = store
                .get_file_id(path)
                .ok()
                .flatten()
                .and_then(|fid| {
                    store.get_nodes_for_file(fid).ok().and_then(|nodes| {
                        nodes
                            .into_iter()
                            .find(|n| {
                                *line_num as i32 >= n.line_start && *line_num as i32 <= n.line_end
                            })
                            .map(|n| n.name)
                    })
                })
                .unwrap_or_default();
            lines.push(format!("  {path}:{line_num} [{symbol}] {line_text}"));
        }
        Ok(format!(
            "Found {} matches:\n{}",
            matches.len(),
            lines.join("\n")
        ))
    }

    // ── Phase 4 Graph Tools ─────────────────────────────────

    /// Get all direct and transitive callers of a symbol up to `max_depth` hops.
    /// Depth 1 = direct callers only (default). Depth N includes callers-of-callers up to N hops.
    #[tool(
        name = "get_callers",
        description = "Get all callers of a symbol — reverse adjacency with call-site file, line, and confidence. Supports multi-hop traversal via max_depth (default 1 = direct callers only)."
    )]
    fn get_callers(
        &self,
        Parameters(params): Parameters<GraphTraversalParams>,
    ) -> Result<String, String> {
        let _span = info_span!("mcp_tool", name = "get_callers").entered();
        let store = self.state.store.lock().map_err(|e| e.to_string())?;
        let qname = resolve_qname(&self.state, &params.source);
        let node = self
            .state
            .graph
            .get_node(&qname)
            .or_else(|| self.state.graph.get_node(&params.source));
        let node = match node {
            Some(n) => n,
            None => return Ok(format!("Symbol '{}' not found.", params.source)),
        };

        let max_depth = if params.max_depth == 0 {
            1
        } else {
            params.max_depth as usize
        };

        // BFS over reverse adjacency in the graph cache.
        use std::collections::HashSet;
        let mut visited: HashSet<NodeId> = HashSet::new();
        visited.insert(node.id);
        let mut results: Vec<(NodeId, usize, f64)> = Vec::new();
        let mut frontier: Vec<(NodeId, usize, f64)> = vec![(node.id, 0, 1.0)];
        while let Some((nid, depth, conf)) = frontier.pop() {
            if depth >= max_depth {
                continue;
            }
            for (caller_id, edge_conf) in self.state.graph.get_callers(nid) {
                if visited.contains(&caller_id) {
                    continue;
                }
                visited.insert(caller_id);
                let combined = conf * edge_conf;
                results.push((caller_id, depth + 1, combined));
                frontier.push((caller_id, depth + 1, combined));
            }
        }

        if results.is_empty() {
            return Ok(format!("No callers found for '{}'.", params.source));
        }

        // Query incoming edges ONCE and build a lookup map for depth-1 callers.
        // Previously this re-queried for each caller — O(n²) store round-trips.
        use std::collections::HashMap;
        let edge_map: HashMap<NodeId, (String, i32)> = store
            .get_incoming_edges(node.id)
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|e| e.edge_type == "calls")
            .map(|e| {
                let file = e
                    .file_id
                    .and_then(|fid| store.get_file_path(fid).ok().flatten())
                    .unwrap_or_default();
                let line = e.line.unwrap_or(0);
                (e.source_id, (file, line))
            })
            .collect();

        results.sort_by_key(|r| r.1);
        let count = results.len();
        let mut lines = Vec::with_capacity(count);
        for (caller_id, depth, conf) in &results {
            let caller_name = self
                .state
                .graph
                .nodes
                .get(caller_id)
                .map(|n| n.qualified_name.clone())
                .unwrap_or_else(|| format!("node#{caller_id}"));
            if *depth == 1 {
                // Direct caller — use pre-built edge map for file/line.
                let (file, line) = edge_map.get(caller_id).cloned().unwrap_or_default();
                lines.push(format!(
                    "  {caller_name} → {qname} at {file}:{line} (confidence {:.2})",
                    conf
                ));
            } else {
                lines.push(format!(
                    "  {caller_name} → (depth {depth}, confidence {:.2})",
                    conf
                ));
            }
        }
        Ok(format!(
            "Found {count} callers of '{}':\n{}",
            params.source,
            lines.join("\n")
        ))
    }

    /// Get all direct and transitive callees of a symbol up to `max_depth` hops.
    /// Depth 1 = direct callees only (default). Depth N includes callees-of-callees up to N hops.
    #[tool(
        name = "get_callees",
        description = "Get all callees of a symbol — forward adjacency with call-site file, line, and confidence. Supports multi-hop traversal via max_depth (default 1 = direct callees only)."
    )]
    fn get_callees(
        &self,
        Parameters(params): Parameters<GraphTraversalParams>,
    ) -> Result<String, String> {
        let _span = info_span!("mcp_tool", name = "get_callees").entered();
        let store = self.state.store.lock().map_err(|e| e.to_string())?;
        let qname = resolve_qname(&self.state, &params.source);
        let node = self
            .state
            .graph
            .get_node(&qname)
            .or_else(|| self.state.graph.get_node(&params.source));
        let node = match node {
            Some(n) => n,
            None => return Ok(format!("Symbol '{}' not found.", params.source)),
        };

        let max_depth = if params.max_depth == 0 {
            1
        } else {
            params.max_depth as usize
        };

        // BFS over forward adjacency in the graph cache.
        use std::collections::HashSet;
        let mut visited: HashSet<NodeId> = HashSet::new();
        visited.insert(node.id);
        let mut results: Vec<(NodeId, usize, f64)> = Vec::new();
        let mut frontier: Vec<(NodeId, usize, f64)> = vec![(node.id, 0, 1.0)];
        while let Some((nid, depth, conf)) = frontier.pop() {
            if depth >= max_depth {
                continue;
            }
            for (callee_id, edge_conf) in self.state.graph.get_callees(nid) {
                if visited.contains(&callee_id) {
                    continue;
                }
                visited.insert(callee_id);
                let combined = conf * edge_conf;
                results.push((callee_id, depth + 1, combined));
                frontier.push((callee_id, depth + 1, combined));
            }
        }

        if results.is_empty() {
            return Ok(format!("No callees found for '{}'.", params.source));
        }

        // Query outgoing edges ONCE and build a lookup map for depth-1 callees.
        // Previously this re-queried for each callee — O(n²) store round-trips.
        use std::collections::HashMap;
        let edge_map: HashMap<NodeId, (String, i32)> = store
            .get_outgoing_edges(node.id)
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|e| e.edge_type == "calls")
            .filter_map(|e| {
                let target_id = e.target_id?;
                let file = e
                    .file_id
                    .and_then(|fid| store.get_file_path(fid).ok().flatten())
                    .unwrap_or_default();
                let line = e.line.unwrap_or(0);
                Some((target_id, (file, line)))
            })
            .collect();

        results.sort_by_key(|r| r.1);
        let count = results.len();
        let mut lines = Vec::with_capacity(count);
        for (callee_id, depth, conf) in &results {
            let callee_name = match self.state.graph.nodes.get(callee_id) {
                Some(n) => n.qualified_name.clone(),
                None => format!("node#{callee_id}"),
            };
            if *depth == 1 {
                // Direct callee — use pre-built edge map for file/line.
                let (file, line) = edge_map.get(callee_id).cloned().unwrap_or_default();
                lines.push(format!(
                    "  {qname} → {callee_name} at {file}:{line} (confidence {:.2})",
                    conf
                ));
            } else {
                lines.push(format!(
                    "  → {callee_name} (depth {depth}, confidence {:.2})",
                    conf
                ));
            }
        }
        Ok(format!(
            "Found {count} callees of '{}':\n{}",
            params.source,
            lines.join("\n")
        ))
    }

    /// Find the shortest call path from source to target via BFS over call edges.
    #[tool(
        name = "get_call_path",
        description = "Find the shortest call path from source to target via BFS over resolved call edges. Only calls that were resolved at index time are traversed — calls to external libraries, stdlib, or unresolved methods are not included. Returns the ordered path with per-hop confidence, or 'no path' if unreachable."
    )]
    fn get_call_path(
        &self,
        Parameters(params): Parameters<CallPathParams>,
    ) -> Result<String, String> {
        let _span = info_span!("mcp_tool", name = "get_call_path").entered();
        let src_qname = resolve_qname(&self.state, &params.source);
        let tgt_qname = resolve_qname(&self.state, &params.target);

        let src = self
            .state
            .graph
            .get_node(&src_qname)
            .or_else(|| self.state.graph.get_node(&params.source));
        let tgt = self
            .state
            .graph
            .get_node(&tgt_qname)
            .or_else(|| self.state.graph.get_node(&params.target));

        let (src, tgt) = match (src, tgt) {
            (Some(s), Some(t)) => (s, t),
            (None, _) => return Ok(format!("Source '{}' not found.", params.source)),
            (_, None) => return Ok(format!("Target '{}' not found.", params.target)),
        };

        match get_call_path(&self.state.graph, src.id, tgt.id, params.max_depth as usize) {
            Some(path) if path.len() < 2 => {
                Ok("Source and target are the same symbol.".to_string())
            }
            Some(path) => {
                let mut lines = Vec::with_capacity(path.len());
                for (i, hop) in path.iter().enumerate() {
                    let node = self.state.graph.nodes.get(&hop.node_id);
                    let name = node
                        .as_ref()
                        .map(|n| n.qualified_name.clone())
                        .unwrap_or_else(|| format!("node#{}", hop.node_id));
                    let prefix = if i == 0 {
                        "  ".to_string()
                    } else {
                        format!("  → (conf {:.2}) ", hop.confidence)
                    };
                    lines.push(format!("{prefix}{name}"));
                }
                Ok(format!(
                    "Path ({} hops):\n{}",
                    path.len() - 1,
                    lines.join("\n")
                ))
            }
            None => Ok(format!(
                "No path from '{}' to '{}' within {} hops.",
                params.source, params.target, params.max_depth
            )),
        }
    }

    /// Compute the impact radius — all transitively affected symbols via reverse BFS.
    #[tool(
        name = "get_impact_radius",
        description = "Compute the impact radius of a symbol: all transitively affected symbols (callers of callers, etc.) via reverse BFS, grouped by depth with risk scores."
    )]
    fn get_impact_radius_tool(
        &self,
        Parameters(params): Parameters<ImpactRadiusParams>,
    ) -> Result<String, String> {
        let _span = info_span!("mcp_tool", name = "get_impact_radius_tool").entered();
        let qname = resolve_qname(&self.state, &params.source);
        let node = self
            .state
            .graph
            .get_node(&qname)
            .or_else(|| self.state.graph.get_node(&params.source));
        let node = match node {
            Some(n) => n,
            None => return Ok(format!("Symbol '{}' not found.", params.source)),
        };

        let depth = if params.max_depth == 0 {
            10
        } else {
            params.max_depth as usize
        };
        let radius = get_impact_radius(&self.state.graph, node.id, depth);

        if radius.total == 0 {
            return Ok(format!("No transitive impact for '{}'.", params.source));
        }

        let mut lines = Vec::new();
        for (depth, entries) in radius.grouped_by_depth() {
            lines.push(format!("Depth {depth}:"));
            for entry in entries {
                let node = self.state.graph.nodes.get(&entry.node_id);
                let name = node
                    .as_ref()
                    .map(|n| n.qualified_name.clone())
                    .unwrap_or_else(|| format!("node#{}", entry.node_id));
                lines.push(format!(
                    "  {name} (risk {:.2}, confidence {:.2})",
                    entry.risk_score, entry.confidence
                ));
            }
        }
        Ok(format!(
            "Impact radius for '{}': {} affected symbols, max depth {}\n{}",
            params.source,
            radius.total,
            radius.max_depth,
            lines.join("\n")
        ))
    }

    /// Detect dead code: symbols with zero incoming call edges, excluding entry points.
    #[tool(
        name = "detect_dead_code",
        description = "Detect dead code: functions with zero incoming call edges, excluding exported symbols, known entry points (main, test functions), and trait method implementations (e.g. Default::default) that are called via trait resolution the call graph cannot track."
    )]
    fn detect_dead_code(&self) -> Result<String, String> {
        let _span = info_span!("mcp_tool", name = "detect_dead_code").entered();
        // Use the in-RAM graph cache instead of SQL round-trips.
        // A node is "called" if it appears as a key in the reverse adjacency map.
        // ponytail: this misses non-calls edges (references), but detect_dead_code
        // is about call graph dead code — references edges don't make code alive.
        let graph = &self.state.graph;

        // Collect all called node IDs from reverse adjacency
        let called: HashSet<NodeId> = graph.reverse.iter().map(|entry| *entry.key()).collect();

        // Iterate all nodes in the graph, find dead ones
        let dead: Vec<_> = graph
            .nodes
            .iter()
            .filter(|entry| {
                let n = entry.value();
                (n.kind == "function" || n.kind == "method")
                    && !called.contains(&n.id)
                    && !n.is_exported
                    && n.name != "main"
                    && !n.name.starts_with("test_")
                    && !n.name.ends_with("_test")
                    && !n.name.starts_with("Test")
                    // PHP/Python dunder methods (__construct, __toString, __init__) are
                    // invoked by the runtime via protocol hooks, not direct calls.
                    && !n.name.starts_with("__")
                    // Ruby constructors (initialize) invoked by Class.new at runtime.
                    && n.name != "initialize"
                    // Java/C++/C# constructors: method named same as enclosing class.
                    // Qualified name format: "path/to/File.ext.ClassName.ClassName"
                    // so a constructor has last two '.' segments identical.
                    && !is_constructor(&n.qualified_name)
                    // Default trait impls (Default::default) are called via trait
                    // resolution which the call graph can't track. Skip to avoid
                    // false positives — these are always reachable at runtime.
                    && n.name != "default"
                    // Serde default functions (#[serde(default = "fn_name")]) are
                    // invoked via deserialization, not direct calls. All start with
                    // "default_" by our own convention.
                    && !n.name.starts_with("default_")
                    // Benchmark harness functions are invoked by the benchmark
                    // runner, not by project code — would be false positives.
                    && !n.name.starts_with("bench_")
                    // From/Into/TryFrom trait impls called via conversion syntax
                    // which the call graph can't track.
                    && n.name != "from"
                    && n.name != "into"
                    && n.name != "try_into"
            })
            .map(|entry| entry.value().clone())
            .collect();

        if dead.is_empty() {
            return Ok("No dead code detected.".into());
        }

        let count = dead.len();
        let store = self.state.store.lock().map_err(|e| e.to_string())?;
        let mut lines = Vec::with_capacity(count);
        for n in &dead {
            let file = store
                .get_file_path(n.file_id)
                .ok()
                .flatten()
                .unwrap_or_default();
            lines.push(format!(
                "  {} {} at {}:{}",
                n.kind, n.qualified_name, file, n.line_start
            ));
        }
        Ok(format!("Found {count} dead symbols:\n{}", lines.join("\n")))
    }

    // ── Phase 4 Tools ──────────────────────────────────────

    /// Detect communities in the call graph using Louvain modularity optimization.
    /// Returns communities sorted by size, with member symbols, representative,
    /// internal/external edge counts.
    #[tool(
        name = "get_communities",
        description = "Detect communities in the call graph using Louvain modularity optimization over symmetrized call edges. Returns communities sorted by size with member symbols, representative (most-connected), internal/external edge weights."
    )]
    fn get_communities(&self) -> Result<String, String> {
        let _span = info_span!("mcp_tool", name = "get_communities").entered();
        let communities = detect_communities(&self.state.graph);

        if communities.is_empty() {
            return Ok("No communities detected — graph is empty or has no edges.".into());
        }

        let store = self.state.store.lock().map_err(|e| e.to_string())?;
        // Cap output: skip singleton communities (noise) and limit to top 50
        // multi-member communities to prevent massive MCP response payloads.
        const MAX_COMMUNITIES: usize = 50;
        let mut lines = Vec::new();
        let mut shown = 0;
        for (i, c) in communities.iter().enumerate() {
            if c.members.len() < 2 {
                continue;
            }
            if shown >= MAX_COMMUNITIES {
                break;
            }
            shown += 1;
            let rep_name = store
                .get_node(c.representative)
                .ok()
                .flatten()
                .map(|n| n.qualified_name.clone())
                .unwrap_or_else(|| format!("node#{}", c.representative));

            let member_names: Vec<String> = c
                .members
                .iter()
                .filter_map(|&nid| {
                    store
                        .get_node(nid)
                        .ok()
                        .flatten()
                        .map(|n| n.qualified_name.clone())
                })
                .collect();

            lines.push(format!(
                "  Community {} ({} members, rep: {}): internal={:.1}, external={:.1}\n    {}",
                i + 1,
                c.members.len(),
                rep_name,
                c.internal_edges,
                c.external_edges,
                member_names.join(", ")
            ));
        }
        let total = communities.len();
        let multi = communities.iter().filter(|c| c.members.len() >= 2).count();
        let singletons = total - multi;
        let truncated = if shown < multi {
            format!(", showing {shown} of {multi}")
        } else {
            String::new()
        };
        let summary = format!(
            "Found {total} communities ({multi} multi-member, {singletons} singletons{truncated}):"
        );
        Ok(format!("{summary}\n{}", lines.join("\n")))
    }

    /// Get project architecture summary: languages, file structure, entry points,
    /// hub functions (top by in-degree), and communities.
    #[tool(
        name = "get_architecture",
        description = "Get project architecture summary: language distribution, module structure (files grouped by top-level directory), entry points (exported symbols + main), hub functions (top 10 by incoming call count), and detected communities."
    )]
    fn get_architecture(&self) -> Result<String, String> {
        let _span = info_span!("mcp_tool", name = "get_architecture").entered();
        let store = self.state.store.lock().map_err(|e| e.to_string())?;
        let stats = store.stats().map_err(|e| e.to_string())?;

        // 1. Language distribution
        let lang_lines: Vec<String> = stats
            .language_distribution
            .iter()
            .map(|(lang, count)| format!("  {lang}: {count} files"))
            .collect();

        // 2. Module structure: group files by top-level dir under project_root
        let all_nodes = store.get_all_nodes().map_err(|e| e.to_string())?;
        let dir_counts = group_files_by_dir(&store, &all_nodes, &self.state.project_root);
        let mut dir_lines: Vec<String> = dir_counts
            .into_iter()
            .map(|(dir, count)| format!("  {dir}/: {count} files"))
            .collect();
        dir_lines.sort();

        // 3. Entry points: functions named "main", plus exported functions and
        // methods. Structs/enums/types are data declarations, not entry points.
        let is_entry = |n: &kernava_store::NodeRow| {
            n.name == "main"
                || (n.is_exported
                    && n.kind == "function"
                    && !n.name.starts_with("test_")
                    && !n.name.starts_with("bench_"))
        };
        let entry_count = all_nodes.iter().filter(|n| is_entry(n)).count();
        const MAX_ENTRY_DISPLAY: usize = 20;
        let entry_points: Vec<String> = all_nodes
            .iter()
            .filter(|n| is_entry(n))
            .take(MAX_ENTRY_DISPLAY)
            .map(|n| {
                format!(
                    "  {} ({}) at line {}",
                    n.qualified_name, n.kind, n.line_start
                )
            })
            .collect();

        // 4. Hub functions: top 10 by in-degree (reverse adjacency size)
        let mut hub_list: Vec<(String, usize)> = self
            .state
            .graph
            .reverse
            .iter()
            .filter_map(|entry| {
                let callers = entry.value().len();
                let node = self.state.graph.nodes.get(entry.key())?;
                Some((node.qualified_name.clone(), callers))
            })
            .collect();
        hub_list.sort_by_key(|b| std::cmp::Reverse(b.1));
        let hub_lines: Vec<String> = hub_list
            .iter()
            .take(10)
            .map(|(name, callers)| format!("  {name}: {callers} callers"))
            .collect();

        // 5. Communities
        let communities = detect_communities(&self.state.graph);
        let comm_summary = if communities.is_empty() {
            "  (none — graph empty or no edges)".into()
        } else {
            let multi = communities.iter().filter(|c| c.members.len() >= 2).count();
            let singletons = communities.len() - multi;
            let largest = communities
                .iter()
                .map(|c| c.members.len())
                .max()
                .unwrap_or(0);
            format!(
                "  {} communities ({} multi-member, {} singletons, largest: {} members)",
                communities.len(),
                multi,
                singletons,
                largest
            )
        };

        let entry_truncated = if entry_count > MAX_ENTRY_DISPLAY {
            format!(" (showing {MAX_ENTRY_DISPLAY} of {entry_count})")
        } else {
            String::new()
        };

        Ok(format!(
            "Project Architecture ({} files, {} symbols, {} call edges)\n\n\
             Languages:\n{}\n\n\
             Module structure:\n{}\n\n\
             Entry points ({}):\n{}{}\n\n\
             Hub functions (top {}):\n{}\n\n\
             Communities:\n{}",
            stats.file_count,
            stats.node_count,
            stats.edge_count,
            lang_lines.join("\n"),
            dir_lines.join("\n"),
            entry_count,
            entry_points.join("\n"),
            entry_truncated,
            hub_list.len().min(10),
            hub_lines.join("\n"),
            comm_summary,
        ))
    }

    /// Analyze git diff impact: find changed files, identify affected symbols,
    /// run impact radius per symbol, classify risk HIGH/MEDIUM/LOW.
    #[tool(
        name = "get_git_impact",
        description = "Analyze git diff impact: runs `git diff --name-only`, finds symbols in changed files/line ranges, computes transitive impact radius, classifies risk as HIGH (many callers), MEDIUM (some), LOW (leaf)."
    )]
    fn get_git_impact(
        &self,
        Parameters(params): Parameters<GitImpactParams>,
    ) -> Result<String, String> {
        let _span = info_span!("mcp_tool", name = "get_git_impact").entered();
        // 1. Run git diff --name-only to get changed files
        let mut cmd = std::process::Command::new("git");
        cmd.arg("diff").arg("--name-only").arg("--no-color");
        if let Some(ref git_ref) = params.git_ref {
            cmd.arg(git_ref);
        }
        let output = cmd
            .current_dir(&self.state.project_root)
            .output()
            .map_err(|e| format!("Failed to run git diff: {e}"))?;

        if !output.status.success() {
            return Ok(format!(
                "git diff failed (exit {:?}). Is '{}' a git repo?",
                output.status.code(),
                self.state.project_root.display()
            ));
        }

        let changed_paths: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();

        if changed_paths.is_empty() {
            return Ok("No uncommitted changes detected by git diff.".into());
        }

        // 2. Find symbols in changed files
        let store = self.state.store.lock().map_err(|e| e.to_string())?;
        let mut affected_symbols: Vec<(kernava_store::NodeRow, String)> = Vec::new();

        for rel_path in &changed_paths {
            let abs_path = resolve_path(&self.state, rel_path);
            if let Ok(Some(fid)) = store.get_file_id(&abs_path) {
                if let Ok(nodes) = store.get_nodes_for_file(fid) {
                    for n in nodes {
                        affected_symbols.push((n.clone(), rel_path.clone()));
                    }
                }
            }
        }

        if affected_symbols.is_empty() {
            return Ok(format!(
                "Found {} changed files but no indexed symbols in them.\nChanged: {}",
                changed_paths.len(),
                changed_paths.join(", ")
            ));
        }

        // 3. Classify risk by impact radius
        let mut high = Vec::new();
        let mut medium = Vec::new();
        let mut low = Vec::new();

        for (n, file) in &affected_symbols {
            let radius = get_impact_radius(&self.state.graph, n.id, 5);
            let total = radius.total;
            let (tag, level) = classify_risk(total);
            let target = match level {
                RiskLevel::High => &mut high,
                RiskLevel::Medium => &mut medium,
                RiskLevel::Low => &mut low,
            };
            target.push(format!(
                "  {tag} {} ({file}:{}) — {total} transitive callers",
                n.qualified_name, n.line_start
            ));
        }

        let sections: Vec<String> = [&high, &medium, &low]
            .into_iter()
            .filter(|v| !v.is_empty())
            .map(|v| v.join("\n"))
            .collect();

        Ok(format!(
            "Git impact analysis: {} changed files, {} affected symbols\n\
             HIGH: {} | MEDIUM: {} | LOW: {}\n\n{}",
            changed_paths.len(),
            affected_symbols.len(),
            high.len(),
            medium.len(),
            low.len(),
            sections.join("\n\n"),
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskLevel {
    High,
    Medium,
    Low,
}

/// Classify risk by transitive caller count.
/// HIGH > 20, MEDIUM 5-20, LOW < 5 (per DEVELOPMENT_PLAN.md spec).
pub fn classify_risk(total: usize) -> (&'static str, RiskLevel) {
    if total > 20 {
        ("[HIGH]", RiskLevel::High)
    } else if total >= 5 {
        ("[MED]", RiskLevel::Medium)
    } else {
        ("[LOW]", RiskLevel::Low)
    }
}

/// Detect Java/C++/C# constructors: a method whose simple name matches its
/// enclosing class. Qualified name format is "path/to/File.ext.ClassName.method",
/// so a constructor has its last two '.'-separated segments identical.
/// Invoked by the runtime via instantiation (`new ClassName()`), not direct calls,
/// so they'd be false positives in the dead-code filter.
fn is_constructor(qualified_name: &str) -> bool {
    let parts: Vec<&str> = qualified_name.split('.').collect();
    parts.len() >= 2 && parts[parts.len() - 1] == parts[parts.len() - 2]
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GitImpactParams {
    /// Optional: specify a git ref to diff against (e.g. "HEAD~1", "main").
    /// Defaults to staged + unstaged working tree changes.
    pub git_ref: Option<String>,
}

/// Group indexed files by top-level directory under project_root.
/// Files directly under project_root bucket as "(root)".
/// Exposed for testability — takes all_nodes to avoid duplicate store query.
pub fn group_files_by_dir(
    store: &Store,
    all_nodes: &[kernava_store::NodeRow],
    project_root: &std::path::Path,
) -> std::collections::HashMap<String, usize> {
    let canonical_root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    let mut dir_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut file_ids: std::collections::HashSet<i64> = std::collections::HashSet::new();
    for n in all_nodes {
        file_ids.insert(n.file_id);
    }
    for fid in &file_ids {
        if let Ok(Some(path)) = store.get_file_path(*fid) {
            let p = std::path::Path::new(&path);
            // File directly under root → "(root)"; otherwise first path component.
            let top = if p.parent() == Some(canonical_root.as_path()) {
                "(root)".to_string()
            } else {
                let rel = p.strip_prefix(&canonical_root).unwrap_or(p);
                rel.components()
                    .next()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .unwrap_or_else(|| "(root)".to_string())
            };
            *dir_counts.entry(top).or_default() += 1;
        }
    }
    dir_counts
}

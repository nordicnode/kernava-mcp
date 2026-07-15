# Kernava MCP

A graph-backed coding intelligence MCP server built in Rust. Parses your codebase with tree-sitter, stores symbols and call relationships in SQLite-WAL, and serves a warm in-RAM call graph over streamable HTTP. Designed for token efficiency — answer "who calls this?", "what's the blast radius of this change?", and "where's the dead code?" without re-reading files.

## Why

Most coding MCP tools either re-read files on every query (expensive, token-heavy) or rely on LSP round-trips (slow, language-specific). Kernava builds the call graph once, keeps it hot in RAM, and answers structural questions in sub-millisecond — the MCP client never touches the source files.

## Architecture

```
Source files → tree-sitter (11 languages) → symbols + calls + imports
                                              ↓
                           SQLite-WAL (persistent, FTS5 full-text search)
                                              ↓
                           GraphCache (DashMap, in-RAM, shared across sessions)
                                              ↓
                           MCP server (streamable HTTP via rmcp)
```

- **Transport**: Streamable HTTP via [`rmcp`](https://crates.io/crates/rmcp). Long-lived process — the graph stays warm between MCP sessions. No stdio cold-start penalty.
- **Storage**: SQLite-WAL with FTS5. Proven at 28M LOC with sub-ms queries.
- **Graph**: Single global `GraphCache` (DashMap-backed) shared across all MCP sessions. Lock-free reads.
- **Parsing**: tree-sitter, 11 languages. Incremental re-indexing on file watch with content-hash dedup.

## Supported Languages

| Language | Symbol extraction | Call resolution | Import parsing |
|---|---|---|---|
| TypeScript / TSX | ✅ | ✅ High | ✅ |
| JavaScript / JSX | ✅ | ✅ Medium | ✅ (CommonJS + ESM) |
| Python | ✅ | ✅ High | ✅ |
| Rust | ✅ | ✅ Medium | ✅ |
| Go | ✅ | ✅ Medium | ✅ |
| Java | ✅ | ✅ High | ✅ |
| C# | ✅ | ✅ High | ✅ |
| Ruby | ✅ | ⚠ Medium | ✅ |
| PHP | ✅ | ⚠ Medium | ✅ |
| C | ✅ | ⚠ Medium | ✅ |
| C++ | ✅ | ⚠ Medium | ✅ |

Kotlin is deferred (tree-sitter-kotlin requires tree-sitter <0.23, incompatible with workspace 0.25).

## MCP Tools

| Tool | Description |
|---|---|
| `index_project` | Parse all source files, build symbol + call graph, populate SQLite + warm RAM cache |
| `get_index_status` | File count, symbol count, edge count, resolved calls, language distribution |
| `search_symbols` | FTS5 full-text symbol search (camelCase, snake_case, PascalCase tokenized) |
| `get_symbol` | Full metadata: kind, signature, return type, complexity, caller/callee counts |
| `get_file_outline` | All symbols in a file, sorted by line |
| `find_references` | Every call site across the codebase |
| `find_definition` | Definition of a symbol called from a given call site |
| `search_code` | Regex search across all indexed file contents |
| `get_callers` | Direct callers (reverse adjacency) with call-site locations |
| `get_callees` | Direct callees (forward adjacency) with call-site locations |
| `get_call_path` | Shortest call path source→target via BFS |
| `get_impact_radius` | Transitive callers (reverse BFS) grouped by depth with risk scores |
| `detect_dead_code` | Symbols with zero incoming edges, minus entry points |
| `get_communities` | Louvain community detection over symmetrized call edges |
| `get_architecture` | Language distribution, module structure, entry points, hubs, communities |
| `get_git_impact` | `git diff` → affected symbols → impact radius → risk classification |

## Quick Start

```bash
# Build
cargo build --release

# Run (indexes the current directory, serves MCP over streamable HTTP)
./target/release/kernava-server --port 8080 /path/to/your/project

# Connect from any MCP client (Claude Desktop, etc.)
# Endpoint: http://localhost:8080/mcp
```

## Workspace Layout

```
crates/
  kernava-server/    MCP server (rmcp + axum, 16 tools)
  kernava-indexer/   tree-sitter parsing, symbol/call extraction, import resolution
  kernava-graph/     In-RAM call graph (DashMap), Louvain communities, BFS traversal
  kernava-store/     SQLite-WAL persistence, FTS5 search
```

## Call Resolution

Six-strategy cascade at index time:

1. **ImportMap** — match callee against imported names → qualified target
2. **SameFile** — callee matches a symbol defined in the same file
3. **GlobalUnique** — callee is globally unique across the codebase
4. **CrossFile** — callee matches a symbol in a file on the import path
5. **Default** — callee unresolved, edge stored with `target_id = NULL`
6. **Builtins** — common standard library calls (partial v1)

## License

Dual-licensed under MIT OR Apache-2.0.

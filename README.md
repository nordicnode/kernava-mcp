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

- **Transport**: Streamable HTTP **or** stdio, both via [`rmcp`](https://crates.io/crates/rmcp). For the HTTP mode the server is a long-lived process — the graph stays warm between MCP sessions, no cold-start penalty. For stdio the server is spawned as a child process by the MCP client (the mode jcode/Claude Code use when they launch the binary themselves) and shuts down on stdin EOF.
- **Storage**: SQLite-WAL with FTS5. Same architecture proven by [CBM](https://github.com/DeusData/codebase-memory-mcp) at 28M LOC with sub-ms queries. Large-repo soak test pending (§6.5).
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

# Start the MCP server over streamable HTTP (indexes your project, listens on port 8080)
./target/release/kernava serve --port 8080 --project-root /path/to/your/project

# Or run it over stdio (the mode MCP clients that spawn the binary as a child process use)
./target/release/kernava serve --transport stdio --project-root /path/to/your/project

# Or index from CLI without running the server
./target/release/kernava index --path /path/to/your/project
./target/release/kernava stats  # show index statistics

# Query the index from CLI (for debugging/scripting)
./target/release/kernava query search_symbols --args '{"query":"handleRequest"}' --db-path kernava.db
```

For HTTP, configure your MCP client (below) to connect to `http://localhost:8080/mcp`.
For stdio, configure your MCP client to spawn the binary as a child process (see "stdio" config below).

## MCP Client Configuration

### Claude Code

```json
{
  "mcpServers": {
    "kernava": {
      "type": "http",
      "url": "http://localhost:8080/mcp"
    }
  }
}
```

### Cursor

```json
{
  "mcpServers": {
    "kernava": {
      "url": "http://localhost:8080/mcp"
    }
  }
}
```

### Zed

```json
{
  "context_servers": {
    "kernava": {
      "url": "http://localhost:8080/mcp",
      "headers": {}
    }
  }
}
```

### Generic MCP Client (streamable HTTP)

Endpoint: `http://localhost:8080/mcp` (streamable HTTP, POST).

### stdio (jcode, Claude Code, any client that spawns the binary)

Some clients (jcode, Claude Code) launch the MCP server as a child process and talk JSON-RPC over its stdin/stdout rather than dialling an HTTP URL. For those, set `--transport stdio`. Logs are written to stderr so they don't corrupt the JSON-RPC stream on stdout.

```json
{
  "mcpServers": {
    "kernava": {
      "command": "/path/to/kernava",
      "args": ["serve", "--transport", "stdio", "--project-root", "/path/to/your/project"]
    }
  }
}
```

Optional: pass `--db-path /path/to/kernava.db` to persist the index between runs (defaults to `kernava.db` in the server's CWD).

## Configuration

Place a `kernava.toml` at your project root to override defaults. All fields are optional:

```toml
# Maximum file size in bytes. Files larger than this are skipped during indexing.
# Default: 1 MiB (1_048_576)
max_file_size = 1_048_576

# Additional glob patterns to ignore (beyond .gitignore).
# Applied at file level, not directory level.
# Default: []
ignore = ["**/generated/**", "**/*.pb.go"]

# Whether to follow symbolic links during file discovery.
# Default: false — symlinks can cause cycles and index duplicate files.
follow_symlinks = false
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

## Performance

Measured with `criterion` on a debug build (Intel i5-9400, Linux). Release builds are faster.

| Operation | Median | What it measures |
|---|---|---|
| Parse TypeScript | 10.5 µs | tree-sitter parse only |
| Parse Python | 6.7 µs | tree-sitter parse only |
| Parse Rust | 7.2 µs | tree-sitter parse only |
| Index single file | 789 µs | parse → extract → resolve → SQLite upsert (1 transaction) |
| Index 11 files | 2.4 ms | full project: walk → topo sort → index all files |
| Symbol search (FTS5) | 34 µs | full-text search across all symbols |
| Cross-style search | 38 µs | snake_case query matching camelCase symbol |

11-language support, SQLite-WAL storage. Queries measured at 34–38 µs on the
test fixture (11 files). The call graph stays warm in RAM across all MCP
sessions — no cold starts. Large-repo soak test pending (§6.5).

## License

Dual-licensed under MIT OR Apache-2.0.

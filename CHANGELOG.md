# Changelog

## v0.5.0 (2026-07-14)

Initial public release of Kernava — a graph-backed, streamable-HTTP MCP coding server.

### Features

- **16 MCP tools**: `index_project`, `get_index_status`, `search_symbols`, `get_symbol`, `get_file_outline`, `find_references`, `find_definition`, `search_code`, `get_callers`, `get_callees`, `get_call_path`, `get_impact_radius`, `detect_dead_code`, `get_communities`, `get_architecture`, `get_git_impact`
- **11 language support**: TypeScript/TSX, JavaScript/JSX, Python, Rust, Go, Java, C#, Ruby, PHP, C, C++ via tree-sitter (Kotlin deferred — grammar incompatible with workspace tree-sitter 0.25)
- **Graph-backed architecture**: SQLite-WAL storage + DashMap `GraphCache` warm in RAM across all sessions, shared via `Arc<AppState>` — no cold starts
- **Streamable HTTP transport** via `rmcp` (long-lived process, persistent graph) — not stdio (which kills process on disconnect)
- **6-strategy call resolution**: ImportMap, SameFile, ModulePath, ImportMap Case B (class-qualified), SameName, Builtins
- **Louvain community detection** on the call graph
- **kernava.toml config file**: `max_file_size` and custom `ignore` globs, loaded from project root, defaults when absent
- **CLI subcommands**: `serve` (MCP server), `index` (batch index), `stats` (index statistics), `query` (single tool call for scripting/debugging)
- **File watcher**: `notify`-based with XXH3 content-hash dedup, incremental re-indexing with reverse-dependent expansion
- **FTS5 symbol search** with cross-style tokenizer (snake_case ↔ camelCase ↔ PascalCase)
- **Graceful shutdown**: SIGINT + SIGTERM via `tokio::select!`
- **`.gitignore`-aware file discovery** via `ignore::WalkBuilder`
- **Structured tracing** spans on all 16 MCP tool methods

### Performance (debug build, Intel i5-9400)

| Operation | Median |
|---|---|
| Parse TypeScript | 10.5 µs |
| Parse Python | 6.7 µs |
| Parse Rust | 7.2 µs |
| Index single file | 789 µs |
| Index 11 files | 2.4 ms |
| Symbol search (FTS5) | 34 µs |
| Cross-style search | 38 µs |

### Tests

179 tests pass (20 graph + 101 indexer + 14 server + 24 store + 20 integration). Clippy clean (`-D warnings`), `cargo fmt --check` clean.

### CI

GitHub Actions: clippy (`-D warnings`) + fmt check + build + test on ubuntu-latest and macos-latest. `Swatinem/rust-cache@v2` for caching.

### License

MIT OR Apache-2.0 dual license.

### Known Limitations

- Rust `use` paths (`crate::module::func`) don't match file-path-based qualified names in resolver — cross-file calls from `main.rs` unresolved
- ImportMap Case B class-qualified method resolution requires singular match — ambiguous matches return unresolved
- File deletions ignored by watcher (stale nodes persist until full re-index)
- `index_project` is synchronous — no MCP progress notifications (deferred §6.9)
- Session lifecycle (timeout, cache release, reconnect) not implemented (deferred §6.10)
- Parallel indexing not implemented — indexer is sequential
- Kotlin support deferred (`tree-sitter-kotlin 0.3.8` requires `tree-sitter <0.23`)

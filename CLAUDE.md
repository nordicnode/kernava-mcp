# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build / test / lint

CI runs this exact gate on ubuntu-latest + macos-latest; everything below must pass locally:

```bash
cargo fmt --all -- --check          # fmt gate — fix before commit
cargo clippy --workspace --all-targets -- -D warnings   # warnings are errors
cargo build --workspace
cargo test --workspace
```

- Single test: `cargo test -p kernava-indexer --test integration test_snapshot_nodes_and_edges`
- Benches (criterion, indexer only): `cargo bench -p kernava-indexer`
- Run server: `./target/release/kernava serve --port 8080 --project-root /path`
- CLI quick paths: `kernava index --path`, `kernava stats`, `kernava query <tool> --args '<json>' --db-path kernava.db`

## Architecture

Graph-backed code-intelligence MCP server. Parse once → SQLite-WAL → warm in-RAM call graph → serve over streamable HTTP. Four crates, one data flow:

```
file → parser (tree-sitter, 11 langs) → extractor (symbols+calls+imports)
     → resolver (6-strategy cascade) → Store Txn (delete-reinsert + FTS5) → commit
     → GraphCache (DashMaps, warm on startup / sync_upsert on watch) → MCP tool response
```

**Crate boundaries** (low-level → high-level; `store` < `graph` < `indexer` < `server`):

- `kernava-store` — SQLite-WAL. `Store` owns the single `Connection`; `StoreTxn` is the transaction scope used for atomic per-file indexing. Schema: files/nodes/edges/import_edges/meta + `fts5_symbols`. **FK cascade rules**: nodes→files CASCADE; edges.source_id CASCADE; **edges.target_id ON DELETE SET NULL** — unresolved calls persist with NULL target, this is intentional. The builder deletes edges explicitly in `StoreTxn::delete_file_symbols` rather than relying on cascade — preserve that.
- `kernava-graph` — in-RAM. `GraphCache` = six DashMaps (`by_name`, `by_qualified`, `forward`, `reverse`, `nodes`, `file_nodes`). Lock-free reads, **single-writer required** (see SAFETY comment in `sync_delete_file`). `louvain` runs over **symmetrized** CALLS edges (directed variant deferred).
- `kernava-indexer` — parsing + resolution. `parser` (Language enum, Kotlin deferred — needs tree-sitter <0.23), `extractor` (AST walk → symbols/calls/imports), `resolver` (6-strategy cascade; 3 strategies are v1 stubs), `builder` (orchestration), `watcher` (notify, 150ms drain debounce), `config` (`kernava.toml` → `IndexerConfig`).
- `kernava-server` — rmcp + axum. `AppState` wraps `Mutex<Store>` + `GraphCache` + config; `KernavaHandler` impls 16 `#[tool]`s + a `query()` dispatch used by the CLI so the same logic serves MCP and `kernava query`. Mounts `/mcp`.

### Non-obvious — read before touching these

- **Topo-sort in incremental indexing.** `index_incremental*` uses parse-based `build_import_deps`, NOT SQL `import_edges`. SQL import_edges don't exist on a fresh store → alphabetical sort → FK cascade nulls edge targets during delete-reinsert. The parse-based deps work on a cold store. Don't "simplify" it back to SQL. (`builder.rs:~380`+watcher integration test guard this.)
- **GraphCache warm vs cold.** Warmed via `load_from_store` only if `node_count > 0` (server `lib.rs` + `query_cmd`). Empty cache is valid; don't assume it returns *anything* until warmed.
- **`kernava.toml`.** Loaded by `load_config(project_root)`; absent → `Default::default()`. `IndexerConfig { max_file_size (1 MiB default), ignore: Vec<String> }`.
- **FTS5 tokenizer is app-layer, not SQLite's.** `tokenize_symbol_name` splits camelCase/snake_case at insert time ("handleRequest"→"handle request"). Keep insert + query tokenization aligned.
- **macOS `$TMPDIR` symlink.** Temp dirs in tests must be `canonicalize()`d — `/var/folders/...` symlinks to `/private/var/...`; stored vs looked-up paths diverge → `None` lookups.

## Tests & fixtures

- Unit tests: inline `#[cfg(test)] mod tests` in nearly every module.
- Integration: `crates/kernava-indexer/tests/integration.rs` (multi-language fixtures under `tests/fixtures/<lang>-small/…`) and `crates/kernava-server/tests/integration.rs` (tokio vertical-slice tests).
- New language support: add parser + extractor + `parse_imports`, and a fixture dir + integration test mirroring the existing ones.

## Branch / commit

Main is the default branch. Commit co-author trailer: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`. Don't `git add -A` when stray dirs exist (e.g. `.omo/`) — stage explicit paths; `.omo/` is gitignored session state, not repo content.

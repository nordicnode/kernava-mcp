# Kernava MCP Server — E2E Test Report

**Date:** 2026-07-15  
**Coverage:** All 16 MCP tools tested. 69 files, 592 symbols, 686 edges. All 34 existing Rust tests pass.

---

## BUG 1 (HIGH): `split_callee` doesn't handle Rust `::` path separators

**Location:** `crates/kernava-indexer/src/resolver.rs:186-191`

`split_callee` splits on `.` only. Rust `::`-qualified calls like `resolver::resolve_calls(...)` are not split into prefix/suffix. The callee stays as one string `"resolver::resolve_calls"`, which won't match any symbol in the registry (the simple name is `"resolve_calls"`).

**Impact:** 88% of calls unresolved (5162/5848). Cross-module Rust calls via `crate::`, `self::`, `super::` or module paths like `resolver::resolve_calls` are never resolved. This makes `get_call_path`, `get_impact_radius`, `detect_dead_code`, and call graph traversal fundamentally incomplete for Rust projects.

**Fix:** Add `::` splitting alongside `.`:
```rust
fn split_callee(callee: &str) -> (Option<&str>, &str) {
    if let Some(pos) = callee.rfind("::") {
        return (Some(&callee[..pos]), &callee[pos + 2..]);
    }
    match callee.rfind('.') {
        Some(pos) => (Some(&callee[..pos]), &callee[pos + 1..]),
        None => (None, callee),
    }
}
```

---

## BUG 2 (MEDIUM): `get_impact_radius_tool` silently overrides `max_depth=20` to `10`

**Location:** `crates/kernava-server/src/handler.rs:964-968`

```rust
let depth = if params.max_depth == 20 {
    10
} else {
    params.max_depth as usize
};
```

The default `max_depth` is 20 (from `default_depth()`). When a user doesn't specify `max_depth`, they get the default 20, which is silently clamped to 10. But if a user explicitly passes `max_depth=20` wanting 20 hops, they also get 10. This magic override is undocumented.

**Fix:** Use a separate default constant instead of testing against 20:
```rust
let depth = if params.max_depth == 0 { 10 } else { params.max_depth as usize };
```
Or: add `default_impact_depth() -> u32 { 10 }` and use `#[serde(default = "default_impact_depth")]`.

---

## BUG 3 (MEDIUM): `search_code` `file_glob` doesn't support standard glob patterns

**Location:** `crates/kernava-server/src/handler.rs:634-639`

The glob filter is `path.ends_with(g.trim_start_matches('*'))`. This works for simple suffixes like `"*.ts"` (strips to `.ts`) and `"handler.rs"` (matches exactly). But standard patterns fail:
- `"**/*.rs"` → strips to `/*.rs` → matches nothing (no path contains `/*.rs` as suffix)
- `"src/**"` → strips to `src/**` → only matches if path literally ends with `src/**`

**Fix:** Handle `**/*.ext` by stripping `**/` prefix first:
```rust
let filter = g.trim_start_matches("**/").trim_start_matches('*');
```

---

## BUG 4 (LOW): `get_git_impact` produces extra blank lines when some risk categories are empty

**Location:** `crates/kernava-server/src/handler.rs:1321-1332`

When `high` or `medium` vectors are empty, `high.join("\n")` produces `""`, and the format string inserts blank lines.

**Fix:** Filter out empty sections before joining.

---

## BUG 5 (LOW): `get_callers`/`get_callees` O(n²) edge lookup at depth 1

**Location:** `crates/kernava-server/src/handler.rs:760-776` (get_callers), `850-856` (get_callees)

For each direct caller/callee at depth 1, the code re-queries ALL incoming/outgoing edges from the store, then filters for the matching source/target. This is O(n²).

**Fix:** Query edges once before the loop, build a HashMap of `(source_id, target_id) → (file, line)`.

---

## BUG 6 (LOW): `detect_dead_code` false positives for serde default functions and benchmarks

**Location:** `crates/kernava-server/src/handler.rs:1019-1032`

Functions called via serde's `#[serde(default = "fn_name")]` are not tracked by the call graph. `default_limit`, `default_code_limit`, `default_depth` are flagged as dead but are alive via serde. Benchmark functions (`bench_parse`, etc.) are invoked by the harness, not project code.

**Fix:** Add `n.name.starts_with("bench_")` exclusion. Document serde default as known limitation.

---

## BUG 7 (LOW): `get_communities` summary string has confusing `capped` logic

**Location:** `crates/kernava-server/src/handler.rs:1119-1127`

When all multi-member communities are shown, `capped` is false and the summary omits totals. When capped, it redundantly says "showing N multi-member, N total multi-member". The output is inconsistent.

**Fix:** Simplify to always show totals.

---

## OBSERVATIONS

1. **Rust `use` import maps don't map to file-path-based qualified names** — The Rust import parser stores `use` paths as module paths (e.g., `"std::collections::HashMap"`), but the registry uses file-path-based qualified names. Import map strategy is effectively dead code for Rust.

2. **`search_code` reads files from disk on every call** — `std::fs::read_to_string(path)` for every indexed file on each invocation. Consider caching.

3. **Architecture "entry points" shows all 397 exported symbols** — `n.is_exported || n.name == "main"` includes all `pub fn`, `pub struct`, etc. True entry points would be `main` and crate-root `pub fn` only.

4. **`get_architecture` community largest-size assumes sort order** — `communities[0].members.len()` assumes first community is largest.

---

## Test Results Summary

| Tool | Status | Notes |
|------|--------|-------|
| get_index_status | ✅ Pass | Correct stats |
| get_architecture | ✅ Pass | Works but entry_points inflated (O3) |
| search_symbols | ✅ Pass | FTS5 + LIKE fallback work |
| search_code | ⚠️ Works | Glob broken for `**/*.ext` (BUG 3) |
| get_symbol | ✅ Pass | Correct metadata |
| get_file_outline | ✅ Pass | Correct outline |
| get_callees | ✅ Pass | Multi-hop traversal works |
| get_callers | ✅ Pass | O(n²) at depth 1 (BUG 5) |
| find_definition | ✅ Pass | Line filtering works |
| find_references | ✅ Pass | Correct references |
| get_call_path | ⚠️ Works | Missing edges due to BUG 1 |
| get_impact_radius | ⚠️ Works | Silent depth override (BUG 2) |
| detect_dead_code | ⚠️ Works | False positives (BUG 6) |
| get_communities | ✅ Pass | Confusing summary (BUG 7) |
| get_git_impact | ⚠️ Works | Blank lines (BUG 4) |
| index_project | ✅ Pass | Reindex works, 295ms |

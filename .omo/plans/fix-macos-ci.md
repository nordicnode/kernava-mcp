# Fix macOS CI: Temp Dir Canonicalization Mismatch

## Status: AWAITING APPROVAL

## Problem

GitHub Actions CI fails on **macOS only**. Linux passes clean. The `Test` step fails with 2 integration test failures:

1. `test_index_incremental_reindexes_changed_file_and_reverse_deps` — `crates/kernava-indexer/tests/integration.rs:328`
2. `test_snapshot_nodes_and_edges` — `crates/kernava-indexer/tests/integration.rs:441`

## Root Cause

**Path canonicalization mismatch between indexer storage and test lookups on macOS.**

On macOS, `std::env::temp_dir()` returns `/var/folders/8j/sfr9qqcj73j4p6nhwcfpr0th0000gn/T/...` — a symlinked path. `index_full` canonicalizes the project root at `builder.rs:217`:

```rust
let root = std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
```

This canonicalizes `/var/folders/.../T/kernava-test-.../` → `/private/var/folders/.../T/kernava-test-.../` on macOS (resolves the `/var` → `/private/var` symlink). All files walked from the canonicalized root are stored in SQLite under `/private/var/folders/.../main.ts` etc.

But the tests build paths from the **uncanonicalized** `dir` returned by `copy_fixture_to_tmp()`:

```rust
// integration.rs:309
let math_path = dir.join("math.ts");  // /var/folders/.../math.ts (NOT canonicalized)

// integration.rs:432
let math_path = dir.join("math.ts").to_string_lossy().to_string();  // /var/folders/.../math.ts

// integration.rs:391
store.get_file_id(&p.to_string_lossy())  // looks up /var/folders/.../math.ts
```

`get_file_id` does **exact string match** on the `path` column — no normalization. SQLite has `/private/var/folders/.../math.ts`, test queries for `/var/folders/.../math.ts` → returns `None`.

### Failure 1: `test_index_incremental_reindexes_changed_file_and_reverse_deps` (L328)

`index_incremental` BFS at `builder.rs:391`:
```rust
let Some(fid) = store.get_file_id(&p.to_string_lossy())? else { continue; };
```
`p` = `math_path` = `/var/folders/.../math.ts` (uncanonicalized). Store has `/private/var/folders/.../math.ts`. Lookup returns `None` → `continue` → reverse-dep BFS never expands → only `math.ts` re-indexed (its direct call), `main.ts` (reverse-dep) NOT included → assertion at L328 fails:

```
main.ts (reverse-dep) should be re-indexed: ["/var/folders/.../math.ts"]
```

### Failure 2: `test_snapshot_nodes_and_edges` (L441)

`store.get_file_id(&math_path)` at L441 where `math_path` = `/var/folders/.../math.ts` (uncanonicalized, L432). Returns `None`. `.unwrap().unwrap()` panics:

```
called `Option::unwrap()` on a `None` value
```

### Why Linux passes

Linux `std::env::temp_dir()` returns `/tmp/...` — NOT symlinked. `canonicalize("/tmp/...")` returns `/tmp/...` unchanged. No mismatch.

### Why previous fix attempt ("Fix macOS CI: canonicalize temp dirs in tests") failed

That commit likely added canonicalization to `copy_fixture_to_tmp` or some test paths but missed other call sites. The issue is systemic: every path constructed from `dir.join(...)` must be canonicalized, or the indexer must canonicalize incoming changed paths before `get_file_id` lookup.

## Solution

**Canonicalize `dir` inside `copy_fixture_to_tmp()`** so all downstream `dir.join(...)` paths are already canonical. One-line fix at the source — no changes needed at each call site.

### Step 1: Canonicalize temp dir in `copy_fixture_to_tmp`

**File:** `crates/kernava-indexer/tests/integration.rs:227-243`

**Change:** After `create_dir_all(&dst)`, canonicalize `dst` and use the canonical path for the rest of the function and as the return value.

```rust
fn copy_fixture_to_tmp() -> PathBuf {
    let src = fixture_dir();
    let dst = std::env::temp_dir().join(format!(
        "kernava-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dst).unwrap();
    // Canonicalize so all derived paths match what index_full stores.
    // macOS: std::env::temp_dir() → /var/folders/.../T (symlinked to /private/var/...).
    // index_full canonicalizes the root, storing /private/var/... paths in SQLite.
    // Without canonicalize here, dir.join("math.ts") → /var/folders/... (non-canonical),
    // and get_file_id exact-match returns None → test failures (CI run 29388280686).
    let dst = std::fs::canonicalize(&dst).unwrap_or(dst);
    for entry in std::fs::read_dir(&src).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), dst.join(entry.file_name())).unwrap();
    }
    dst
}
```

**Why this works:**
- `index_full` canonicalizes root at `builder.rs:217`, walks from canonical root, stores canonical paths.
- `copy_fixture_to_tmp` now returns canonical `dst`.
- All `dir.join("math.ts")` etc. in tests become canonical → match stored paths → `get_file_id` succeeds.

### Step 2 (optional, defensive): Canonicalize changed paths in `index_incremental`

**File:** `crates/kernava-indexer/src/builder.rs:371-426`

**Rationale:** `index_incremental` receives `changed: Vec<PathBuf>` from callers. If a caller (MCP tool, watcher, CLI) passes uncanonicalized paths, `get_file_id` at L391 fails silently and reverse-dep BFS skips all expansion. The test triggered this code path; real users would too.

**Change:** Canonicalize each changed path before the BFS loop at L385:

```rust
pub fn index_incremental_with_config(
    store: &mut Store,
    changed: Vec<std::path::PathBuf>,
    config: &crate::config::IndexerConfig,
) -> Result<Vec<IndexFileResult>> {
    // Canonicalize so get_file_id matches paths stored by index_full
    // (which canonicalizes the root before walking).
    let to_index: HashSet<std::path::PathBuf> = changed
        .into_iter()
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
        .collect();
    let mut visited: HashSet<i64> = HashSet::new();
    let mut queue: VecDeque<std::path::PathBuf> = to_index.iter().cloned().collect();
    // ... rest unchanged
```

This ensures `index_incremental` is robust regardless of what paths callers pass — not just the test.

### Step 3: Verify fix locally

Cannot reproduce on Linux (no symlink mismatch). Verify by:

1. **Compile check:** `cargo test -p kernava-indexer --test integration -- --track-time` (confirms no compile regression)
2. **Logic review:** Confirm `copy_fixture_to_tmp` return value is canonicalized and all 3 affected tests use `dir` from `copy_fixture_to_tmp`:
   - `test_index_incremental_reindexes_changed_file_and_reverse_deps` (L302) ✓
   - `test_snapshot_nodes_and_edges` (L428) ✓
   - `test_index_incremental_on_fresh_store` (L364) ✓
3. **Push and watch CI:**
   ```bash
   git add -A && git commit -m "Fix macOS CI: canonicalize temp dirs in copy_fixture_to_tmp"
   git push origin main
   gh run watch
   ```

## Files Changed

| File | Change | Lines |
|------|--------|-------|
| `crates/kernava-indexer/tests/integration.rs` | Add `canonicalize` in `copy_fixture_to_tmp` | 1 line added (~L237) |
| `crates/kernava-indexer/src/builder.rs` | (Optional) Canonicalize changed paths in `index_incremental_with_config` | ~3 lines changed (L385) |

## Risk

- **Step 1:** Zero risk. Only changes which path form tests use. All already-passing Linux tests unaffected (`/tmp` canonicalizes to itself).
- **Step 2:** Low risk. `canonicalize` on a non-existent path returns `unwrap_or(p)` (unchanged). For-existent paths always resolve. No behavior change on Linux.
- **No production code path touched by Step 1.**

## Verification After Implementation

```bash
# Local (Linux — confirms no regression)
cargo test -p kernava-indexer --test integration

# CI (macOS — confirms fix)
gh run watch
```

Expected: both `ubuntu-latest` AND `macos-latest` jobs pass all steps.

// kernava-store: SQLite schema, migrations, and query layer
// P1 task 1.2: schema definition

pub mod fts5;
pub mod queries;

pub use queries::{
    EdgeRecord, EdgeRow, FileRecord, ImportEdgeRecord, IndexStats, NodeRecord, NodeRow, Store,
    StoreTxn,
};

use anyhow::Result;
use rusqlite::Connection;

/// Current schema version. Increment on breaking schema changes.
pub const SCHEMA_VERSION: u32 = 1;

/// Initialize the database schema. Creates all tables if they don't exist.
/// Idempotent — safe to call on every startup.
pub fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        PRAGMA journal_mode = WAL;
        PRAGMA foreign_keys = ON;
        PRAGMA synchronous = NORMAL;

        CREATE TABLE IF NOT EXISTS meta (
            key     TEXT PRIMARY KEY,
            value   TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS files (
            id              INTEGER PRIMARY KEY,
            path            TEXT UNIQUE NOT NULL,
            language        TEXT NOT NULL,
            content_hash    BLOB NOT NULL,
            mtime           INTEGER NOT NULL,
            size            INTEGER NOT NULL,
            symbol_count    INTEGER DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS nodes (
            id              INTEGER PRIMARY KEY,
            kind            TEXT NOT NULL,
            name            TEXT NOT NULL,
            qualified_name  TEXT NOT NULL,
            file_id         INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
            line_start      INTEGER NOT NULL,
            line_end        INTEGER NOT NULL,
            col_start       INTEGER,
            signature       TEXT,
            return_type     TEXT,
            receiver_type   TEXT,
            is_exported     INTEGER DEFAULT 0,
            complexity      INTEGER DEFAULT 0,
            decorators      TEXT,
            metadata        TEXT
        );

        -- ponytail: surrogate PK instead of composite PK on edges.
        -- v2 may need edge-level metadata or multiple call sites at same line.
        -- Surrogate id avoids a migration later. Same storage cost.
        CREATE TABLE IF NOT EXISTS edges (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            source_id       INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
            target_id       INTEGER REFERENCES nodes(id) ON DELETE SET NULL,
            edge_type       TEXT NOT NULL,
            confidence      REAL DEFAULT 1.0,
            file_id         INTEGER REFERENCES files(id) ON DELETE CASCADE,
            line            INTEGER,
            metadata        TEXT,
            UNIQUE(source_id, target_id, edge_type, file_id, line)
        );

        -- Reverse-dependency map: which files import from which files.
        -- Used by incremental indexer to re-resolve reverse-dependents on file change.
        CREATE TABLE IF NOT EXISTS import_edges (
            importer_file_id    INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
            imported_file_id    INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
            PRIMARY KEY (importer_file_id, imported_file_id)
        );

        CREATE INDEX IF NOT EXISTS idx_nodes_name ON nodes(name);
        CREATE INDEX IF NOT EXISTS idx_nodes_qualified ON nodes(qualified_name);
        CREATE INDEX IF NOT EXISTS idx_nodes_file ON nodes(file_id);
        CREATE INDEX IF NOT EXISTS idx_edges_source ON edges(source_id, edge_type);
        CREATE INDEX IF NOT EXISTS idx_edges_target ON edges(target_id, edge_type);
        CREATE INDEX IF NOT EXISTS idx_edges_type ON edges(edge_type);
        CREATE INDEX IF NOT EXISTS idx_import_edges_imported ON import_edges(imported_file_id);

        INSERT OR IGNORE INTO meta (key, value) VALUES ('schema_version', '1');
        ",
    )?;

    Ok(())
}

/// FTS5 virtual table for full-text symbol search.
/// Created separately so it can be rebuilt independently of the main schema.
pub fn init_fts5(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE VIRTUAL TABLE IF NOT EXISTS fts5_symbols USING fts5(
            name,
            qualified_name,
            signature,
            tokenize = 'unicode61 remove_diacritics 2'
        );
        ",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_schema_creation() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        init_fts5(&conn).unwrap();

        // Verify tables exist
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert!(tables.contains(&"meta".to_string()));
        assert!(tables.contains(&"files".to_string()));
        assert!(tables.contains(&"nodes".to_string()));
        assert!(tables.contains(&"edges".to_string()));
        assert!(tables.contains(&"import_edges".to_string()));
        assert!(tables.contains(&"fts5_symbols".to_string()));
    }

    #[test]
    fn test_schema_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        init_schema(&conn).unwrap(); // should not error
    }

    #[test]
    fn test_cascade_delete_file() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();

        conn.execute(
            "INSERT INTO files (id, path, language, content_hash, mtime, size) VALUES (1, 'test.ts', 'typescript', zeroblob(8), 0, 0)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualified_name, file_id, line_start, line_end) VALUES (1, 'function', 'foo', 'test.foo', 1, 1, 5)",
            [],
        ).unwrap();

        // Delete file → node should cascade
        conn.execute("DELETE FROM files WHERE id = 1", []).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_edge_null_target_allowed() {
        // Unresolved calls have NULL target_id — must be allowed by schema
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();

        conn.execute(
            "INSERT INTO files (id, path, language, content_hash, mtime, size) VALUES (1, 'test.ts', 'typescript', zeroblob(8), 0, 0)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualified_name, file_id, line_start, line_end) VALUES (1, 'function', 'caller', 'test.caller', 1, 1, 5)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO edges (source_id, target_id, edge_type, confidence, file_id, line) VALUES (1, NULL, 'CALLS', 0.5, 1, 3)",
            [],
        ).unwrap();

        let target_id: Option<i64> = conn
            .query_row(
                "SELECT target_id FROM edges WHERE source_id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(target_id.is_none());
    }
}

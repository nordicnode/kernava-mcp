// kernava-store: query layer
// P1 task 1.4: CRUD operations for files, nodes, edges
// All operations are transaction-batched for bulk insert performance.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension, Transaction};

use crate::SCHEMA_VERSION;

/// Entry point for all database operations.
/// Owns the SQLite connection, configured in WAL mode.
pub struct Store {
    conn: Connection,
}

/// A row representing a symbol node, returned by queries.
#[derive(Debug, Clone)]
pub struct NodeRow {
    pub id: i64,
    pub kind: String,
    pub name: String,
    pub qualified_name: String,
    pub file_id: i64,
    pub line_start: i32,
    pub line_end: i32,
    pub col_start: Option<i32>,
    pub signature: Option<String>,
    pub return_type: Option<String>,
    pub receiver_type: Option<String>,
    pub is_exported: bool,
    pub complexity: i32,
    pub decorators: Option<String>,
    pub metadata: Option<String>,
}

/// A row representing an edge between two symbol nodes.
#[derive(Debug, Clone)]
pub struct EdgeRow {
    pub id: i64,
    pub source_id: i64,
    pub target_id: Option<i64>,
    pub edge_type: String,
    pub confidence: f64,
    pub file_id: Option<i64>,
    pub line: Option<i32>,
    pub metadata: Option<String>,
}

/// A file record for insertion.
#[derive(Debug, Clone)]
pub struct FileRecord {
    pub path: String,
    pub language: String,
    pub content_hash: Vec<u8>,
    pub mtime: i64,
    pub size: i64,
}

/// A symbol node for insertion.
#[derive(Debug, Clone)]
pub struct NodeRecord {
    pub kind: String,
    pub name: String,
    pub qualified_name: String,
    pub file_id: i64,
    pub line_start: i32,
    pub line_end: i32,
    pub col_start: Option<i32>,
    pub signature: Option<String>,
    pub return_type: Option<String>,
    pub receiver_type: Option<String>,
    pub is_exported: bool,
    pub complexity: i32,
    pub decorators: Option<String>,
    pub metadata: Option<String>,
}

/// An edge for insertion.
#[derive(Debug, Clone)]
pub struct EdgeRecord {
    pub source_id: i64,
    pub target_id: Option<i64>,
    pub edge_type: String,
    pub confidence: f64,
    pub file_id: Option<i64>,
    pub line: Option<i32>,
    pub metadata: Option<String>,
}

/// An import-edge for insertion (reverse-dependency map).
#[derive(Debug, Clone)]
pub struct ImportEdgeRecord {
    pub importer_file_id: i64,
    pub imported_file_id: i64,
}

impl Store {
    /// Open a database at the given path, initializing schema if needed.
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        crate::init_schema(&conn)?;
        crate::init_fts5(&conn)?;
        Ok(Self { conn })
    }

    /// Open an in-memory database (for tests).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        crate::init_schema(&conn)?;
        crate::init_fts5(&conn)?;
        Ok(Self { conn })
    }

    /// Access the raw connection (for advanced queries / transactions).
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    // ── File operations ──────────────────────────────────

    /// Insert or update a file. Returns the file's row id.
    pub fn upsert_file(&self, rec: &FileRecord) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO files (path, language, content_hash, mtime, size)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(path) DO UPDATE SET
                language = excluded.language,
                content_hash = excluded.content_hash,
                mtime = excluded.mtime,
                size = excluded.size",
            params![
                rec.path,
                rec.language,
                &rec.content_hash,
                rec.mtime,
                rec.size
            ],
        )?;
        let id: i64 = self.conn.query_row(
            "SELECT id FROM files WHERE path = ?1",
            params![rec.path],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    /// Look up a file by its path. Returns the id if found.
    pub fn get_file_id(&self, path: &str) -> Result<Option<i64>> {
        let id = self
            .conn
            .query_row(
                "SELECT id FROM files WHERE path = ?1",
                params![path],
                |row| row.get(0),
            )
            .optional()?;
        Ok(id)
    }

    /// Look up a file's path by its id (reverse of `get_file_id`).
    pub fn get_file_path(&self, file_id: i64) -> Result<Option<String>> {
        let path = self
            .conn
            .query_row(
                "SELECT path FROM files WHERE id = ?1",
                params![file_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(path)
    }

    /// Get file's content hash to check if re-indexing is needed.
    pub fn get_file_hash(&self, path: &str) -> Result<Option<Vec<u8>>> {
        let hash = self
            .conn
            .query_row(
                "SELECT content_hash FROM files WHERE path = ?1",
                params![path],
                |row| row.get(0),
            )
            .optional()?;
        Ok(hash)
    }

    /// Delete a file and all its symbols (cascades via FK).
    pub fn delete_file(&self, file_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM files WHERE id = ?1", params![file_id])?;
        Ok(())
    }

    /// Delete a file by path and all its symbols.
    pub fn delete_file_by_path(&self, path: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM files WHERE path = ?1", params![path])?;
        Ok(())
    }

    // ── Node operations ──────────────────────────────────

    /// Insert a symbol node. Returns the new node's id.
    pub fn insert_node(&self, rec: &NodeRecord) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO nodes (kind, name, qualified_name, file_id, line_start, line_end,
                col_start, signature, return_type, receiver_type, is_exported, complexity,
                decorators, metadata)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                rec.kind,
                rec.name,
                rec.qualified_name,
                rec.file_id,
                rec.line_start,
                rec.line_end,
                rec.col_start,
                rec.signature,
                rec.return_type,
                rec.receiver_type,
                rec.is_exported as i32,
                rec.complexity,
                rec.decorators,
                rec.metadata,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Batch insert nodes within a transaction. Returns the list of generated ids.
    pub fn insert_nodes_batch(&self, recs: &[NodeRecord]) -> Result<Vec<i64>> {
        let tx = self.conn.unchecked_transaction()?;
        let mut ids = Vec::with_capacity(recs.len());
        for rec in recs {
            tx.execute(
                "INSERT INTO nodes (kind, name, qualified_name, file_id, line_start, line_end,
                    col_start, signature, return_type, receiver_type, is_exported, complexity,
                    decorators, metadata)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    rec.kind,
                    rec.name,
                    rec.qualified_name,
                    rec.file_id,
                    rec.line_start,
                    rec.line_end,
                    rec.col_start,
                    rec.signature,
                    rec.return_type,
                    rec.receiver_type,
                    rec.is_exported as i32,
                    rec.complexity,
                    rec.decorators,
                    rec.metadata,
                ],
            )?;
            ids.push(tx.last_insert_rowid());
        }
        tx.commit()?;
        Ok(ids)
    }

    /// Get a node by id.
    pub fn get_node(&self, id: i64) -> Result<Option<NodeRow>> {
        let node = self
            .conn
            .query_row(
                "SELECT id, kind, name, qualified_name, file_id, line_start, line_end,
                    col_start, signature, return_type, receiver_type, is_exported,
                    complexity, decorators, metadata
                 FROM nodes WHERE id = ?1",
                params![id],
                |row| {
                    Ok(NodeRow {
                        id: row.get(0)?,
                        kind: row.get(1)?,
                        name: row.get(2)?,
                        qualified_name: row.get(3)?,
                        file_id: row.get(4)?,
                        line_start: row.get(5)?,
                        line_end: row.get(6)?,
                        col_start: row.get(7)?,
                        signature: row.get(8)?,
                        return_type: row.get(9)?,
                        receiver_type: row.get(10)?,
                        is_exported: row.get::<_, i32>(11)? != 0,
                        complexity: row.get(12)?,
                        decorators: row.get(13)?,
                        metadata: row.get(14)?,
                    })
                },
            )
            .optional()?;
        Ok(node)
    }

    /// Find nodes by simple name (exact match).
    pub fn find_nodes_by_name(&self, name: &str) -> Result<Vec<NodeRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, name, qualified_name, file_id, line_start, line_end,
                col_start, signature, return_type, receiver_type, is_exported,
                complexity, decorators, metadata
             FROM nodes WHERE name = ?1 ORDER BY file_id, line_start",
        )?;
        let rows = stmt.query_map(params![name], |row| {
            Ok(NodeRow {
                id: row.get(0)?,
                kind: row.get(1)?,
                name: row.get(2)?,
                qualified_name: row.get(3)?,
                file_id: row.get(4)?,
                line_start: row.get(5)?,
                line_end: row.get(6)?,
                col_start: row.get(7)?,
                signature: row.get(8)?,
                return_type: row.get(9)?,
                receiver_type: row.get(10)?,
                is_exported: row.get::<_, i32>(11)? != 0,
                complexity: row.get(12)?,
                decorators: row.get(13)?,
                metadata: row.get(14)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Find a node by qualified name (exact match).
    pub fn find_node_by_qualified(&self, qualified_name: &str) -> Result<Option<NodeRow>> {
        let node = self
            .conn
            .query_row(
                "SELECT id, kind, name, qualified_name, file_id, line_start, line_end,
                    col_start, signature, return_type, receiver_type, is_exported,
                    complexity, decorators, metadata
                 FROM nodes WHERE qualified_name = ?1",
                params![qualified_name],
                |row| {
                    Ok(NodeRow {
                        id: row.get(0)?,
                        kind: row.get(1)?,
                        name: row.get(2)?,
                        qualified_name: row.get(3)?,
                        file_id: row.get(4)?,
                        line_start: row.get(5)?,
                        line_end: row.get(6)?,
                        col_start: row.get(7)?,
                        signature: row.get(8)?,
                        return_type: row.get(9)?,
                        receiver_type: row.get(10)?,
                        is_exported: row.get::<_, i32>(11)? != 0,
                        complexity: row.get(12)?,
                        decorators: row.get(13)?,
                        metadata: row.get(14)?,
                    })
                },
            )
            .optional()?;
        Ok(node)
    }

    /// Get all nodes for a given file.
    pub fn get_nodes_for_file(&self, file_id: i64) -> Result<Vec<NodeRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, name, qualified_name, file_id, line_start, line_end,
                col_start, signature, return_type, receiver_type, is_exported,
                complexity, decorators, metadata
             FROM nodes WHERE file_id = ?1 ORDER BY line_start",
        )?;
        let rows = stmt.query_map(params![file_id], |row| {
            Ok(NodeRow {
                id: row.get(0)?,
                kind: row.get(1)?,
                name: row.get(2)?,
                qualified_name: row.get(3)?,
                file_id: row.get(4)?,
                line_start: row.get(5)?,
                line_end: row.get(6)?,
                col_start: row.get(7)?,
                signature: row.get(8)?,
                return_type: row.get(9)?,
                receiver_type: row.get(10)?,
                is_exported: row.get::<_, i32>(11)? != 0,
                complexity: row.get(12)?,
                decorators: row.get(13)?,
                metadata: row.get(14)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    // ── Edge operations ──────────────────────────────────

    /// Insert an edge. Returns the edge id.
    pub fn insert_edge(&self, rec: &EdgeRecord) -> Result<i64> {
        self.conn.execute(
            "INSERT OR IGNORE INTO edges (source_id, target_id, edge_type, confidence,
                file_id, line, metadata)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                rec.source_id,
                rec.target_id,
                rec.edge_type,
                rec.confidence,
                rec.file_id,
                rec.line,
                rec.metadata,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Batch insert edges within a transaction.
    pub fn insert_edges_batch(&self, recs: &[EdgeRecord]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for rec in recs {
            tx.execute(
                "INSERT OR IGNORE INTO edges (source_id, target_id, edge_type, confidence,
                    file_id, line, metadata)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    rec.source_id,
                    rec.target_id,
                    rec.edge_type,
                    rec.confidence,
                    rec.file_id,
                    rec.line,
                    rec.metadata,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Get all outgoing edges from a node (callees).
    pub fn get_outgoing_edges(&self, node_id: i64) -> Result<Vec<EdgeRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, source_id, target_id, edge_type, confidence, file_id, line, metadata
             FROM edges WHERE source_id = ?1",
        )?;
        let rows = stmt.query_map(params![node_id], |row| {
            Ok(EdgeRow {
                id: row.get(0)?,
                source_id: row.get(1)?,
                target_id: row.get(2)?,
                edge_type: row.get(3)?,
                confidence: row.get(4)?,
                file_id: row.get(5)?,
                line: row.get(6)?,
                metadata: row.get(7)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Get all incoming edges to a node (callers / references).
    pub fn get_incoming_edges(&self, node_id: i64) -> Result<Vec<EdgeRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, source_id, target_id, edge_type, confidence, file_id, line, metadata
             FROM edges WHERE target_id = ?1",
        )?;
        let rows = stmt.query_map(params![node_id], |row| {
            Ok(EdgeRow {
                id: row.get(0)?,
                source_id: row.get(1)?,
                target_id: row.get(2)?,
                edge_type: row.get(3)?,
                confidence: row.get(4)?,
                file_id: row.get(5)?,
                line: row.get(6)?,
                metadata: row.get(7)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Get all nodes in the store. Used by GraphCache bulk load on startup.
    pub fn get_all_nodes(&self) -> Result<Vec<NodeRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, name, qualified_name, file_id, line_start, line_end,
                col_start, signature, return_type, receiver_type, is_exported,
                complexity, decorators, metadata
             FROM nodes ORDER BY file_id, line_start",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(NodeRow {
                id: row.get(0)?,
                kind: row.get(1)?,
                name: row.get(2)?,
                qualified_name: row.get(3)?,
                file_id: row.get(4)?,
                line_start: row.get(5)?,
                line_end: row.get(6)?,
                col_start: row.get(7)?,
                signature: row.get(8)?,
                return_type: row.get(9)?,
                receiver_type: row.get(10)?,
                is_exported: row.get::<_, i32>(11)? != 0,
                complexity: row.get(12)?,
                decorators: row.get(13)?,
                metadata: row.get(14)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Get all edges in the store. Used by GraphCache bulk load on startup.
    pub fn get_all_edges(&self) -> Result<Vec<EdgeRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, source_id, target_id, edge_type, confidence, file_id, line, metadata
             FROM edges",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(EdgeRow {
                id: row.get(0)?,
                source_id: row.get(1)?,
                target_id: row.get(2)?,
                edge_type: row.get(3)?,
                confidence: row.get(4)?,
                file_id: row.get(5)?,
                line: row.get(6)?,
                metadata: row.get(7)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    // ── Import-edge operations ──────────────────────────

    /// Record that `importer_file_id` imports from `imported_file_id`.
    pub fn insert_import_edge(&self, rec: &ImportEdgeRecord) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO import_edges (importer_file_id, imported_file_id)
             VALUES (?1, ?2)",
            params![rec.importer_file_id, rec.imported_file_id],
        )?;
        Ok(())
    }

    /// Get all files that import from the given file (reverse dependents).
    pub fn get_reverse_deps(&self, file_id: i64) -> Result<Vec<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT importer_file_id FROM import_edges WHERE imported_file_id = ?1")?;
        let rows = stmt.query_map(params![file_id], |row| row.get(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    // ── Delete operations ─────────────────────────────────

    /// Delete all symbols (nodes) and edges for a file.
    /// Edges cascade via FK; nodes cascade via FK; we just delete the file.
    /// But we also need to clean up edges where source nodes are in this file
    /// but target nodes are elsewhere — that cascades via ON DELETE CASCADE on source_id.
    /// IMPORTANT: also nullify edges FROM external nodes TO nodes in this file.
    /// Those have target_id pointing into this file; cascade SET NULL handles it.
    /// So deleting all nodes for a file is sufficient — FK cascades handle edges.
    pub fn delete_file_symbols(&self, file_id: i64) -> Result<()> {
        // Delete FTS5 entries first (before nodes are gone, so subquery can find rowids)
        crate::fts5::delete_fts_for_file(&self.conn, file_id)?;
        self.conn
            .execute("DELETE FROM nodes WHERE file_id = ?1", params![file_id])?;
        // Also clear import_edges where this file was importer or imported
        // so stale reverse-dep entries don't cause unnecessary re-resolution.
        self.conn.execute(
            "DELETE FROM import_edges WHERE importer_file_id = ?1 OR imported_file_id = ?1",
            params![file_id],
        )?;
        Ok(())
    }

    // ── Meta operations ───────────────────────────────────

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let val = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(val)
    }

    /// Get index statistics for CLI `stats` command.
    pub fn stats(&self) -> Result<IndexStats> {
        let file_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))?;
        let node_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))?;
        let edge_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM edges", [], |row| row.get(0))?;
        let import_edge_count: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM import_edges", [], |row| row.get(0))?;
        let indexed_at = self.get_meta("indexed_at")?;
        let schema_version = self.get_meta("schema_version")?;

        let language_distribution: Vec<(String, i64)> = {
            let mut stmt = self.conn.prepare(
                "SELECT language, COUNT(*) FROM files GROUP BY language ORDER BY COUNT(*) DESC",
            )?;
            let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        Ok(IndexStats {
            file_count,
            node_count,
            edge_count,
            import_edge_count,
            indexed_at,
            schema_version,
            language_distribution,
        })
    }

    // ── Transaction support ──────────────────────────────

    /// Start an explicit transaction. Use `StoreTxn` methods for all inserts
    /// within the transaction, then call `.commit()` to atomically persist.
    pub fn transaction(&mut self) -> Result<StoreTxn<'_>> {
        let tx = self.conn.transaction()?;
        Ok(StoreTxn { tx })
    }
}

/// A transaction-scoped handle for atomic batch operations.
/// All inserts within this transaction commit together on `.commit()`.
pub struct StoreTxn<'store> {
    tx: Transaction<'store>,
}

impl<'store> StoreTxn<'store> {
    /// Commit the transaction.
    pub fn commit(self) -> Result<()> {
        self.tx.commit()?;
        Ok(())
    }

    /// Upsert a file within the transaction. Returns the file's row id.
    pub fn upsert_file(&self, rec: &FileRecord) -> Result<i64> {
        self.tx.execute(
            "INSERT INTO files (path, language, content_hash, mtime, size)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(path) DO UPDATE SET
                language = excluded.language,
                content_hash = excluded.content_hash,
                mtime = excluded.mtime,
                size = excluded.size",
            params![
                rec.path,
                rec.language,
                &rec.content_hash,
                rec.mtime,
                rec.size
            ],
        )?;
        let id: i64 = self.tx.query_row(
            "SELECT id FROM files WHERE path = ?1",
            params![rec.path],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    /// Look up a file by path within the transaction.
    pub fn get_file_id(&self, path: &str) -> Result<Option<i64>> {
        let id = self
            .tx
            .query_row(
                "SELECT id FROM files WHERE path = ?1",
                params![path],
                |row| row.get(0),
            )
            .optional()?;
        Ok(id)
    }

    /// Delete all symbols (nodes) and edges for a file within the transaction.
    pub fn delete_file_symbols(&self, file_id: i64) -> Result<()> {
        // Delete FTS5 entries first (before nodes are gone, so subquery can find rowids)
        self.tx.execute(
            "DELETE FROM fts5_symbols WHERE rowid IN (SELECT id FROM nodes WHERE file_id = ?1)",
            params![file_id],
        )?;
        // Explicitly delete edges for this file — don't rely solely on FK cascade
        self.tx
            .execute("DELETE FROM edges WHERE file_id = ?1", params![file_id])?;
        self.tx
            .execute("DELETE FROM nodes WHERE file_id = ?1", params![file_id])?;
        self.tx.execute(
            "DELETE FROM import_edges WHERE importer_file_id = ?1 OR imported_file_id = ?1",
            params![file_id],
        )?;
        Ok(())
    }

    /// Batch insert nodes within the transaction. Returns the list of generated ids.
    pub fn insert_nodes_batch(&self, recs: &[NodeRecord]) -> Result<Vec<i64>> {
        let mut ids = Vec::with_capacity(recs.len());
        for rec in recs {
            self.tx.execute(
                "INSERT INTO nodes (kind, name, qualified_name, file_id, line_start, line_end,
                    col_start, signature, return_type, receiver_type, is_exported, complexity,
                    decorators, metadata)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    rec.kind,
                    rec.name,
                    rec.qualified_name,
                    rec.file_id,
                    rec.line_start,
                    rec.line_end,
                    rec.col_start,
                    rec.signature,
                    rec.return_type,
                    rec.receiver_type,
                    rec.is_exported as i32,
                    rec.complexity,
                    rec.decorators,
                    rec.metadata,
                ],
            )?;
            let id = self.tx.last_insert_rowid();
            // Insert FTS5 index row with tokenized name
            let tokenized = crate::fts5::tokenize_symbol_name(&rec.name);
            self.tx.execute(
                "INSERT INTO fts5_symbols (rowid, name, qualified_name, signature)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    id,
                    tokenized,
                    &rec.qualified_name,
                    rec.signature.as_deref().unwrap_or("")
                ],
            )?;
            ids.push(id);
        }
        Ok(ids)
    }

    /// Find a node by qualified name within the transaction.
    pub fn find_node_by_qualified(&self, qualified_name: &str) -> Result<Option<NodeRow>> {
        let node = self
            .tx
            .query_row(
                "SELECT id, kind, name, qualified_name, file_id, line_start, line_end,
                    col_start, signature, return_type, receiver_type, is_exported,
                    complexity, decorators, metadata
                 FROM nodes WHERE qualified_name = ?1",
                params![qualified_name],
                |row| {
                    Ok(NodeRow {
                        id: row.get(0)?,
                        kind: row.get(1)?,
                        name: row.get(2)?,
                        qualified_name: row.get(3)?,
                        file_id: row.get(4)?,
                        line_start: row.get(5)?,
                        line_end: row.get(6)?,
                        col_start: row.get(7)?,
                        signature: row.get(8)?,
                        return_type: row.get(9)?,
                        receiver_type: row.get(10)?,
                        is_exported: row.get::<_, i32>(11)? != 0,
                        complexity: row.get(12)?,
                        decorators: row.get(13)?,
                        metadata: row.get(14)?,
                    })
                },
            )
            .optional()?;
        Ok(node)
    }

    /// Batch insert edges within the transaction.
    pub fn insert_edges_batch(&self, recs: &[EdgeRecord]) -> Result<()> {
        for rec in recs {
            self.tx.execute(
                "INSERT OR IGNORE INTO edges (source_id, target_id, edge_type, confidence,
                    file_id, line, metadata)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    rec.source_id,
                    rec.target_id,
                    rec.edge_type,
                    rec.confidence,
                    rec.file_id,
                    rec.line,
                    rec.metadata,
                ],
            )?;
        }
        Ok(())
    }

    /// Insert an import edge within the transaction.
    pub fn insert_import_edge(&self, rec: &ImportEdgeRecord) -> Result<()> {
        self.tx.execute(
            "INSERT OR IGNORE INTO import_edges (importer_file_id, imported_file_id)
             VALUES (?1, ?2)",
            params![rec.importer_file_id, rec.imported_file_id],
        )?;
        Ok(())
    }

    /// Set a meta key within the transaction.
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.tx.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }
}

#[derive(Debug)]
pub struct IndexStats {
    pub file_count: i64,
    pub node_count: i64,
    pub edge_count: i64,
    pub import_edge_count: i64,
    pub indexed_at: Option<String>,
    pub schema_version: Option<String>,
    pub language_distribution: Vec<(String, i64)>,
}

// Silence unused import warning for SCHEMA_VERSION (used in future migration checks)
const _: u32 = SCHEMA_VERSION;

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_store() -> Store {
        Store::open_in_memory().unwrap()
    }

    #[test]
    fn test_upsert_file() {
        let store = make_test_store();
        let rec = FileRecord {
            path: "src/foo.ts".into(),
            language: "typescript".into(),
            content_hash: vec![1, 2, 3, 4, 5, 6, 7, 8],
            mtime: 1234567890,
            size: 1024,
        };
        let id = store.upsert_file(&rec).unwrap();
        assert!(id > 0);

        // Upsert again updates
        let rec2 = FileRecord {
            path: "src/foo.ts".into(),
            language: "typescript".into(),
            content_hash: vec![8, 7, 6, 5, 4, 3, 2, 1],
            mtime: 1234567891,
            size: 2048,
        };
        let id2 = store.upsert_file(&rec2).unwrap();
        assert_eq!(id, id2); // same path → same id

        let hash = store.get_file_hash("src/foo.ts").unwrap().unwrap();
        assert_eq!(hash, vec![8, 7, 6, 5, 4, 3, 2, 1]);
    }

    #[test]
    fn test_insert_and_get_node() {
        let store = make_test_store();
        let file_id = store
            .upsert_file(&FileRecord {
                path: "src/bar.ts".into(),
                language: "typescript".into(),
                content_hash: vec![0; 8],
                mtime: 0,
                size: 0,
            })
            .unwrap();

        let node_id = store
            .insert_node(&NodeRecord {
                kind: "function".into(),
                name: "handleRequest".into(),
                qualified_name: "src/bar.handleRequest".into(),
                file_id,
                line_start: 10,
                line_end: 20,
                col_start: Some(0),
                signature: Some("(req: Request): Promise<Response>".into()),
                return_type: Some("Promise<Response>".into()),
                receiver_type: None,
                is_exported: true,
                complexity: 3,
                decorators: None,
                metadata: None,
            })
            .unwrap();

        let node = store.get_node(node_id).unwrap().unwrap();
        assert_eq!(node.name, "handleRequest");
        assert_eq!(node.kind, "function");
        assert!(node.is_exported);
    }

    #[test]
    fn test_insert_and_query_edges() {
        let store = make_test_store();
        let file_id = store
            .upsert_file(&FileRecord {
                path: "src/calls.ts".into(),
                language: "typescript".into(),
                content_hash: vec![0; 8],
                mtime: 0,
                size: 0,
            })
            .unwrap();

        let caller_id = store
            .insert_node(&NodeRecord {
                kind: "function".into(),
                name: "caller".into(),
                qualified_name: "src/calls.caller".into(),
                file_id,
                line_start: 1,
                line_end: 5,
                col_start: None,
                signature: None,
                return_type: None,
                receiver_type: None,
                is_exported: false,
                complexity: 0,
                decorators: None,
                metadata: None,
            })
            .unwrap();

        let callee_id = store
            .insert_node(&NodeRecord {
                kind: "function".into(),
                name: "callee".into(),
                qualified_name: "src/calls.callee".into(),
                file_id,
                line_start: 7,
                line_end: 10,
                col_start: None,
                signature: None,
                return_type: None,
                receiver_type: None,
                is_exported: false,
                complexity: 0,
                decorators: None,
                metadata: None,
            })
            .unwrap();

        store
            .insert_edge(&EdgeRecord {
                source_id: caller_id,
                target_id: Some(callee_id),
                edge_type: "CALLS".into(),
                confidence: 0.95,
                file_id: Some(file_id),
                line: Some(3),
                metadata: None,
            })
            .unwrap();

        let outgoing = store.get_outgoing_edges(caller_id).unwrap();
        assert_eq!(outgoing.len(), 1);
        assert_eq!(outgoing[0].target_id, Some(callee_id));
        assert!((outgoing[0].confidence - 0.95).abs() < 1e-9);

        let incoming = store.get_incoming_edges(callee_id).unwrap();
        assert_eq!(incoming.len(), 1);
        assert_eq!(incoming[0].source_id, caller_id);
    }

    #[test]
    fn test_edge_with_null_target() {
        // Unresolved call — target_id is NULL
        let store = make_test_store();
        let file_id = store
            .upsert_file(&FileRecord {
                path: "src/unresolved.ts".into(),
                language: "typescript".into(),
                content_hash: vec![0; 8],
                mtime: 0,
                size: 0,
            })
            .unwrap();

        let caller_id = store
            .insert_node(&NodeRecord {
                kind: "function".into(),
                name: "caller".into(),
                qualified_name: "src/unresolved.caller".into(),
                file_id,
                line_start: 1,
                line_end: 5,
                col_start: None,
                signature: None,
                return_type: None,
                receiver_type: None,
                is_exported: false,
                complexity: 0,
                decorators: None,
                metadata: None,
            })
            .unwrap();

        store
            .insert_edge(&EdgeRecord {
                source_id: caller_id,
                target_id: None, // unresolved
                edge_type: "CALLS".into(),
                confidence: 0.5,
                file_id: Some(file_id),
                line: Some(2),
                metadata: Some("raw_callee:externalFn".into()),
            })
            .unwrap();

        let outgoing = store.get_outgoing_edges(caller_id).unwrap();
        assert_eq!(outgoing.len(), 1);
        assert!(outgoing[0].target_id.is_none());
    }

    #[test]
    fn test_delete_file_symbols() {
        let store = make_test_store();
        let file_id = store
            .upsert_file(&FileRecord {
                path: "src/deleteme.ts".into(),
                language: "typescript".into(),
                content_hash: vec![0; 8],
                mtime: 0,
                size: 0,
            })
            .unwrap();

        store
            .insert_node(&NodeRecord {
                kind: "function".into(),
                name: "func1".into(),
                qualified_name: "src/deleteme.func1".into(),
                file_id,
                line_start: 1,
                line_end: 5,
                col_start: None,
                signature: None,
                return_type: None,
                receiver_type: None,
                is_exported: false,
                complexity: 0,
                decorators: None,
                metadata: None,
            })
            .unwrap();

        store
            .insert_node(&NodeRecord {
                kind: "function".into(),
                name: "func2".into(),
                qualified_name: "src/deleteme.func2".into(),
                file_id,
                line_start: 7,
                line_end: 10,
                col_start: None,
                signature: None,
                return_type: None,
                receiver_type: None,
                is_exported: false,
                complexity: 0,
                decorators: None,
                metadata: None,
            })
            .unwrap();

        let before = store.get_nodes_for_file(file_id).unwrap();
        assert_eq!(before.len(), 2);

        store.delete_file_symbols(file_id).unwrap();

        let after = store.get_nodes_for_file(file_id).unwrap();
        assert_eq!(after.len(), 0);
    }

    #[test]
    fn test_import_edges_and_reverse_deps() {
        let store = make_test_store();

        let file_a = store
            .upsert_file(&FileRecord {
                path: "src/a.ts".into(),
                language: "typescript".into(),
                content_hash: vec![0; 8],
                mtime: 0,
                size: 0,
            })
            .unwrap();
        let file_b = store
            .upsert_file(&FileRecord {
                path: "src/b.ts".into(),
                language: "typescript".into(),
                content_hash: vec![0; 8],
                mtime: 0,
                size: 0,
            })
            .unwrap();

        // b imports from a
        store
            .insert_import_edge(&ImportEdgeRecord {
                importer_file_id: file_b,
                imported_file_id: file_a,
            })
            .unwrap();

        let reverse_deps = store.get_reverse_deps(file_a).unwrap();
        assert_eq!(reverse_deps, vec![file_b]);

        // a has no reverse deps
        let empty = store.get_reverse_deps(file_b).unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn test_stats() {
        let store = make_test_store();
        store
            .upsert_file(&FileRecord {
                path: "src/x.ts".into(),
                language: "typescript".into(),
                content_hash: vec![0; 8],
                mtime: 0,
                size: 0,
            })
            .unwrap();
        store
            .upsert_file(&FileRecord {
                path: "src/y.py".into(),
                language: "python".into(),
                content_hash: vec![0; 8],
                mtime: 0,
                size: 0,
            })
            .unwrap();

        let stats = store.stats().unwrap();
        assert_eq!(stats.file_count, 2);
        assert_eq!(stats.node_count, 0);
        assert_eq!(stats.language_distribution.len(), 2);
    }

    #[test]
    fn test_batch_insert_nodes() {
        let store = make_test_store();
        let file_id = store
            .upsert_file(&FileRecord {
                path: "src/batch.ts".into(),
                language: "typescript".into(),
                content_hash: vec![0; 8],
                mtime: 0,
                size: 0,
            })
            .unwrap();

        let recs = (0..5)
            .map(|i| NodeRecord {
                kind: "function".into(),
                name: format!("func{}", i),
                qualified_name: format!("src/batch.func{}", i),
                file_id,
                line_start: i * 10,
                line_end: i * 10 + 5,
                col_start: None,
                signature: None,
                return_type: None,
                receiver_type: None,
                is_exported: false,
                complexity: 0,
                decorators: None,
                metadata: None,
            })
            .collect::<Vec<_>>();

        let ids = store.insert_nodes_batch(&recs).unwrap();
        assert_eq!(ids.len(), 5);

        let nodes = store.get_nodes_for_file(file_id).unwrap();
        assert_eq!(nodes.len(), 5);
    }

    #[test]
    fn test_transaction_atomic_commit() {
        let mut store = Store::open_in_memory().unwrap();
        let file_rec = FileRecord {
            path: "tx_test.ts".into(),
            language: "typescript".into(),
            content_hash: vec![0; 32],
            mtime: 1000,
            size: 42,
        };

        let txn = store.transaction().unwrap();
        let file_id = txn.upsert_file(&file_rec).unwrap();
        let node_recs = vec![NodeRecord {
            kind: "function".into(),
            name: "ftest".into(),
            qualified_name: "tx_test.ts.ftest".into(),
            file_id,
            line_start: 1,
            line_end: 5,
            col_start: None,
            signature: None,
            return_type: None,
            receiver_type: None,
            is_exported: true,
            complexity: 1,
            decorators: None,
            metadata: None,
        }];
        let ids = txn.insert_nodes_batch(&node_recs).unwrap();
        assert_eq!(ids.len(), 1);

        // Before commit, the data is visible within the tx
        let found = txn.find_node_by_qualified("tx_test.ts.ftest").unwrap();
        assert!(found.is_some());

        txn.commit().unwrap();

        // After commit, data is visible in the store
        let found = store.find_node_by_qualified("tx_test.ts.ftest").unwrap();
        assert!(found.is_some());
    }
}

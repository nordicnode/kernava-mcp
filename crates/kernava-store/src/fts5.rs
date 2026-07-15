/// FTS5 symbol search: tokenization, rebuild, insert, delete, query.
/// Application-layer camelCase/snake_case splitter runs before FTS5 insert
/// so "handleRequest" and "handle_request" both match substring "handle"
/// and exact "handleRequest" / "handle_request".
use rusqlite::{params, Connection};

use anyhow::Result;

use crate::NodeRow;

/// Split a symbol name into space-separated tokens for FTS5 indexing.
/// camelCase → "camel Case", snake_case → "snake case", PascalCase → "Pascal Case".
/// ponytail: app-layer tokenizer, not a custom FTS5 tokenizer module (avoids
/// C extension complexity). Insert the tokenized string into FTS5, query with
/// the same tokenizer on the search side.
pub fn tokenize_symbol_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 8);
    let chars: Vec<char> = name.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch.is_uppercase() {
            // Split before uppercase if previous char was lowercase
            if i > 0 && chars[i - 1].is_ascii_lowercase() {
                out.push(' ');
            }
            // Group consecutive uppercase chars as one token (acronym),
            // but if the NEXT char is lowercase, the acronym ends here
            // (e.g. "HTTPRequest" → "http request", not "h t t p request")
            let mut end = i + 1;
            while end < chars.len() && chars[end].is_uppercase() {
                end += 1;
            }
            if end < chars.len() && chars[end].is_ascii_lowercase() && end > i + 1 {
                // "HTTPRequest" — split "HTTP" then "Request"
                for c in &chars[i..end - 1] {
                    out.push(c.to_ascii_lowercase());
                }
                out.push(' ');
                out.push(chars[end - 1].to_ascii_lowercase());
            } else {
                for c in &chars[i..end] {
                    out.push(c.to_ascii_lowercase());
                }
            }
            i = end;
        } else if ch == '_' {
            out.push(' ');
            i += 1;
        } else {
            out.push(ch);
            i += 1;
        }
    }
    out
}
/// Rebuild the FTS5 index from the `nodes` table. Drops and re-inserts all rows.
pub fn rebuild_fts(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM fts5_symbols", [])?;
    let mut stmt =
        conn.prepare("SELECT id, name, qualified_name, signature FROM nodes ORDER BY id")?;
    let rows: Vec<(i64, String, String, Option<String>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);

    let tx = conn.unchecked_transaction()?;
    for (id, name, qname, sig) in &rows {
        let tokenized = tokenize_symbol_name(name);
        tx.execute(
            "INSERT INTO fts5_symbols (rowid, name, qualified_name, signature)
             VALUES (?1, ?2, ?3, ?4)",
            params![id, tokenized, qname, sig.as_deref().unwrap_or("")],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Insert a single node into FTS5. Call after inserting into `nodes`.
pub fn insert_fts(
    conn: &Connection,
    node_id: i64,
    name: &str,
    qualified_name: &str,
    signature: Option<&str>,
) -> Result<()> {
    let tokenized = tokenize_symbol_name(name);
    conn.execute(
        "INSERT INTO fts5_symbols (rowid, name, qualified_name, signature)
         VALUES (?1, ?2, ?3, ?4)",
        params![node_id, tokenized, qualified_name, signature.unwrap_or("")],
    )?;
    Ok(())
}

/// Delete a single node from FTS5 by rowid (matches node id).
pub fn delete_fts(conn: &Connection, node_id: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM fts5_symbols WHERE rowid = ?1",
        params![node_id],
    )?;
    Ok(())
}

/// Delete all FTS5 entries for a file (by node rowids).
pub fn delete_fts_for_file(conn: &Connection, file_id: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM fts5_symbols WHERE rowid IN (SELECT id FROM nodes WHERE file_id = ?1)",
        params![file_id],
    )?;
    Ok(())
}

/// Search symbols by FTS5 query. Returns matching NodeRows.
/// ponytail: uses MATCH with the tokenized query. Caller should tokenize
/// the search term the same way (tokenize_symbol_name) before passing.
pub fn search_symbols(conn: &Connection, query: &str, limit: i64) -> Result<Vec<NodeRow>> {
    // Tokenize the query for FTS5 MATCH
    let tokenized = tokenize_symbol_name(query);
    if tokenized.is_empty() {
        // Fall back to LIKE for single-char or empty queries
        let pattern = format!("%{}%", query);
        let mut stmt = conn.prepare(
            "SELECT n.id, n.kind, n.name, n.qualified_name, n.file_id, n.line_start, n.line_end,
                n.col_start, n.signature, n.return_type, n.receiver_type, n.is_exported,
                n.complexity, n.decorators, n.metadata
             FROM nodes n
             WHERE n.name LIKE ?1
             ORDER BY n.is_exported DESC, n.name
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![pattern, limit], |row| {
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
                is_exported: row.get::<_, i64>(11)? != 0,
                complexity: row.get(12)?,
                decorators: row.get(13)?,
                metadata: row.get(14)?,
            })
        })?;
        return rows.collect::<Result<Vec<_>, _>>().map_err(Into::into);
    }

    let fts_query = format!("{}*", tokenized);
    let mut stmt = conn.prepare(
        "SELECT n.id, n.kind, n.name, n.qualified_name, n.file_id, n.line_start, n.line_end,
            n.col_start, n.signature, n.return_type, n.receiver_type, n.is_exported,
            n.complexity, n.decorators, n.metadata
         FROM fts5_symbols
         JOIN nodes n ON n.id = fts5_symbols.rowid
         WHERE fts5_symbols MATCH ?1
         ORDER BY rank, n.is_exported DESC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![fts_query, limit], |row| {
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
            is_exported: row.get::<_, i64>(11)? != 0,
            complexity: row.get(12)?,
            decorators: row.get(13)?,
            metadata: row.get(14)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tokenize_camel_case() {
        assert_eq!(tokenize_symbol_name("handleRequest"), "handle request");
        assert_eq!(
            tokenize_symbol_name("handleRequestDTO"),
            "handle request dto"
        );
    }

    #[test]
    fn test_tokenize_snake_case() {
        assert_eq!(tokenize_symbol_name("process_request"), "process request");
        assert_eq!(tokenize_symbol_name("_private_func"), " private func");
    }

    #[test]
    fn test_tokenize_pascal_case() {
        assert_eq!(tokenize_symbol_name("MyClass"), "my class");
    }

    #[test]
    fn test_tokenize_mixed() {
        assert_eq!(tokenize_symbol_name("myVariableName"), "my variable name");
        assert_eq!(tokenize_symbol_name("HTTPServer"), "http server");
    }

    #[test]
    fn test_tokenize_simple() {
        assert_eq!(tokenize_symbol_name("add"), "add");
        assert_eq!(tokenize_symbol_name("main"), "main");
    }

    #[test]
    fn test_search_symbols_match_branch() {
        use crate::{NodeRecord, Store};

        let mut store = Store::open_in_memory().unwrap();

        // Insert a file + node so FTS5 has rows
        store
            .upsert_file(&crate::FileRecord {
                path: "test.ts".into(),
                language: "typescript".into(),
                content_hash: vec![0; 16],
                mtime: 0,
                size: 42,
            })
            .unwrap();
        let file_id = store.get_file_id("test.ts").unwrap().unwrap();

        let txn = store.transaction().unwrap();
        txn.insert_nodes_batch(&[NodeRecord {
            kind: "function".into(),
            name: "handleRequest".into(),
            qualified_name: "test.ts.handleRequest".into(),
            file_id,
            line_start: 1,
            line_end: 5,
            col_start: Some(0),
            signature: Some("(req)".into()),
            return_type: None,
            receiver_type: None,
            is_exported: true,
            complexity: 1,
            decorators: None,
            metadata: None,
        }])
        .unwrap();
        txn.commit().unwrap();

        // Search "handle" — hits FTS5 MATCH branch (non-empty tokenized)
        let results = search_symbols(store.conn(), "handle", 10).unwrap();
        assert_eq!(results.len(), 1, "should find handleRequest via FTS5 MATCH");
        assert_eq!(results[0].name, "handleRequest");
    }

    #[test]
    fn test_search_symbols_cross_style_match() {
        use crate::{NodeRecord, Store};

        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_file(&crate::FileRecord {
                path: "api.ts".into(),
                language: "typescript".into(),
                content_hash: vec![0; 16],
                mtime: 0,
                size: 42,
            })
            .unwrap();
        let file_id = store.get_file_id("api.ts").unwrap().unwrap();

        let txn = store.transaction().unwrap();
        // Insert camelCase symbol
        txn.insert_nodes_batch(&[NodeRecord {
            kind: "function".into(),
            name: "handleRequest".into(),
            qualified_name: "api.ts.handleRequest".into(),
            file_id,
            line_start: 1,
            line_end: 5,
            col_start: Some(0),
            signature: None,
            return_type: None,
            receiver_type: None,
            is_exported: true,
            complexity: 1,
            decorators: None,
            metadata: None,
        }])
        .unwrap();
        txn.commit().unwrap();

        // Query with snake_case should match camelCase symbol
        let results = search_symbols(store.conn(), "handle_request", 10).unwrap();
        assert_eq!(
            results.len(),
            1,
            "snake_case query should match camelCase symbol"
        );
        assert_eq!(results[0].name, "handleRequest");

        // Query with camelCase should also match
        let results = search_symbols(store.conn(), "handleRequest", 10).unwrap();
        assert_eq!(
            results.len(),
            1,
            "camelCase query should match camelCase symbol"
        );
        assert_eq!(results[0].name, "handleRequest");
    }
}

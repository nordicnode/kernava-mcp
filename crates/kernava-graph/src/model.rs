// kernava-graph: graph model types
// P2 task 2.1: Node, Edge, NodeId — the building blocks of the in-RAM graph cache.

use kernava_store::{EdgeRow, NodeRow};

/// Unique node identifier. Maps 1:1 to `nodes.id` in SQLite.
pub type NodeId = i64;

/// Unique file identifier. Maps 1:1 to `files.id` in SQLite.
pub type FileId = i64;

/// A node in the graph — a symbol (function, method, class, etc.).
/// Strips `decorators` and `metadata` from `NodeRow` — unused by traversal algorithms.
#[derive(Debug, Clone)]
pub struct Node {
    pub id: NodeId,
    pub kind: String,
    pub name: String,
    pub qualified_name: String,
    pub file_id: FileId,
    pub line_start: i32,
    pub line_end: i32,
    pub signature: Option<String>,
    pub return_type: Option<String>,
    pub receiver_type: Option<String>,
    pub is_exported: bool,
    pub complexity: i32,
}

impl From<NodeRow> for Node {
    fn from(r: NodeRow) -> Self {
        Self {
            id: r.id,
            kind: r.kind,
            name: r.name,
            qualified_name: r.qualified_name,
            file_id: r.file_id,
            line_start: r.line_start,
            line_end: r.line_end,
            signature: r.signature,
            return_type: r.return_type,
            receiver_type: r.receiver_type,
            is_exported: r.is_exported,
            complexity: r.complexity,
        }
    }
}

/// A directed edge between two nodes. `edge_type` stored as raw string
/// — promote to enum when a second edge type appears in builder.
#[derive(Debug, Clone)]
pub struct Edge {
    pub id: i64,
    pub source: NodeId,
    pub target: Option<NodeId>,
    pub edge_type: String,
    pub confidence: f64,
    pub file_id: Option<FileId>,
    pub line: Option<i32>,
}

impl From<EdgeRow> for Edge {
    fn from(r: EdgeRow) -> Self {
        Self {
            id: r.id,
            source: r.source_id,
            target: r.target_id,
            edge_type: r.edge_type,
            confidence: r.confidence,
            file_id: r.file_id,
            line: r.line,
        }
    }
}

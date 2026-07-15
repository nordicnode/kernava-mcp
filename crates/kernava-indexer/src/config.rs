// kernava-indexer: indexer configuration

use serde::{Deserialize, Serialize};

/// Configuration for the indexer.
/// Loaded from `kernava.toml` at the project root by the server crate.
/// Indexer itself is config-source-agnostic — it just receives this struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexerConfig {
    /// Maximum file size in bytes. Files larger than this are skipped.
    /// Default: 1 MiB (1_048_576).
    pub max_file_size: usize,

    /// Additional glob patterns to ignore (beyond .gitignore).
    /// e.g. ["**/generated/**", "**/*.pb.go"]
    pub ignore: Vec<String>,

    /// Whether to follow symbolic links during file discovery.
    /// Default: false — symlinks can cause cycles and index duplicate files.
    #[serde(default)]
    pub follow_symlinks: bool,
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            max_file_size: 1_048_576, // 1 MiB
            ignore: Vec::new(),
            follow_symlinks: false,
        }
    }
}

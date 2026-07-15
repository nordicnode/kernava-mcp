// kernava-indexer: tree-sitter parsing, symbol extraction, call resolution, incremental indexing

pub mod builder;
pub mod extractor;
pub mod languages;
pub mod parser;
pub mod resolver;
pub mod watcher;

#[cfg(test)]
mod ast_dump_tests;

pub use extractor::{extract, CallSite, ExtractionResult, SymbolDef, SymbolKind};
pub use languages::ModuleMap;
pub use parser::{parse, Language};
pub use resolver::{resolve_calls, FunctionRegistry, ResolutionStrategy, ResolvedCall};

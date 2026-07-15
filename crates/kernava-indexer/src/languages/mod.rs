// Language-specific import parsers
pub mod c;
pub mod cpp;
pub mod csharp;
pub mod go;
pub mod java;
pub mod php;
pub mod python;
pub mod ruby;
pub mod rust;
pub mod ts;
pub use ts::ModuleMap;

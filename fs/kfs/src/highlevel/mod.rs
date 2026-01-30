//! High-level filesystem APIs (std-like wrappers).
mod file;
mod fs;

pub use file::*;
// Re-export the wrapper FsContext for backward compatibility
pub use fs::{FS_CONTEXT, FsContext, ROOT_FS_CONTEXT, ReadDir, ReadDirEntry};

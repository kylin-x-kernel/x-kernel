// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! High-level filesystem APIs (std-like wrappers).
mod file;
mod fs;

pub use file::*;
// Re-export the wrapper FsContext for backward compatibility
pub use fs::{
    FsContext, KERNEL_FS_CONTEXT, ROOT_FS_CONTEXT, ReadDir, ReadDirEntry, kernel_fs_context,
    new_process_fs_context,
};

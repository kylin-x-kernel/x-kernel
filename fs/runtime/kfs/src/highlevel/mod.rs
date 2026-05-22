// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! High-level filesystem APIs (std-like wrappers).
mod directory;
mod file;
mod fs;

use alloc::{borrow::Cow, string::ToString};

pub use directory::*;
pub use file::*;
// Re-export the wrapper FsContext for backward compatibility
pub use fs::{
    FsContext, KERNEL_FS_CONTEXT, ROOT_FS_CONTEXT, ReadDir, ReadDirEntry, kernel_fs_context,
    new_process_fs_context,
};
use kvfs::Location;

pub(crate) fn path_for(loc: &Location) -> Cow<'static, str> {
    loc.absolute_path()
        .map_or_else(|_| "<error>".into(), |f| Cow::Owned(f.to_string()))
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! 9P-backed open-directory iteration.

use kvfs::{DirContext, FileDirOperations, VfsFile, VfsResult};

use super::{
    inode::{V9fsInode, qid_to_inode_number},
    util::dtype_to_vfs,
};

impl FileDirOperations for V9fsInode {
    fn iterate_shared(&self, file: &VfsFile, ctx: &mut DirContext<'_>) -> VfsResult<usize> {
        let entries = Self::file_fid(file)?.read_dir(ctx.pos())?;
        let mut count = 0;
        for entry in entries {
            if !ctx.emit(
                &entry.name,
                qid_to_inode_number(entry.qid),
                dtype_to_vfs(entry.entry_type),
                entry.offset,
            ) {
                break;
            }
            count += 1;
        }
        Ok(count)
    }
}

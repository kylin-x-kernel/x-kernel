// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::String, vec::Vec};

use ktask::KtaskRef;
use kvfs::VfsResult;

#[derive(Clone, Copy)]
pub struct ProcFsHooks {
    pub irq_count: fn() -> usize,
    pub fd_ids: fn(&KtaskRef) -> Vec<u32>,
    pub fd_path: fn(&KtaskRef, u32) -> VfsResult<String>,
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kvfs::{DirMaker, DirMapping, SimpleFs};

use super::dir::new_root_inode;

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    let maker: DirMaker = Arc::new(move || new_root_inode(fs.clone()));
    root.add("irq", maker);
}

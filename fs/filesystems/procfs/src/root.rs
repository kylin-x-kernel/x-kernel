// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kvfs_simple::{DirMaker, DirMapping, SimpleDir, SimpleDirOps, SimpleFs};

use crate::{basic_nodes, device_nodes, irq_nodes, mem_nodes, task_nodes, trace_nodes};

pub fn builder(fs: Arc<SimpleFs>) -> DirMaker {
    let mut root = DirMapping::new();

    basic_nodes::root::add_root_entries(&mut root, fs.clone());
    device_nodes::root::add_root_entries(&mut root, fs.clone());
    irq_nodes::root::add_root_entries(&mut root, fs.clone());
    mem_nodes::root::add_root_entries(&mut root, fs.clone());
    task_nodes::mounts::add_root_entries(&mut root, fs.clone());
    trace_nodes::root::add_root_entries(&mut root, fs.clone());
    #[cfg(feature = "sysrq")]
    crate::sysrq_nodes::root::add_root_entries(&mut root, fs.clone());

    let dynamic_dirs = task_nodes::root::ProcFsHandler::new(fs.clone()).chain(root);
    SimpleDir::new_maker(fs, Arc::new(dynamic_dirs))
}

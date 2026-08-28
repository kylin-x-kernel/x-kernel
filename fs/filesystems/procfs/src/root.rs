// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kvfs::{DirMaker, DirMapping, NodeType, SimpleDir, SimpleDirOps, SimpleFile, SimpleFs};

use crate::{basic_nodes, device_nodes, irq_nodes, mem_nodes, task_nodes, trace_nodes};

pub fn builder(fs: Arc<SimpleFs>) -> DirMaker {
    let mut root = DirMapping::new();

    basic_nodes::root::add_root_entries(&mut root, fs.clone());
    device_nodes::root::add_root_entries(&mut root, fs.clone());
    irq_nodes::root::add_root_entries(&mut root, fs.clone());
    mem_nodes::root::add_root_entries(&mut root, fs.clone());
    root.add(
        "mounts",
        SimpleFile::new(fs.clone(), NodeType::Symlink, || Ok("self/mounts")),
    );
    trace_nodes::root::add_root_entries(&mut root, fs.clone());
    #[cfg(feature = "lock_stat")]
    root.add(
        "lock_stat",
        SimpleFile::new_regular(fs.clone(), || Ok(klockstat::dump_lock_stat())),
    );
    #[cfg(feature = "sched_stat")]
    root.add(
        "sched_stat",
        SimpleFile::new_regular(fs.clone(), || Ok(ktask::sched_stats_text())),
    );
    #[cfg(feature = "vmm")]
    root.add(
        "kvmm",
        SimpleFile::new_regular(fs.clone(), || Ok(kvmm::dump_vm_info())),
    );
    #[cfg(feature = "sysrq")]
    crate::sysrq_nodes::root::add_root_entries(&mut root, fs.clone());
    #[cfg(feature = "kwork_stress")]
    crate::kwork_stress_nodes::root::add_root_entries(&mut root, fs.clone());

    let dynamic_dirs = task_nodes::root::ProcFsHandler::new(fs.clone()).chain(root);
    SimpleDir::new_maker(fs, Arc::new(dynamic_dirs))
}

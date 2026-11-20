// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kvfs::{DirMaker, DirMapping, NodeType, SimpleDir, SimpleDirOps, SimpleFile, SimpleFs};

use crate::{basic_nodes, device_nodes, irq_nodes, mem_nodes, task_nodes, trace_nodes};

pub(crate) fn builder_for(
    observer_ns: Option<alloc::sync::Arc<kprocess::PidNamespace>>,
) -> impl FnOnce(Arc<SimpleFs>) -> DirMaker {
    move |fs| builder_with(fs, observer_ns)
}

fn builder_with(
    fs: Arc<SimpleFs>,
    observer_ns: Option<alloc::sync::Arc<kprocess::PidNamespace>>,
) -> DirMaker {
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
    #[cfg(feature = "net")]
    crate::net_nodes::root::add_root_entries(&mut root, fs.clone());
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

    root.add("sys", {
        let mut sys = DirMapping::new();
        basic_nodes::root::add_sys_entries(&mut sys, fs.clone());
        #[cfg(feature = "net")]
        crate::net_nodes::root::add_sys_entries(&mut sys, fs.clone());
        SimpleDir::new_maker(fs.clone(), Arc::new(sys))
    });
    root.add(
        "config.gz",
        SimpleFile::new_regular(fs.clone(), || Ok(kbuild_config::CONFIG_GZIP.to_vec())),
    );
    let dynamic_dirs = task_nodes::root::ProcFsHandler::new(fs.clone(), observer_ns).chain(root);
    SimpleDir::new_maker(fs, Arc::new(dynamic_dirs))
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kvfs::{DirMaker, DirMapping, SimpleDir, SimpleFs};

use crate::nodes;

pub fn builder(fs: Arc<SimpleFs>) -> DirMaker {
    let mut root = DirMapping::new();

    nodes::null_zero_full::add_root_entries(&mut root, fs.clone());
    nodes::random::add_root_entries(&mut root, fs.clone());
    nodes::cpu_dma_latency::add_root_entries(&mut root, fs.clone());
    nodes::rtc::add_root_entries(&mut root, fs.clone());
    nodes::fb::add_root_entries(&mut root, fs.clone());
    nodes::dri::add_root_entries(&mut root, fs.clone());
    nodes::tty_nodes::add_root_entries(&mut root, fs.clone());
    nodes::r#loop::add_root_entries(&mut root, fs.clone());
    nodes::shm::add_root_entries(&mut root, fs.clone());
    nodes::dtb::add_root_entries(&mut root, fs.clone());

    #[cfg(feature = "dev-log")]
    nodes::log::add_root_entries(&mut root, fs.clone());
    #[cfg(feature = "memtrack")]
    nodes::memtrack::add_root_entries(&mut root, fs.clone());
    #[cfg(feature = "input")]
    nodes::event::add_root_entries(&mut root, fs.clone());
    #[cfg(all(feature = "dice", target_os = "none"))]
    nodes::dice::add_root_entries(&mut root, fs.clone());

    SimpleDir::new_maker(fs, Arc::new(root))
}

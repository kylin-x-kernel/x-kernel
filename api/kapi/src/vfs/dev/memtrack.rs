// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{collections::btree_map::BTreeMap, vec::Vec};
use core::{
    alloc::Layout,
    any::Any,
    cmp, fmt,
    sync::atomic::{AtomicU64, Ordering},
};

use backtrace::Backtrace;
use fs_ng_vfs::{NodeFlags, VfsResult};
use kcore::{
    mm::clear_elf_cache,
    task::{cleanup_task_tables, tasks},
};

use crate::vfs::DeviceOps;

static STAMPED_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Memory allocation category based on backtrace analysis
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum MemoryCategory {
    Known(&'static str),
    Unknown(Backtrace),
}

impl MemoryCategory {
    /// Create a new memory category from a backtrace
    fn new(backtrace: &Backtrace) -> Self {
        match Self::category(backtrace) {
            Some(category) => Self::Known(category),
            None => Self::Unknown(backtrace.clone()),
        }
    }

    /// Identify known allocation categories from backtrace frames
    fn category(backtrace: &Backtrace) -> Option<&'static str> {
        for (_, frame) in backtrace.frames()? {
            let Some(func) = frame.function else {
                continue;
            };
            if func.language != Some(gimli::DW_LANG_Rust) {
                continue;
            }
            let Ok(name) = func.demangle() else {
                continue;
            };
            match name.as_ref() {
                "kcore::mm::ElfLoader::load" => {
                    return Some("elf cache");
                }
                "kcore::task::ProcessData::new" => {
                    return Some("process data");
                }
                "kprocess::process::Process::new" => {
                    return Some("process");
                }
                "kprocess::process_group::ProcessGroup::new" => {
                    return Some("process group");
                }
                "kfs::fs::ext4::inode::Inode::new" => {
                    return Some("ext4 inode");
                }
                "kfs::highlevel::file::CachedFile::get_or_create"
                | "kfs::highlevel::file::CachedFile::page_or_insert" => {
                    return Some("cached file");
                }
                "ktask::timers::set_alarm_wakeup" => {
                    return Some("timer");
                }
                "fs_ng_vfs::node::dir::DirNode::lookup_locked"
                | "fs_ng_vfs::node::dir::DirNode::create_locked" => {
                    return Some("dentry");
                }
                "ext4_user_malloc" => {
                    return Some("lwext4");
                }
                _ => continue,
            }
        }

        None
    }
}

impl fmt::Display for MemoryCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemoryCategory::Known(name) => write!(f, "[{name}]"),
            MemoryCategory::Unknown(backtrace) => write!(f, "{backtrace}"),
        }
    }
}

/// Analyze memory allocations between stamped generations and print results
fn run_memory_analysis() {
    // Wait for gc
    ktask::yield_now();
    cleanup_task_tables();
    clear_elf_cache();

    kprintln!(
        "Alive tasks: {:?}",
        tasks().iter().map(|it| it.id_name()).collect::<Vec<_>>()
    );

    let from = STAMPED_GENERATION.load(Ordering::SeqCst);
    let to = kalloc::current_generation();

    let mut allocations: BTreeMap<MemoryCategory, Vec<Layout>> = BTreeMap::new();
    kalloc::allocations_in(from..to, |info| {
        let category = MemoryCategory::new(&info.backtrace);
        allocations.entry(category).or_default().push(info.layout);
    });
    let mut allocations = allocations
        .into_iter()
        .map(|(category, layouts)| {
            let total_size = layouts.iter().map(|l| l.size()).sum::<usize>();
            (category, layouts, total_size)
        })
        .collect::<Vec<_>>();
    allocations.sort_by_key(|it| cmp::Reverse(it.2));
    if !allocations.is_empty() {
        kprintln!("===========================");
        kprintln!("Memory usage:");
        for (category, layouts, total_size) in allocations {
            kprintln!(
                " {} bytes, {} allocations, {:?}, {category}",
                total_size,
                layouts.len(),
                layouts[0],
            );
        }
        kprintln!("==========================");
    }
}

/// Memory tracking device for allocation profiling (/dev/memtrack)
pub(crate) struct MemTrack;

impl DeviceOps for MemTrack {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Ok(buf.len())
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        if offset == 0 && !buf.is_empty() {
            match buf {
                b"start\n" => {
                    let generation = kalloc::current_generation();
                    STAMPED_GENERATION.store(generation, Ordering::SeqCst);
                    kprintln!("Memory allocation generation stamped: {}", generation);
                    kalloc::enable_tracking();
                }
                b"end\n" => {
                    run_memory_analysis();
                    kalloc::disable_tracking();
                }
                _ => {}
            }
        }
        Ok(buf.len())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }
}

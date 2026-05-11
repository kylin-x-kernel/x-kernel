// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{format, string::String, sync::Arc, vec, vec::Vec};

use kalloc::UsageKind;
use kcore::vfs::{
    DirMaker, DirMapping, SeqFileNode, SeqIterator, SimpleDir, SimpleDirOps, SimpleFile, SimpleFs,
};

use crate::{
    hooks::ProcFsHooks, mounts::ProcMountIter, task::ProcFsHandler, tracing::tracing_dir_maker,
};

const KB: usize = 1024;
const PAGE_SIZE: usize = 0x1000;

fn render_interrupts(irq_count: usize) -> String {
    format!("0: {}\n", irq_count)
}

fn bytes_to_kib(bytes: usize) -> usize {
    bytes / KB
}

struct MeminfoIter {
    lines: Vec<String>,
    next_index: usize,
}

struct InterruptsIter {
    hooks: ProcFsHooks,
    emitted: bool,
}

impl InterruptsIter {
    fn new(hooks: ProcFsHooks) -> Self {
        Self {
            hooks,
            emitted: false,
        }
    }
}

impl MeminfoIter {
    fn new() -> Self {
        let mut iter = Self {
            lines: Vec::new(),
            next_index: 0,
        };
        iter.rewind();
        iter
    }
}

impl SeqIterator for MeminfoIter {
    type Item = String;

    fn rewind(&mut self) {
        let allocator = kalloc::global_allocator();
        let usages = allocator.usages();
        let used_pages = allocator.used_pages();
        let free_pages = allocator.available_pages();
        let total_pages = used_pages + free_pages;
        let total_kib = total_pages * PAGE_SIZE / KB;
        let free_kib = free_pages * PAGE_SIZE / KB;
        let cache_kib = bytes_to_kib(usages.get(UsageKind::PageCache));
        let heap_kib = bytes_to_kib(usages.get(UsageKind::RustHeap));
        let page_table_kib = bytes_to_kib(usages.get(UsageKind::PageTable));
        let user_kib = bytes_to_kib(usages.get(UsageKind::VirtMem));
        let dma_kib = bytes_to_kib(usages.get(UsageKind::Dma));
        let available_kib = free_kib + cache_kib;

        self.lines = vec![
            format!("MemTotal:{total_kib:>15} kB\n"),
            format!("MemFree:{free_kib:>16} kB\n"),
            format!("MemAvailable:{available_kib:>11} kB\n"),
            format!("Buffers:{:>16} kB\n", 0),
            format!("Cached:{cache_kib:>17} kB\n"),
            format!("SwapCached:{:>13} kB\n", 0),
            format!("Active:{:>18} kB\n", 0),
            format!("Inactive:{:>16} kB\n", 0),
            format!("SwapTotal:{:>14} kB\n", 0),
            format!("SwapFree:{:>15} kB\n", 0),
            format!("Dirty:{:>19} kB\n", 0),
            format!("Writeback:{:>15} kB\n", 0),
            format!("AnonPages:{user_kib:>15} kB\n"),
            format!("Mapped:{user_kib:>17} kB\n"),
            format!("Shmem:{:>18} kB\n", 0),
            format!("Slab:{heap_kib:>19} kB\n"),
            format!("SReclaimable:{:>11} kB\n", 0),
            format!("SUnreclaim:{heap_kib:>13} kB\n"),
            format!("PageTables:{page_table_kib:>13} kB\n"),
            format!("KernelStack:{:>12} kB\n", 0),
            format!("NFS_Unstable:{:>11} kB\n", 0),
            format!("Bounce:{:>17} kB\n", 0),
            format!("CmaTotal:{dma_kib:>14} kB\n"),
            format!("HugePages_Total:{:>8}\n", 0),
            format!("HugePages_Free:{:>9}\n", 0),
            format!("Hugepagesize:{:>11} kB\n", 2048),
            format!("DirectMap4k:{total_kib:>13} kB\n"),
            format!("DirectMap2M:{:>13} kB\n", 0),
        ];
        self.next_index = 0;
    }

    fn start(&mut self) -> Option<Self::Item> {
        self.rewind();
        self.next()
    }

    fn next(&mut self) -> Option<Self::Item> {
        let item = self.lines.get(self.next_index).cloned();
        if item.is_some() {
            self.next_index += 1;
        }
        item
    }

    fn show(&self, item: &Self::Item, buf: &mut String) -> core::fmt::Result {
        buf.push_str(item);
        Ok(())
    }
}

impl SeqIterator for InterruptsIter {
    type Item = usize;

    fn rewind(&mut self) {
        self.emitted = false;
    }

    fn start(&mut self) -> Option<Self::Item> {
        self.rewind();
        self.next()
    }

    fn next(&mut self) -> Option<Self::Item> {
        if self.emitted {
            return None;
        }
        self.emitted = true;
        Some((self.hooks.irq_count)())
    }

    fn show(&self, item: &Self::Item, buf: &mut String) -> core::fmt::Result {
        buf.push_str(&render_interrupts(*item));
        Ok(())
    }
}

pub fn builder(fs: Arc<SimpleFs>, hooks: ProcFsHooks) -> DirMaker {
    let mut root = DirMapping::new();
    root.add(
        "cmdline",
        SimpleFile::new_regular(fs.clone(), || {
            Ok(match khal::cmdline() {
                Some(cmdline) if !cmdline.is_empty() => format!("{cmdline}\n"),
                _ => String::from("\n"),
            })
        }),
    );
    root.add(
        "mounts",
        SeqFileNode::new_regular(fs.clone(), ProcMountIter::mounts()),
    );
    root.add(
        "meminfo",
        SeqFileNode::new_regular(fs.clone(), MeminfoIter::new()),
    );
    root.add(
        "meminfo2",
        SimpleFile::new_regular(fs.clone(), || {
            let allocator = kalloc::global_allocator();
            Ok(format!("{:?}\n", allocator.usages()))
        }),
    );
    root.add(
        "instret",
        SimpleFile::new_regular(fs.clone(), || {
            #[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))]
            {
                Ok(format!("{}\n", riscv::register::instret::read64()))
            }
            #[cfg(not(any(target_arch = "riscv32", target_arch = "riscv64")))]
            {
                Ok(String::from("0\n"))
            }
        }),
    );
    root.add(
        "interrupts",
        SeqFileNode::new_regular(fs.clone(), InterruptsIter::new(hooks)),
    );
    root.add("tracing", tracing_dir_maker(fs.clone()));
    root.add("sys", {
        let mut sys = DirMapping::new();

        sys.add("kernel", {
            let mut kernel = DirMapping::new();
            kernel.add(
                "pid_max",
                SimpleFile::new_regular(fs.clone(), || Ok("32768\n")),
            );
            SimpleDir::new_maker(fs.clone(), Arc::new(kernel))
        });

        SimpleDir::new_maker(fs.clone(), Arc::new(sys))
    });

    let proc_dir = ProcFsHandler::new(fs.clone(), hooks);
    SimpleDir::new_maker(fs, Arc::new(proc_dir.chain(root)))
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::{bytes_to_kib, render_interrupts};

    #[def_test]
    fn test_render_interrupts_includes_trailing_newline() {
        assert_eq!(render_interrupts(867), "0: 867\n");
    }

    #[def_test]
    fn test_bytes_to_kib_rounds_down() {
        assert_eq!(bytes_to_kib(4097), 4);
    }
}

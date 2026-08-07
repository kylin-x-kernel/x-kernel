// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{format, string::String, sync::Arc, vec, vec::Vec};

use kalloc::UsageKind;
use kvfs::{DirMapping, SeqFileInode, SeqIterator, SimpleFile, SimpleFs};
use memaddr::{KB, PAGE_SIZE_4K};

fn bytes_to_kib(bytes: usize) -> usize {
    bytes / KB
}

struct MeminfoIter {
    lines: Vec<String>,
    next_index: usize,
}

impl MeminfoIter {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            next_index: 0,
        }
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
        let total_kib = total_pages * PAGE_SIZE_4K / KB;
        let free_kib = free_pages * PAGE_SIZE_4K / KB;
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

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    root.add(
        "meminfo",
        SeqFileInode::new_regular(fs.clone(), || Ok(MeminfoIter::new())),
    );
    root.add(
        "meminfo2",
        SimpleFile::new_regular(fs.clone(), || {
            let allocator = kalloc::global_allocator();
            Ok(format!("{:?}\n", allocator.usages()))
        }),
    );
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::bytes_to_kib;

    #[def_test]
    fn test_bytes_to_kib_rounds_down() {
        assert_eq!(bytes_to_kib(4097), 4);
    }
}

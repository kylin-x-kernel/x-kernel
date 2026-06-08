// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Heap management syscalls.

use kaddr_layout::{USER_HEAP_BASE, USER_HEAP_SIZE, USER_HEAP_SIZE_MAX};
use kerrno::KResult;
use khal::paging::{MappingFlags, PageSize};
use kthread::current_process_state;
use memaddr::{VirtAddr, align_up_4k};
use memspace_file::new_alloc;

#[derive(Clone, Copy)]
struct HeapLayout {
    base: usize,
    initial_end: usize,
    limit: usize,
}

impl HeapLayout {
    fn current() -> Self {
        Self {
            base: USER_HEAP_BASE,
            initial_end: USER_HEAP_BASE + USER_HEAP_SIZE,
            limit: USER_HEAP_BASE + USER_HEAP_SIZE_MAX,
        }
    }

    fn contains_brk(self, addr: usize) -> bool {
        (self.base..=self.limit).contains(&addr)
    }

    fn expand_range(self, current_top: usize, new_top: usize) -> Option<(VirtAddr, usize)> {
        let start = self.initial_end.max(align_up_4k(current_top));
        let end = align_up_4k(new_top);
        (end > start).then(|| (VirtAddr::from(start), end - start))
    }

    fn shrink_range(self, current_top: usize, new_top: usize) -> Option<(VirtAddr, usize)> {
        let start = self.initial_end.max(align_up_4k(new_top));
        let end = align_up_4k(current_top);
        (end > start).then(|| (VirtAddr::from(start), end - start))
    }
}

pub fn sys_brk(addr: usize) -> KResult<isize> {
    let proc_state = current_process_state();
    let current_top = proc_state.heap_top();
    let heap_layout = HeapLayout::current();

    if addr == 0 {
        return Ok(current_top as isize);
    }

    if !heap_layout.contains_brk(addr) {
        return Ok(current_top as isize);
    }

    if let Some((expand_start, expand_size)) = heap_layout.expand_range(current_top, addr) {
        if proc_state
            .address_space()
            .lock()
            .map(
                expand_start,
                expand_size,
                MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER,
                false,
                new_alloc(expand_start, PageSize::Size4K),
            )
            .is_err()
        {
            return Ok(current_top as isize);
        }
    } else if let Some((shrink_start, shrink_size)) = heap_layout.shrink_range(current_top, addr)
        && proc_state
            .address_space()
            .lock()
            .unmap(shrink_start, shrink_size)
            .is_err()
    {
        return Ok(current_top as isize);
    }

    proc_state.set_heap_top(addr);
    Ok(addr as isize)
}

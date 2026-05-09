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

pub fn sys_brk(addr: usize) -> KResult<isize> {
    let proc_state = current_process_state();
    let current_top = proc_state.heap_top();
    let heap_limit = USER_HEAP_BASE + USER_HEAP_SIZE_MAX;

    if addr == 0 {
        return Ok(current_top as isize);
    }

    if addr < USER_HEAP_BASE || addr > heap_limit {
        return Ok(current_top as isize);
    }

    let new_top_aligned = align_up_4k(addr);
    let current_top_aligned = align_up_4k(current_top);
    // Initial heap region end address (already mapped during ELF loading)
    let initial_heap_end = USER_HEAP_BASE + USER_HEAP_SIZE;

    // Only map new pages when expanding beyond already mapped region
    // Expansion start should be the greater of initial_heap_end and current_top_aligned
    if new_top_aligned > current_top_aligned {
        let expand_start = VirtAddr::from(initial_heap_end.max(current_top_aligned));
        let expand_size = new_top_aligned.saturating_sub(expand_start.as_usize());

        if expand_size > 0
            && proc_state
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
    } else if new_top_aligned < current_top_aligned {
        // Only unmap pages beyond the initially mapped heap region.
        let shrink_start = VirtAddr::from(initial_heap_end.max(new_top_aligned));
        let shrink_size = current_top_aligned.saturating_sub(shrink_start.as_usize());

        if shrink_size > 0
            && proc_state
                .address_space()
                .lock()
                .unmap(shrink_start, shrink_size)
                .is_err()
        {
            return Ok(current_top as isize);
        }
    }

    proc_state.set_heap_top(addr);
    Ok(addr as isize)
}

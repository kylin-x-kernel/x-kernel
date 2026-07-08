// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Heap management syscalls.

use kaddr_layout::{USER_HEAP_BASE, USER_HEAP_SIZE, USER_HEAP_SIZE_MAX};
use kerrno::KResult;
use khal::paging::{MappingFlags, PageSize};
use kprocess::{current_user_process, current_user_process_address_space};
use memaddr::{PAGE_SIZE_4K, VirtAddr, VirtAddrRange, align_up_4k};
use memspace::{VmBackingKind, VmRuntimeRef};

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
        let start = align_up_4k(new_top);
        let end = align_up_4k(current_top);
        (end > start).then(|| (VirtAddr::from(start), end - start))
    }
}

struct BrkRequest {
    requested_top: usize,
    current_top: usize,
    layout: HeapLayout,
}

impl BrkRequest {
    fn new(requested_top: usize, current_top: usize) -> Self {
        Self {
            requested_top,
            current_top,
            layout: HeapLayout::current(),
        }
    }

    fn is_query(&self) -> bool {
        self.requested_top == 0
    }

    fn is_in_range(&self) -> bool {
        self.layout.contains_brk(self.requested_top)
    }

    fn expand_range(&self) -> Option<(VirtAddr, usize)> {
        self.layout
            .expand_range(self.current_top, self.requested_top)
    }

    fn shrink_range(&self) -> Option<(VirtAddr, usize)> {
        self.layout
            .shrink_range(self.current_top, self.requested_top)
    }
}

pub fn sys_brk(addr: usize) -> KResult<isize> {
    let process = current_user_process();
    let current_top = process.heap_top()?;
    let request = BrkRequest::new(addr, current_top);

    if request.is_query() {
        return Ok(current_top as isize);
    }

    if !request.is_in_range() {
        return Ok(current_top as isize);
    }

    if let Some((expand_start, expand_size)) = request.expand_range() {
        if process
            .address_space()?
            .lock()
            .map(
                expand_start,
                expand_size,
                MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER,
                false,
                VmRuntimeRef::new_anon_private(expand_start, PageSize::Size4K),
            )
            .is_err()
        {
            return Ok(current_top as isize);
        }
    } else if addr > current_top {
        // Expansion within the pre-mapped range.
        let aspace_ref = current_user_process_address_space();
        let mut aspace = aspace_ref.lock();
        let map_start = VirtAddr::from(align_up_4k(current_top));
        let map_size = align_up_4k(addr) - map_start.as_usize();
        if map_size > 0 {
            match aspace.find_vma(map_start) {
                // External (e.g. shm) VMA at start → block.
                Some(vma)
                    if matches!(vma.backing().kind(), VmBackingKind::AnonymousShared { .. }) =>
                {
                    return Ok(current_top as isize);
                }
                // Brk VMA that already reaches addr → nothing to do.
                Some(vma) if vma.end().as_usize() >= addr => {}
                // Fragment or free: check whole range and re-map.
                _ => {
                    let limit = VirtAddrRange::new(aspace.base(), aspace.end());
                    if aspace.find_free_area(map_start, map_size, limit, PAGE_SIZE_4K)
                        != Some(map_start)
                    {
                        return Ok(current_top as isize);
                    }
                    let _ = aspace.map(
                        map_start,
                        map_size,
                        MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER,
                        false,
                        VmRuntimeRef::new_anon_private(map_start, PageSize::Size4K),
                    );
                }
            }
        }
    } else if let Some((shrink_start, shrink_size)) = request.shrink_range() {
        let aspace_ref = current_user_process_address_space();
        let _ = aspace_ref.lock().unmap(shrink_start, shrink_size);
    }

    current_user_process()
        .set_heap_top(request.requested_top)
        .expect("current user thread must have live process heap state");
    Ok(request.requested_top as isize)
}

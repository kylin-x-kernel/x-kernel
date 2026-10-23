// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};

thread_local! {
    static COUNTS: Cell<Option<(usize, usize)>> = const { Cell::new(None) };
}

struct CountingAllocator;

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn record(is_allocation: bool) {
    let _ = COUNTS.try_with(|counts| {
        if let Some((allocations, deallocations)) = counts.get() {
            counts.set(Some((
                allocations + usize::from(is_allocation),
                deallocations + usize::from(!is_allocation),
            )));
        }
    });
}

// SAFETY: Every operation delegates to System with unchanged arguments.
// Counters are thread-local, nonallocating, and do not unwind.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(true);
        // SAFETY: The allocator caller supplies a valid allocation layout.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(true);
        // SAFETY: The allocator caller supplies a valid allocation layout.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        record(false);
        // SAFETY: The caller supplies a live System pointer and matching layout.
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record(true);
        record(false);
        // SAFETY: The caller supplies a live System allocation and valid size.
        unsafe { System.realloc(pointer, layout, size) }
    }
}

pub fn measure(operation: impl FnOnce()) -> (usize, usize) {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            COUNTS.with(|counts| counts.set(None));
        }
    }
    COUNTS.with(|counts| assert!(counts.replace(Some((0, 0))).is_none()));
    let _reset = Reset;
    operation();
    COUNTS.with(|counts| counts.get().unwrap())
}

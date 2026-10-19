// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host-only, thread-local allocation rejection; other tests keep System allocation.

extern crate std;

use core::{
    alloc::{GlobalAlloc, Layout},
    cell::Cell,
    ptr,
};

use self::std::alloc::System;

std::thread_local! {
    static IS_REJECTING: Cell<bool> = const { Cell::new(false) };
}

struct TestAllocator;

#[global_allocator]
static ALLOCATOR: TestAllocator = TestAllocator;

// SAFETY: rejection returns null without unwinding or allocating; all successful
// allocations and all deallocations use System with unchanged pointer/layout.
unsafe impl GlobalAlloc for TestAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if IS_REJECTING.try_with(Cell::get).unwrap_or(false) {
            ptr::null_mut()
        } else {
            // SAFETY: the allocator caller supplies a valid nonzero layout.
            unsafe { System.alloc(layout) }
        }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: every successful allocation was delegated to System; the
        // allocator caller supplies its live pointer and matching layout.
        unsafe { System.dealloc(pointer, layout) }
    }
}

pub(super) fn without_allocations<R>(operation_fn: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            IS_REJECTING.with(|state| state.set(self.0));
        }
    }
    let _restore = Restore(IS_REJECTING.with(|state| state.replace(true)));
    operation_fn()
}

#[cfg_attr(test, test)]
fn allocation_rejection_is_scoped_and_allows_deallocation() {
    let layout = Layout::new::<u64>();
    // SAFETY: nonzero valid layout; successful allocations are released below.
    let original = unsafe { alloc::alloc::alloc(layout) };
    assert!(!original.is_null());
    let rejected = without_allocations(|| {
        // SAFETY: original was allocated with this layout, and is freed once.
        unsafe { alloc::alloc::dealloc(original, layout) };
        // SAFETY: valid layout; rejection must return null without unwinding.
        unsafe { alloc::alloc::alloc(layout) }
    });
    if !rejected.is_null() {
        // SAFETY: clean up an unexpected successful allocation before failing.
        unsafe { alloc::alloc::dealloc(rejected, layout) };
    }
    assert!(rejected.is_null());
    // SAFETY: valid layout, after restoration of the thread-local allocator mode.
    let restored = unsafe { alloc::alloc::alloc(layout) };
    assert!(!restored.is_null());
    // SAFETY: restored is live and was allocated with this layout.
    unsafe { alloc::alloc::dealloc(restored, layout) };
}

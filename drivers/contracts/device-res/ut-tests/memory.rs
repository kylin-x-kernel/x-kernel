// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::alloc::{alloc, dealloc};
use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::def_test;

use super::*;

struct Provider {
    allocations: AtomicUsize,
    frees: AtomicUsize,
    fail: bool,
}

impl Provider {
    fn new(fail: bool) -> Self {
        Self {
            allocations: AtomicUsize::new(0),
            frees: AtomicUsize::new(0),
            fail,
        }
    }
}

// SAFETY: the allocator honors Layout and returns unique storage; counters are
// atomic and deallocation uses the original allocation's pointer and layout.
unsafe impl CpuMemoryOp for Provider {
    fn alloc_cpu(&self, layout: Layout) -> ResResult<NonNull<u8>> {
        self.allocations.fetch_add(1, Ordering::Relaxed);
        if self.fail {
            return Err(ResError::NoMemory);
        }
        // SAFETY: CpuBuffer requests a valid, nonempty layout. Tests inject
        // exhaustion above instead of invoking the kernel's fatal OOM policy.
        NonNull::new(unsafe { alloc(layout) }).ok_or(ResError::NoMemory)
    }

    unsafe fn free_cpu(&self, pointer: NonNull<u8>, layout: Layout) {
        // SAFETY: the sole owning CpuBuffer returns the original allocation
        // exactly once, after its last byte borrow ends.
        unsafe { dealloc(pointer.as_ptr(), layout) };
        self.frees.fetch_add(1, Ordering::Relaxed);
    }
}

#[def_test]
fn cpu_buffer_initializes_requested_alignment_and_releases_once() {
    let provider = Provider::new(false);
    {
        let mut buffer =
            CpuBuffer::alloc_with(&provider, Layout::from_size_align(513, 64).unwrap()).unwrap();
        let bytes = buffer.as_mut_slice();
        assert_eq!(bytes.len(), 513);
        assert!((bytes.as_ptr() as usize).is_multiple_of(64));
        assert!(bytes.iter().all(|&byte| byte == 0));
        bytes[512] = 0xa7;
        assert_eq!(provider.allocations.load(Ordering::Relaxed), 1);
        assert_eq!(provider.frees.load(Ordering::Relaxed), 0);
    }
    assert_eq!(provider.frees.load(Ordering::Relaxed), 1);
}

#[def_test]
fn cpu_buffer_rejects_empty_layout_and_propagates_allocation_failure() {
    let provider = Provider::new(true);
    assert!(matches!(
        CpuBuffer::alloc_with(&provider, Layout::from_size_align(0, 4).unwrap()),
        Err(ResError::InvalidResource)
    ));
    assert_eq!(provider.allocations.load(Ordering::Relaxed), 0);
    assert!(matches!(
        CpuBuffer::alloc_with(&provider, Layout::from_size_align(512, 4).unwrap()),
        Err(ResError::NoMemory)
    ));
    assert_eq!(provider.allocations.load(Ordering::Relaxed), 1);
    assert_eq!(provider.frees.load(Ordering::Relaxed), 0);
}

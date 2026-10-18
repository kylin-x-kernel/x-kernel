// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Injectable ordinary CPU allocation; no DMA state or alternate storage model.

use alloc::alloc::{alloc, dealloc};
use core::{
    alloc::Layout,
    ptr::NonNull,
    sync::atomic::{AtomicUsize, Ordering},
};

use device_res::{CpuMemoryOp, ResError, ResResult};

pub(crate) struct Provider {
    pub(crate) allocations: AtomicUsize,
    pub(crate) frees: AtomicUsize,
    pub(crate) last_size: AtomicUsize,
    fail: bool,
}

impl Provider {
    pub(crate) const fn new() -> Self {
        Self::with_failure(false)
    }

    pub(crate) const fn with_failure(fail: bool) -> Self {
        Self {
            allocations: AtomicUsize::new(0),
            frees: AtomicUsize::new(0),
            last_size: AtomicUsize::new(0),
            fail,
        }
    }
}

pub(super) static DEFAULT: Provider = Provider::new();

// SAFETY: ordinary allocation honors Layout and returns unique storage until
// the owner's matching release. All provider accounting is synchronized.
unsafe impl CpuMemoryOp for Provider {
    fn alloc_cpu(&self, layout: Layout) -> ResResult<NonNull<u8>> {
        if layout.size() == 0 {
            return Err(ResError::InvalidResource);
        }
        self.allocations.fetch_add(1, Ordering::Relaxed);
        self.last_size.store(layout.size(), Ordering::Relaxed);
        if self.fail {
            return Err(ResError::NoMemory);
        }
        // SAFETY: layout is valid and nonempty. Tests inject OOM above rather
        // than exercising the global allocator's fatal policy.
        NonNull::new(unsafe { alloc(layout) }).ok_or(ResError::NoMemory)
    }

    unsafe fn free_cpu(&self, pointer: NonNull<u8>, layout: Layout) {
        // SAFETY: CpuBuffer returns its original live allocation once, after
        // the last byte borrow ends. No device mapping was created.
        unsafe { dealloc(pointer.as_ptr(), layout) };
        self.frees.fetch_add(1, Ordering::Relaxed);
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ordinary CPU allocation without DMA mappings or page-attribute changes.

use core::{alloc::Layout, ptr::NonNull};

use device_res::{CpuMemoryOp, ResError, ResResult};

use crate::XKernelResourceProvider;

// SAFETY: the kernel's ordinary allocator returns unique live memory satisfying
// Layout, serializes its own allocation state, and accepts the original pointer
// and layout for one matching release. This uses the fallible inherent method,
// not the GlobalAlloc implementation with its fatal OOM policy.
unsafe impl CpuMemoryOp for XKernelResourceProvider {
    fn alloc_cpu(&self, layout: Layout) -> ResResult<NonNull<u8>> {
        if layout.size() == 0 {
            return Err(ResError::InvalidResource);
        }
        kalloc::global_allocator()
            .alloc(layout)
            .map_err(|_| ResError::NoMemory)
    }

    unsafe fn free_cpu(&self, pointer: NonNull<u8>, layout: Layout) {
        kalloc::global_allocator().dealloc(pointer, layout);
    }
}

unittest::path_mod_test!(tests, "../ut-tests/memory.rs");

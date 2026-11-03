// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::zero_owned_memory;

#[def_test]
fn owned_memory_zero_preserves_unaligned_allocation_boundaries() {
    for offset in 0..128 {
        for len in [0, 1, 63, 64, 127, 255, 256, 257, 511, 512, 1024] {
            let mut bytes = [0x5au8; 1280];
            // SAFETY: this stack array is exclusive normal memory and
            // offset + len never exceeds its 1280-byte allocation.
            let ptr = bytes.as_mut_ptr().wrapping_add(offset);
            // SAFETY: this stack array is exclusive normal memory and
            // offset + len never exceeds its 1280-byte allocation.
            unsafe { zero_owned_memory(ptr, len) };
            assert!(bytes[..offset].iter().all(|byte| *byte == 0x5a));
            assert!(bytes[offset..offset + len].iter().all(|byte| *byte == 0));
            assert!(bytes[offset + len..].iter().all(|byte| *byte == 0x5a));
        }
    }
}

#[def_test]
fn global_alloc_zeroed_initializes_the_requested_layout() {
    use core::alloc::{GlobalAlloc, Layout};
    let allocator = crate::global_allocator();
    for size in [1, 255, 256, 4096, 262144] {
        let layout = Layout::from_size_align(size, 16).unwrap();
        // SAFETY: layout is nonzero and valid; the allocation stays owned
        // until deallocation with this same layout below.
        let allocation = unsafe { GlobalAlloc::alloc_zeroed(allocator, layout) };
        assert!(!allocation.is_null());
        // SAFETY: alloc_zeroed initialized every byte in this live region,
        // and the shared view is only used before its deallocation.
        let bytes = unsafe { core::slice::from_raw_parts(allocation, size) };
        assert!(bytes.iter().all(|byte| *byte == 0));
        // SAFETY: this pointer still owns the allocation returned above;
        // its original layout is unchanged and no view is used afterward.
        unsafe { GlobalAlloc::dealloc(allocator, allocation, layout) };
    }
}

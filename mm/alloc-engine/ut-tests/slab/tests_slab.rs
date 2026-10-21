// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::alloc::Layout;

use unittest::def_test;

use super::SlabByteAllocator;
use crate::{BaseAllocator, ByteAllocator};

#[def_test]
fn test_slab_allocate_deallocate() {
    let mut alloc = SlabByteAllocator::new();
    let mut heap = alloc::vec![0u8; 64 * 1024].into_boxed_slice();
    let base = heap.as_mut_ptr() as usize;
    let size = heap.len();
    alloc.init_region(base, size);
    let layout = Layout::from_size_align(32, 8).unwrap();
    let ptr = alloc.allocate(layout).unwrap();
    assert!(alloc.used_bytes() >= 32);
    alloc.deallocate(ptr, layout);
    assert!(alloc.used_bytes() <= alloc.total_bytes());
}

#[def_test]
fn test_slab_available_bytes() {
    let mut alloc = SlabByteAllocator::new();
    let mut heap = alloc::vec![0u8; 64 * 1024].into_boxed_slice();
    let base = heap.as_mut_ptr() as usize;
    let size = heap.len();
    alloc.init_region(base, size);
    assert_eq!(alloc.total_bytes(), alloc.available_bytes());
}

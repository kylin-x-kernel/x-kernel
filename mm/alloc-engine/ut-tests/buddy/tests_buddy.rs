// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::alloc::Layout;

use unittest::def_test;

use super::BuddyByteAllocator;
use crate::{BaseAllocator, ByteAllocator};

#[def_test]
fn test_buddy_allocate_deallocate() {
    let mut alloc = BuddyByteAllocator::new();
    let mut heap = alloc::vec![0u8; 4096].into_boxed_slice();
    let base = heap.as_mut_ptr() as usize;
    let size = heap.len();
    alloc.init_region(base, size);
    let layout = Layout::from_size_align(64, 8).unwrap();
    let ptr = alloc.allocate(layout).unwrap();
    assert!(alloc.used_bytes() >= 64);
    alloc.deallocate(ptr, layout);
    assert!(alloc.used_bytes() <= alloc.total_bytes());
}

#[def_test]
fn test_buddy_available_bytes() {
    let mut alloc = BuddyByteAllocator::new();
    let mut heap = alloc::vec![0u8; 4096].into_boxed_slice();
    let base = heap.as_mut_ptr() as usize;
    let size = heap.len();
    alloc.init_region(base, size);
    assert_eq!(alloc.total_bytes(), alloc.available_bytes());
}

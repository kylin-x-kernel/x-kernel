// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn register_pool_and_recycle_mixed_size_buffers() {
    let mut backing = alloc::vec![0u8; 65536];
    let cpu_base = NonNull::new(backing.as_mut_ptr()).unwrap();
    let bus_base = DmaBusAddress::new(0x1000_0000);
    let mut pool = BouncePool::new();
    assert!(!pool.is_initialized());
    pool.initialize(
        DmaInfo {
            cpu_addr: cpu_base,
            bus_addr: bus_base,
        },
        backing.len(),
    );
    assert!(pool.is_initialized());

    for _ in 0..8 {
        let mut allocations = alloc::vec::Vec::new();
        for (index, len) in [64, 4096, 8193].into_iter().enumerate() {
            let (dma, layout) = pool.allocate(len).unwrap();
            let offset = dma.cpu_addr.as_ptr() as usize - cpu_base.as_ptr() as usize;
            assert!(offset + len <= backing.len());
            assert_eq!(dma.bus_addr.as_u64(), bus_base.as_u64() + offset as u64);
            // SAFETY: TLSF returned exclusive writable storage for `len` bytes
            // within the live backing region, retained until deallocation below.
            let buffer = unsafe { core::slice::from_raw_parts_mut(dma.cpu_addr.as_ptr(), len) };
            buffer.fill(index as u8 + 1);
            allocations.push((dma, layout, index as u8 + 1));
        }
        for (dma, layout, pattern) in allocations {
            // SAFETY: this allocation has not been recycled, and the backing
            // region remains live; no other allocation aliases these bytes.
            let buffer =
                unsafe { core::slice::from_raw_parts(dma.cpu_addr.as_ptr(), layout.size()) };
            assert!(buffer.iter().all(|byte| *byte == pattern));
            pool.deallocate(dma.cpu_addr.as_ptr() as usize, layout);
        }
    }
}

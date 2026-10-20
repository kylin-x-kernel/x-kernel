// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Descriptor validation and layout regressions in the kernel test harness.

use alloc::vec;

use super::*;
use crate::{PhysAddr, memory::GuestMemoryAccessor};

#[derive(Clone)]
struct TestTranslator {
    base_host_ptr: usize,
}

impl GuestMemoryAccessor for TestTranslator {
    fn translate_and_get_limit(&self, guest_addr: GuestPhysAddr) -> Option<(PhysAddr, usize)> {
        let offset = guest_addr.as_usize();
        Some((PhysAddr::from(self.base_host_ptr + offset), usize::MAX))
    }
}

#[unittest::def_test]
fn status_descriptor_len_must_be_at_least_one() {
    // Allocate a backing buffer to simulate host memory
    let mut mem = vec![0u8; 4096];
    let base_ptr = mem.as_mut_ptr() as usize;
    let translator = TestTranslator {
        base_host_ptr: base_ptr,
    };
    let mut memory = crate::AddressSpaceMemory::new(&translator);

    // Create a descriptor table at a non-zero guest base within our backing buffer
    let base = GuestPhysAddr::from(0x10usize);
    let table = DescriptorTable::new(base, 2);

    // Build a 2-descriptor chain: desc0 -> desc1
    let mut d0 = VirtQueueDesc::new(GuestPhysAddr::from(0x100usize), 16, 0, 1);
    d0.set_next(true);
    let mut d1 = VirtQueueDesc::new(GuestPhysAddr::from(0x200usize), 0, 0, 0);
    d1.set_write(true); // status descriptor must be write-only for device
    d1.set_next(false);

    table.write_desc(0, &d0, &mut memory).unwrap();
    table.write_desc(1, &d1, &mut memory).unwrap();

    // len == 0 should be invalid
    let err = table.get_status_addr(0, &mut memory).unwrap_err();
    assert!(matches!(err, VirtioError::InvalidQueue));

    // Fix len to 1, now it should pass
    let mut d1_ok = d1;
    d1_ok.len = 1;
    table.write_desc(1, &d1_ok, &mut memory).unwrap();
    let ok_addr = table.get_status_addr(0, &mut memory).unwrap();
    assert_eq!(ok_addr.as_usize(), 0x200);
}

#[unittest::def_test]
fn layout_size_counts_descriptors() {
    // 4 descriptors * 16 bytes (VirtQueueDesc) = 64.
    assert_eq!(DescriptorTable::layout_size(4), 64);
    // Boundary: the largest queue size still counts every descriptor.
    assert_eq!(DescriptorTable::layout_size(256), 4096);
}

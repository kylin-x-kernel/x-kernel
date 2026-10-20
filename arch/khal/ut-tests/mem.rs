// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::{MemFlags, MemoryRegion};

#[def_test]
fn test_memory_region_new_ram() {
    let region = MemoryRegion::new_ram(0x1000, 0x2000, "ram");
    assert_eq!(region.paddr.as_usize(), 0x1000);
    assert_eq!(region.size, 0x2000);
    assert!(region.flags.contains(MemFlags::R));
    assert!(region.flags.contains(MemFlags::W));
    assert!(region.flags.contains(MemFlags::FREE));
}

#[def_test]
fn test_memory_region_new_mmio() {
    let region = MemoryRegion::new_mmio(0x3000, 0x1000, "mmio");
    assert_eq!(region.size, 0x1000);
    assert!(region.flags.contains(MemFlags::DEV));
    assert!(region.flags.contains(MemFlags::RSVD));
}

#[def_test]
fn test_memory_region_new_rsvd() {
    let region = MemoryRegion::new_rsvd(0x4000, 0x800, "rsvd");
    assert_eq!(region.size, 0x800);
    assert!(region.flags.contains(MemFlags::RSVD));
    assert!(!region.flags.contains(MemFlags::FREE));
}

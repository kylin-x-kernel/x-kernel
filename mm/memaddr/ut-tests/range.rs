// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::AddrRange;
use crate::VirtAddr;

#[def_test]
fn test_range_size_and_empty() {
    let range = AddrRange::from_start_size(VirtAddr::from(0x1000usize), 0x1000);
    assert_eq!(range.size(), 0x1000);
    assert!(!range.is_empty());
    let empty = AddrRange::new(VirtAddr::from(0x1000usize), VirtAddr::from(0x1000usize));
    assert!(empty.is_empty());
}

#[def_test]
fn test_range_contains_range() {
    let outer = AddrRange::from_start_size(VirtAddr::from(0x1000usize), 0x3000);
    let inner = AddrRange::from_start_size(VirtAddr::from(0x2000usize), 0x1000);
    assert!(outer.contains_range(inner));
    assert!(inner.contained_in(outer));
}

#[def_test]
fn test_range_overlap() {
    let a = AddrRange::from_start_size(VirtAddr::from(0x1000usize), 0x1000);
    let b = AddrRange::from_start_size(VirtAddr::from(0x1800usize), 0x1000);
    let c = AddrRange::from_start_size(VirtAddr::from(0x3000usize), 0x1000);
    assert!(a.overlaps(b));
    assert!(!a.overlaps(c));
}

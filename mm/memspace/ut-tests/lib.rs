// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use khal::{mem::MemFlags, paging::MappingFlags};
use unittest::def_test;

use super::mem_to_mapping_flags;

#[def_test]
fn test_mem_to_mapping_flags_basic() {
    let flags = MemFlags::READ | MemFlags::WRITE;
    let mapped = mem_to_mapping_flags(flags);
    assert!(mapped.contains(MappingFlags::READ));
    assert!(mapped.contains(MappingFlags::WRITE));
}

#[def_test]
fn test_mem_to_mapping_flags_device_uncached() {
    let flags = MemFlags::DEVICE | MemFlags::UNCACHED;
    let mapped = mem_to_mapping_flags(flags);
    assert!(mapped.contains(MappingFlags::DEVICE));
    assert!(mapped.contains(MappingFlags::UNCACHED));
}

#[def_test]
fn test_mem_to_mapping_flags_empty() {
    let mapped = mem_to_mapping_flags(MemFlags::empty());
    assert!(mapped.is_empty());
}

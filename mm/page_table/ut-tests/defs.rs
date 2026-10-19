// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use memaddr::VirtAddr;
use unittest::def_test;

use super::{PageSize, PagingFlags, PagingMetaData};

struct DummyMeta;

impl PagingMetaData for DummyMeta {
    type VirtAddr = VirtAddr;

    const LEVELS: usize = 4;
    const PA_MAX_BITS: usize = 36;
    const VA_MAX_BITS: usize = 39;

    fn flush_tlb(_vaddr: Option<Self::VirtAddr>) {}
}

#[def_test]
fn test_paging_flags_bits() {
    let flags = PagingFlags::READ | PagingFlags::WRITE;
    assert!(flags.contains(PagingFlags::READ));
    assert!(flags.contains(PagingFlags::WRITE));
    assert!(!flags.contains(PagingFlags::EXECUTE));
}

#[def_test]
fn test_page_size_alignment() {
    assert!(PageSize::Size4K.is_aligned(0x2000));
    assert!(!PageSize::Size4K.is_aligned(0x2001));
    assert!(PageSize::Size2M.is_huge());
}

#[def_test]
fn test_paging_metadata_bounds() {
    assert!(DummyMeta::paddr_is_valid(0));
    assert!(DummyMeta::paddr_is_valid((1 << DummyMeta::PA_MAX_BITS) - 1));
    assert!(!DummyMeta::paddr_is_valid(1 << DummyMeta::PA_MAX_BITS));
}

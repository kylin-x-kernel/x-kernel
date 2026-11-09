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

#[cfg(feature = "smp")]
#[def_test]
fn local_only_metadata_does_not_confirm_a_remote_scope() {
    let mut targets = kcpu_id_map::KCpuMask::new();
    targets.set(0, true);
    assert_eq!(
        DummyMeta::flush_tlb_process_mask(None, targets),
        Err(super::TlbFlushError::UnsupportedScope)
    );
    assert_eq!(
        DummyMeta::flush_tlb_process_mask(None, kcpu_id_map::KCpuMask::new()),
        Ok(())
    );
    assert_eq!(
        DummyMeta::flush_tlb_all_cpus(None),
        Err(super::TlbFlushError::UnsupportedScope)
    );
}

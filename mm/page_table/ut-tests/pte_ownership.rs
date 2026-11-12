// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Check the selected architecture's private mapping-ownership encoding.

use memaddr::PhysAddr;
use unittest::def_test;

use crate::{ArchPageEntry, PageSize, PageTableEntry, PagingFlags};

#[cfg(target_arch = "x86_64")]
const MANAGED_MASK: usize = 1 << 9;
#[cfg(target_arch = "aarch64")]
const MANAGED_MASK: usize = 1 << 55;
#[cfg(target_arch = "riscv64")]
const MANAGED_MASK: usize = 1 << 8;
#[cfg(target_arch = "loongarch64")]
const MANAGED_MASK: usize = 1 << 11;

const PAGE_SIZES: [PageSize; 3] = [PageSize::Size4K, PageSize::Size2M, PageSize::Size1G];
const INITIAL_PADDR: usize = 0x4000_0000;
const UPDATED_PADDR: usize = 0x8000_0000;

#[cfg(target_arch = "riscv64")]
#[def_test]
fn write_only_riscv_leaves_use_read_write_encoding() {
    // Check the hardware R/W bits directly: decoded software permissions alone
    // do not prove that an Sv39/Sv48 leaf has a legal permission encoding.
    const READ_WRITE_BITS: usize = (1 << 1) | (1 << 2);
    for page_size in PAGE_SIZES {
        let flags = PagingFlags::USER | PagingFlags::WRITE;
        let mut entry = ArchPageEntry::new_page(
            PhysAddr::from(INITIAL_PADDR),
            flags,
            page_size.is_huge(),
        );
        assert_eq!(entry.bits() & READ_WRITE_BITS, READ_WRITE_BITS);
        entry.set_managed(true);
        for initial_flags in [PagingFlags::USER, PagingFlags::USER | PagingFlags::READ] {
            entry.set_flags(initial_flags, page_size.is_huge());
            entry.set_flags(flags, page_size.is_huge());
            assert_eq!(entry.bits() & READ_WRITE_BITS, READ_WRITE_BITS);
            assert!(entry.is_present());
            assert!(entry.is_managed());
            assert_eq!(entry.paddr(), PhysAddr::from(INITIAL_PADDR));
        }
    }
}

#[def_test]
fn raw_entries_start_unmanaged() {
    assert!(!ArchPageEntry::EMPTY.is_managed());
    for page_size in PAGE_SIZES {
        let mut entry = ArchPageEntry::new_page(
            PhysAddr::from(INITIAL_PADDR),
            PagingFlags::all(),
            page_size.is_huge(),
        );
        assert!(!entry.is_managed());
        assert_eq!(entry.bits() & MANAGED_MASK, 0);
        entry.set_flags(PagingFlags::READ | PagingFlags::USER, page_size.is_huge());
        assert!(!entry.is_managed());
        entry.set_paddr(PhysAddr::from(UPDATED_PADDR));
        assert!(!entry.is_managed());
    }
    let table = ArchPageEntry::new_table(PhysAddr::from(INITIAL_PADDR));
    assert!(!table.is_managed());
    assert_eq!(table.bits() & MANAGED_MASK, 0);
}

#[def_test]
fn managed_leaf_updates_preserve_ownership() {
    let flag_sets = [
        PagingFlags::READ | PagingFlags::WRITE | PagingFlags::USER,
        PagingFlags::READ | PagingFlags::EXECUTE | PagingFlags::USER,
        PagingFlags::READ | PagingFlags::DEVICE | PagingFlags::USER,
        PagingFlags::READ | PagingFlags::UNCACHED | PagingFlags::USER,
        PagingFlags::READ | PagingFlags::WRITE | PagingFlags::SHARED,
        PagingFlags::empty(),
        PagingFlags::READ,
    ];
    for page_size in PAGE_SIZES {
        let is_huge = page_size.is_huge();
        let mut entry =
            ArchPageEntry::new_page(PhysAddr::from(INITIAL_PADDR), PagingFlags::READ, is_huge);
        entry.set_managed(true);
        let mut paddr = PhysAddr::from(INITIAL_PADDR);
        for flags in flag_sets {
            entry.set_flags(flags, is_huge);
            let expected = ArchPageEntry::new_page(paddr, flags, is_huge);
            assert!(entry.is_managed());
            if expected.is_present() {
                assert_eq!(entry.paddr(), paddr);
            }
            assert_eq!(entry.flags(), expected.flags());
            assert_eq!(entry.is_present(), expected.is_present());
            assert_eq!(entry.is_huge(), expected.is_huge());
            assert_eq!(entry.bits(), expected.bits() | MANAGED_MASK);

            let updated_paddr = PhysAddr::from(if paddr.as_usize() == INITIAL_PADDR {
                UPDATED_PADDR
            } else {
                INITIAL_PADDR
            });
            entry.set_paddr(updated_paddr);
            let expected = ArchPageEntry::new_page(updated_paddr, flags, is_huge);
            assert!(entry.is_managed());
            if expected.is_present() {
                assert_eq!(entry.paddr(), updated_paddr);
            }
            assert_eq!(entry.flags(), expected.flags());
            assert_eq!(entry.is_present(), expected.is_present());
            assert_eq!(entry.is_huge(), expected.is_huge());
            assert_eq!(entry.bits(), expected.bits() | MANAGED_MASK);
            paddr = updated_paddr;
        }
    }
}

#[def_test]
fn ownership_toggle_preserves_leaf_encoding() {
    for page_size in PAGE_SIZES {
        let raw = ArchPageEntry::new_page(
            PhysAddr::from(INITIAL_PADDR),
            PagingFlags::READ | PagingFlags::WRITE | PagingFlags::USER,
            page_size.is_huge(),
        );
        let mut entry = raw;
        for _ in 0..2 {
            entry.set_managed(true);
            assert!(entry.is_managed());
            assert_eq!(entry.bits() ^ raw.bits(), MANAGED_MASK);
            assert_eq!(entry.paddr(), raw.paddr());
            assert_eq!(entry.flags(), raw.flags());
            assert_eq!(entry.is_present(), raw.is_present());
            assert_eq!(entry.is_huge(), raw.is_huge());
        }
        let replayed = ArchPageEntry::new_page(entry.paddr(), entry.flags(), page_size.is_huge());
        assert!(!replayed.is_managed());
        for _ in 0..2 {
            entry.set_managed(false);
            assert!(!entry.is_managed());
            assert_eq!(entry.bits(), raw.bits());
        }
    }
}

#[def_test]
fn ownership_does_not_change_table_decoding() {
    let raw = ArchPageEntry::new_table(PhysAddr::from(INITIAL_PADDR));
    assert!(raw.is_table());
    let mut entry = raw;
    // Toggle an unpublished descriptor to keep category separate from table decoding.
    entry.set_managed(true);
    assert_eq!(entry.bits() ^ raw.bits(), MANAGED_MASK);
    assert!(entry.is_table());
    assert_eq!(entry.paddr(), raw.paddr());
    assert_eq!(entry.flags(), raw.flags());
    assert_eq!(entry.is_present(), raw.is_present());
    assert_eq!(entry.is_huge(), raw.is_huge());

    entry.set_paddr(PhysAddr::from(UPDATED_PADDR));
    assert!(entry.is_managed());
    assert!(entry.is_table());
    assert_eq!(entry.paddr(), PhysAddr::from(UPDATED_PADDR));
    entry.set_managed(false);
    assert_eq!(
        entry.bits(),
        ArchPageEntry::new_table(PhysAddr::from(UPDATED_PADDR)).bits()
    );

    let mut empty = ArchPageEntry::EMPTY;
    empty.set_managed(true);
    assert!(!empty.is_table());
    assert!(!empty.is_present());
}

#[def_test]
fn clear_discards_ownership_category() {
    for page_size in PAGE_SIZES {
        let mut entry = ArchPageEntry::new_page(
            PhysAddr::from(INITIAL_PADDR),
            PagingFlags::READ | PagingFlags::WRITE,
            page_size.is_huge(),
        );
        entry.set_managed(true);
        assert!(entry.is_managed());
        entry.clear();
        assert!(!entry.is_managed());
        assert!(entry.is_unused());
        assert!(!entry.is_present());
        assert!(!entry.is_table());
        assert!(entry.flags().is_empty());
        assert_eq!(entry.bits(), ArchPageEntry::EMPTY.bits());
    }
}

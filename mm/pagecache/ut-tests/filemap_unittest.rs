// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use memaddr::PAGE_SIZE_4K;
use khal::paging::{MappingFlags, PageTable};
use memaddr::VirtAddr;
use unittest::def_test;

use super::*;

#[def_test]
fn collected_folios_reject_removed_and_uninitialized_pages() {
    let cache = MappingPages::new();
    let count = FolioBatch::CAPACITY + 2;
    for index in 0..count {
        let mut folio = Folio::new_zeroed(index as u64).unwrap();
        folio.data()[..3].copy_from_slice(b"old");
        assert!(cache.filemap_add_folio(folio).is_ok());
    }
    let (batch, run_len) = cache.get_folios_contig_with_run_len(0, count);
    assert_eq!(batch.len(), FolioBatch::CAPACITY);
    assert_eq!(run_len, count);
    cache.resize_cached_folios((count * PAGE_SIZE_4K) as u64, 0);
    let mut replacement = Folio::new_zeroed(0).unwrap();
    replacement.data()[..3].copy_from_slice(b"new");
    assert!(cache.filemap_add_folio(replacement).is_ok());
    assert_eq!(batch.with_read_folio(0, |_| ()), None);
    assert_eq!(
        cache
            .get_folios_contig(0, 1)
            .with_read_folio(0, |folio| folio.data()[..3].to_vec()),
        Some(b"new".to_vec())
    );
    assert_eq!(
        cache.with_folio_or_create(1, |_| Err(kerrno::KError::Io), |_| Ok(())),
        Err(kerrno::KError::Io)
    );
    assert_eq!(
        cache.get_folios_contig(1, 1).with_read_folio(0, |_| ()),
        None
    );
    cache
        .with_folio_or_create(
            1,
            |folio| {
                folio.data().fill(0x55);
                Ok(())
            },
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(
        cache
            .get_folios_contig(1, 1)
            .with_read_folio(0, |folio| folio.data()[0]),
        Some(0x55)
    );
}

#[def_test]
fn pinned_folio_keeps_evicted_page_alive_for_readonly_mappings() {
    let cache = MappingPages::new();
    cache
        .with_folio_or_create(
            0,
            |_| Ok(()),
            |folio| {
                folio.data().fill(0x5a);
                Ok(())
            },
        )
        .unwrap();
    let pin = cache
        .pin_folio_or_create(0, |_| Err(kerrno::KError::NoMemory))
        .unwrap();
    let mut page_table = PageTable::try_new().unwrap();
    let mut page_table = page_table.modify();
    let vaddr = VirtAddr::from(0x5000usize);
    // SAFETY: This page table is isolated and never installed on a CPU. The
    // pin remains alive until the page table is dropped.
    let mapping = unsafe {
        pin.clone().map_readonly(
            &mut page_table,
            vaddr,
            MappingFlags::USER | MappingFlags::READ,
        )
    }
    .unwrap();
    let (paddr, _, _) = page_table.query(vaddr).unwrap();
    assert!(pin.matches_paddr(paddr));

    cache.resize_cached_folios(PAGE_SIZE_4K as u64, 0);
    assert_eq!(cache.nrpages(), 0);

    assert!(pin.matches_paddr(paddr));
    let restored = VirtAddr::from(0x7000usize);
    // SAFETY: this isolated table is never activated. The original pin and
    // both tokens retain the page for every operation on the table.
    let restored_mapping = unsafe {
        mapping.map_readonly(&mut page_table, restored, MappingFlags::USER | MappingFlags::READ)
    }.unwrap();
    assert_eq!(page_table.query(restored).unwrap().0, paddr);
    assert!(restored_mapping.matches_paddr(paddr));

    let mut uninit = alloc::vec![core::mem::MaybeUninit::uninit(); PAGE_SIZE_4K];
    pin.copy_to_uninit(uninit.as_mut_slice().try_into().unwrap());
    // SAFETY: copy_to_uninit initializes the entire destination before return.
    let copied = unsafe { core::slice::from_raw_parts(uninit.as_ptr().cast::<u8>(), uninit.len()) };
    assert!(copied.iter().all(|byte| *byte == 0x5a));
}

#[def_test]
fn pinned_page_retains_its_object_without_retaining_the_replacement() {
    let cache = MappingPages::new();
    let pin = cache.pin_folio_or_create(0, |folio| {
        folio.data().fill(0x5a);
        Ok(())
    }).unwrap();
    let original = cache.state.lock().entries[&0].clone();
    cache.truncate_final().unwrap();
    cache.with_folio_or_create(0, |folio| {
        folio.data().fill(0xa5);
        Ok(())
    }, |_| Ok(())).unwrap();
    let replacement = Arc::downgrade(&cache.state.lock().entries[&0]);
    assert!(!Arc::ptr_eq(&original, &cache.state.lock().entries[&0]));
    let clone = pin.clone();
    drop(clone);
    let mut destination = alloc::vec![core::mem::MaybeUninit::uninit(); PAGE_SIZE_4K];
    pin.copy_to_uninit(destination.as_mut_slice().try_into().unwrap());
    // SAFETY: a successful full-page copy initializes the whole destination.
    let copied = unsafe { core::slice::from_raw_parts(destination.as_ptr().cast::<u8>(), PAGE_SIZE_4K) };
    assert!(copied.iter().all(|byte| *byte == 0x5a));
    cache.truncate_final().unwrap();
    assert!(replacement.upgrade().is_none());
    drop(pin);
}

#[def_test]
fn pinned_folio_rejects_writable_page_table_publication() {
    let cache = MappingPages::new();
    let pin = cache.pin_folio_or_create(0, |_| Ok(())).unwrap();
    let mut page_table = PageTable::try_new().unwrap();
    let mut page_table = page_table.modify();

    // SAFETY: The writable flag is rejected before a PTE is published.
    let result = unsafe {
        pin.clone().map_readonly(
            &mut page_table,
            VirtAddr::from(0x1000usize),
            MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
        )
    };
    assert!(matches!(result, Err(kerrno::KError::PermissionDenied)));
    assert!(page_table
        .query(VirtAddr::from(0x1000usize))
        .is_err());

    // SAFETY: This page table is isolated and never installed on a CPU. The
    // pin remains alive until the page table is dropped.
    let _mapping = unsafe {
        pin.clone().map_readonly(
            &mut page_table,
            VirtAddr::from(0x1000usize),
            MappingFlags::USER | MappingFlags::READ,
        )
    }
    .unwrap();
    let (_, flags, _) = page_table.query(VirtAddr::from(0x1000usize)).unwrap();
    assert!(!flags.contains(MappingFlags::WRITE));
}

#[def_test]
fn pinned_folio_rejects_unsupported_page_attributes() {
    let cache = MappingPages::new();
    let pin = cache.pin_folio_or_create(0, |_| Ok(())).unwrap();
    let mut page_table = PageTable::try_new().unwrap();
    let mut page_table = page_table.modify();
    let vaddr = VirtAddr::from(0x3000usize);

    for attribute in [
        MappingFlags::DEVICE,
        MappingFlags::UNCACHED,
        MappingFlags::SHARED,
    ] {
        // SAFETY: Unsupported mapping attributes are rejected before publishing a PTE.
        let result = unsafe {
            pin.clone().map_readonly(
                &mut page_table,
                vaddr,
                MappingFlags::USER | MappingFlags::READ | attribute,
            )
        };
        assert!(matches!(result, Err(kerrno::KError::PermissionDenied)));
        assert!(page_table.query(vaddr).is_err());
    }
}

#[def_test]
fn pinned_folio_readonly_mapping_preserves_page_attributes() {
    let cache = MappingPages::new();
    let pin = cache.pin_folio_or_create(0, |_| Ok(())).unwrap();
    let mut page_table = PageTable::try_new().unwrap();
    let mut page_table = page_table.modify();
    let vaddr = VirtAddr::from(0x2000usize);
    let requested_flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::EXECUTE;

    // SAFETY: This page table is isolated and never installed on a CPU. The
    // pin remains alive until the page table is dropped.
    let _mapping = unsafe { pin.clone().map_readonly(&mut page_table, vaddr, requested_flags) }.unwrap();
    let (_, mapped_flags, _) = page_table.query(vaddr).unwrap();
    assert!(mapped_flags.contains(MappingFlags::USER));
    assert!(mapped_flags.contains(MappingFlags::READ));
    assert!(mapped_flags.contains(MappingFlags::EXECUTE));
    assert!(!mapped_flags.contains(MappingFlags::WRITE));
}

#[def_test]
fn pinned_folio_readonly_mapping_rejects_unaligned_address() {
    let cache = MappingPages::new();
    let pin = cache.pin_folio_or_create(0, |_| Ok(())).unwrap();
    let mut page_table = PageTable::try_new().unwrap();
    let mut page_table = page_table.modify();
    let vaddr = VirtAddr::from(0x3001usize);

    // SAFETY: The unaligned address is rejected before a PTE is published.
    let result = unsafe {
        pin.clone().map_readonly(&mut page_table, vaddr, MappingFlags::USER | MappingFlags::READ)
    };
    assert!(matches!(result, Err(kerrno::KError::InvalidInput)));
    assert!(page_table.query(vaddr).is_err());
}

#[def_test]
fn pinned_folio_readonly_mapping_reports_existing_mapping() {
    let cache = MappingPages::new();
    let pin = cache.pin_folio_or_create(0, |_| Ok(())).unwrap();
    let mut page_table = PageTable::try_new().unwrap();
    let mut page_table = page_table.modify();
    let vaddr = VirtAddr::from(0x4000usize);
    let flags = MappingFlags::USER | MappingFlags::READ;

    // SAFETY: This page table is isolated and never installed on a CPU. The
    // pin remains alive until the page table is dropped.
    let mapping = unsafe { pin.clone().map_readonly(&mut page_table, vaddr, flags) }.unwrap();
    assert!(matches!(
        // SAFETY: An existing PTE is rejected without changing the mapping;
        // the pin remains alive for the page table's entire lifetime.
        unsafe { mapping.map_readonly(&mut page_table, vaddr, flags) },
        Err(kerrno::KError::AlreadyExists)
    ));
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kalloc::{PageRef, UsageKind, alloc_page_ref};
use kspin::NoPreemptIrqSave;
use page_table::PagingHandler;
use unittest::def_test;

use super::{MappingFlags, PageSize, PageTable, PagingHandlerImpl};

#[def_test]
fn handler_keeps_the_transferred_reference_until_table_retirement() {
    let _guard = NoPreemptIrqSave::new();
    let pa = PagingHandlerImpl::alloc_frame().unwrap();
    // SAFETY: consumes this handler's exact transfer before any table/hardware
    // use; after cloning we transfer its original hold back to the handler.
    let owner = unsafe { PageRef::from_raw(pa, 1) };
    let observer = owner.try_clone_ref().unwrap();
    assert_eq!(owner.into_raw(), (pa, 1));
    PagingHandlerImpl::dealloc_frame(pa);

    // A live observer keeps the page out of inventory after the handler drops
    // its hold. Neither allocation is ever installed as an active hardware table.
    let other = alloc_page_ref(UsageKind::PageTable).unwrap();
    assert_ne!(other.phys_addr(), pa);
    drop(other);
    drop(observer);
    // The two allocations left PCP slots. The guard keeps the observer's final
    // release at the top of this CPU's cache until this request consumes it.
    let reused = alloc_page_ref(UsageKind::PageTable).unwrap();
    assert_eq!(reused.phys_addr(), pa);
}

#[def_test]
fn inactive_table_tree_can_be_destroyed_after_its_data_mapping_is_removed() {
    let mut table = PageTable::try_new().unwrap();
    let backing = alloc_page_ref(UsageKind::VirtMem).unwrap();
    let address = 0x800000usize.into();
    let mut edit = table.modify();
    edit.map_raw(
        address,
        backing.phys_addr(),
        PageSize::Size4K,
        MappingFlags::READ,
    )
    .unwrap();
    edit.finish();
    assert_eq!(edit.query(address).unwrap().0, backing.phys_addr());
    edit.unmap(address).unwrap();
    edit.finish();
    drop(edit);
    // This tree has never been activated. Its data page remains independently
    // held while Drop consumes all table-storage raw transfers.
    drop(table);
    let retained = backing.try_clone_ref().unwrap();
    assert_eq!(retained.phys_addr(), backing.phys_addr());
}

#[def_test]
fn copied_root_entries_borrow_the_source_table_storage() {
    let mut source = PageTable::try_new().unwrap();
    let mut target = PageTable::try_new().unwrap();
    let backing = alloc_page_ref(UsageKind::VirtMem).unwrap();
    let address = 0x1000000usize.into();
    {
        let mut edit = source.modify();
        edit.map_raw(
            address,
            backing.phys_addr(),
            PageSize::Size4K,
            MappingFlags::READ,
        )
        .unwrap();
        edit.finish();
    }
    target
        .modify()
        .copy_from(&source, address, memaddr::PAGE_SIZE_4K);
    assert_eq!(target.query(address).unwrap().0, backing.phys_addr());
    // Both tables are inactive. The source outlives the target; dropping the
    // borrower must not consume the source's raw table-storage references.
    drop(target);
    assert_eq!(source.query(address).unwrap().0, backing.phys_addr());
    source.modify().unmap(address).unwrap();
    drop(source);
}

#[def_test]
fn managed_data_page_returns_to_inventory_only_after_mapping_retirement() {
    let _guard = NoPreemptIrqSave::new();
    let mut table = PageTable::try_new().unwrap();
    let owner = alloc_page_ref(UsageKind::VirtMem).unwrap();
    let pa = owner.phys_addr();
    let address = 0x2000000usize.into();
    let mut edit = table.modify();
    edit.map(address, owner.try_clone_ref().unwrap(), MappingFlags::READ)
        .unwrap();
    edit.finish();
    drop(owner);
    assert_eq!(edit.query(address).unwrap().0, pa);
    edit.unmap(address).unwrap();
    let other = alloc_page_ref(UsageKind::VirtMem).unwrap();
    assert_ne!(other.phys_addr(), pa);
    drop(other);
    edit.finish();
    let reused = alloc_page_ref(UsageKind::VirtMem).unwrap();
    assert_eq!(reused.phys_addr(), pa);
}

#[def_test]
fn replacing_managed_data_keeps_the_old_page_until_finish() {
    let _guard = NoPreemptIrqSave::new();
    let mut table = PageTable::try_new().unwrap();
    let old = alloc_page_ref(UsageKind::VirtMem).unwrap();
    let old_pa = old.phys_addr();
    let new = alloc_page_ref(UsageKind::VirtMem).unwrap();
    let new_pa = new.phys_addr();
    let address = 0x2400000usize.into();
    let mut edit = table.modify();
    edit.map(address, old, MappingFlags::READ).unwrap();
    edit.finish();
    let expected = edit.query_entry(address).unwrap();
    edit.replace_if_same(
        address,
        expected,
        new,
        MappingFlags::READ | MappingFlags::WRITE,
    )
    .unwrap();
    assert_eq!(edit.query(address).unwrap().0, new_pa);
    let other = alloc_page_ref(UsageKind::VirtMem).unwrap();
    assert_ne!(other.phys_addr(), old_pa);
    drop(other);
    edit.finish();
    let reused = alloc_page_ref(UsageKind::VirtMem).unwrap();
    assert_eq!(reused.phys_addr(), old_pa);
}

#[def_test(serial)]
fn exact_and_compound_backing_can_each_protect_one_huge_leaf() {
    const HUGE: usize = 2 * 1024 * 1024;
    for compound in [false, true] {
        let _guard = NoPreemptIrqSave::new();
        let before = kalloc::global_allocator().usages().get(UsageKind::VirtMem);
        let mut table = PageTable::try_new().unwrap();
        let owner = if compound {
            kalloc::alloc_compound_pages_ref(HUGE / memaddr::PAGE_SIZE_4K, HUGE, UsageKind::VirtMem)
                .unwrap()
        } else {
            kalloc::alloc_pages_exact_ref(HUGE / memaddr::PAGE_SIZE_4K, HUGE, UsageKind::VirtMem)
                .unwrap()
        };
        let pa = owner.phys_addr();
        let address = 0x4000000usize.into();
        let mut edit = table.modify();
        edit.map_sized(
            address,
            owner.try_clone_ref().unwrap(),
            PageSize::Size2M,
            MappingFlags::READ,
        )
        .unwrap();
        edit.finish();
        let interior = (0x4000000 + 19 * memaddr::PAGE_SIZE_4K).into();
        assert_eq!(
            edit.query(interior).unwrap().0,
            pa + 19 * memaddr::PAGE_SIZE_4K
        );
        drop(owner);
        assert_eq!(
            kalloc::global_allocator().usages().get(UsageKind::VirtMem),
            before + HUGE
        );
        edit.unmap(interior).unwrap();
        // The mapping was the only remaining owner. Even for independent exact
        // pages, unmap must retain the whole leaf until completion is confirmed.
        assert_eq!(
            kalloc::global_allocator().usages().get(UsageKind::VirtMem),
            before + HUGE
        );
        edit.try_finish().unwrap();
        assert_eq!(
            kalloc::global_allocator().usages().get(UsageKind::VirtMem),
            before
        );
    }
}

#[def_test(serial)]
fn inactive_table_destruction_releases_its_live_data_hold() {
    let _guard = NoPreemptIrqSave::new();
    let before = kalloc::global_allocator().usages().get(UsageKind::VirtMem);
    let mut table = PageTable::try_new().unwrap();
    let page = alloc_page_ref(UsageKind::VirtMem).unwrap();
    table
        .modify()
        .map(0x8000000usize.into(), page, MappingFlags::READ)
        .unwrap();
    assert_eq!(
        kalloc::global_allocator().usages().get(UsageKind::VirtMem),
        before + memaddr::PAGE_SIZE_4K
    );
    // This root was never activated; destruction must recover its live leaf's
    // transferred PageRef before releasing the descriptor storage.
    drop(table);
    assert_eq!(
        kalloc::global_allocator().usages().get(UsageKind::VirtMem),
        before
    );
}

#[def_test(serial)]
fn managed_alias_retains_real_page_after_source_retirement() {
    let _guard = NoPreemptIrqSave::new();
    let before = kalloc::global_allocator().usages().get(UsageKind::VirtMem);
    let mut table = PageTable::try_new().unwrap();
    let page = alloc_page_ref(UsageKind::VirtMem).unwrap();
    let pa = page.phys_addr();
    let source = 0x9000000usize.into();
    let alias = 0x9001000usize.into();
    let mut edit = table.modify();
    edit.map(source, page, MappingFlags::READ).unwrap();
    edit.try_finish().unwrap();
    edit.map_existing(source, alias).unwrap();
    edit.unmap(source).unwrap();
    edit.try_finish().unwrap();
    assert_eq!(edit.query(alias).unwrap().0, pa);
    assert_eq!(
        kalloc::global_allocator().usages().get(UsageKind::VirtMem),
        before + memaddr::PAGE_SIZE_4K
    );
    edit.unmap(alias).unwrap();
    assert_eq!(
        kalloc::global_allocator().usages().get(UsageKind::VirtMem),
        before + memaddr::PAGE_SIZE_4K
    );
    edit.try_finish().unwrap();
    assert_eq!(
        kalloc::global_allocator().usages().get(UsageKind::VirtMem),
        before
    );
}

#[def_test(serial)]
fn managed_to_external_replacement_keeps_raw_target_externally_owned() {
    let _guard = NoPreemptIrqSave::new();
    let before = kalloc::global_allocator().usages().get(UsageKind::VirtMem);
    let mut table = PageTable::try_new().unwrap();
    let source = alloc_page_ref(UsageKind::VirtMem).unwrap();
    let target = alloc_page_ref(UsageKind::VirtMem).unwrap();
    let target_pa = target.phys_addr();
    let address = 0xa000000usize.into();
    let mut edit = table.modify();
    edit.map(address, source, MappingFlags::READ).unwrap();
    edit.try_finish().unwrap();
    let expected = edit.query_entry(address).unwrap();
    // SAFETY: target retains this exact allocation until the inactive raw leaf
    // is removed and its retirement completes below. It is not consumed by PTE
    // publication or treated as an unpublished candidate after success.
    unsafe {
        edit.replace_managed_with_external_if_same(
            address,
            expected,
            target_pa,
            MappingFlags::READ | MappingFlags::WRITE,
        )
    }
    .unwrap();
    assert_eq!(edit.query(address).unwrap().0, target_pa);
    assert_eq!(
        kalloc::global_allocator().usages().get(UsageKind::VirtMem),
        before + 2 * memaddr::PAGE_SIZE_4K
    );
    edit.try_finish().unwrap();
    assert_eq!(
        kalloc::global_allocator().usages().get(UsageKind::VirtMem),
        before + memaddr::PAGE_SIZE_4K
    );
    edit.unmap(address).unwrap();
    edit.try_finish().unwrap();
    assert_eq!(
        kalloc::global_allocator().usages().get(UsageKind::VirtMem),
        before + memaddr::PAGE_SIZE_4K
    );
    drop(target);
    assert_eq!(
        kalloc::global_allocator().usages().get(UsageKind::VirtMem),
        before
    );
}

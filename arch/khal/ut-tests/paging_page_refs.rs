// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kalloc::{PageRef, UsageKind, alloc_page_ref};
use page_table::PagingHandler;
use kspin::NoPreemptIrqSave;
use unittest::def_test;

use super::{MappingFlags, PageSize, PageTable, PagingHandlerImpl};

#[def_test]
fn handler_keeps_the_transferred_reference_until_table_retirement() {
    let _guard = NoPreemptIrqSave::new();
    let pa = PagingHandlerImpl::alloc_frame().unwrap();
    // SAFETY: consumes this handler's exact transfer before any table/hardware
    // use; after cloning we transfer its original hold back to the handler.
    let owner = unsafe { PageRef::from_raw(pa) };
    let observer = owner.try_clone_ref().unwrap();
    assert_eq!(owner.into_raw(), pa);
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
    edit.map(address, backing.phys_addr(), PageSize::Size4K, MappingFlags::READ).unwrap();
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
        edit.map(address, backing.phys_addr(), PageSize::Size4K, MappingFlags::READ).unwrap();
        edit.finish();
    }
    target.modify().copy_from(&source, address, memaddr::PAGE_SIZE_4K);
    assert_eq!(target.query(address).unwrap().0, backing.phys_addr());
    // Both tables are inactive. The source outlives the target; dropping the
    // borrower must not consume the source's raw table-storage references.
    drop(target);
    assert_eq!(source.query(address).unwrap().0, backing.phys_addr());
    source.modify().unmap(address).unwrap();
    drop(source);
}

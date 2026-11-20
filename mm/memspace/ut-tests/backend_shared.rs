// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use anon::AnonSharedObject;
use khal::paging::PageSize;
use memaddr::va;
use unittest::def_test;

use super::{SharedBackend, SharedPages};
use crate::{VmBackingKind, backend::BackendOps};

#[def_test]
fn shared_backend_reuses_explicit_shared_owner_identity() {
    let object = AnonSharedObject::new();
    let pages = Arc::new(SharedPages::new(PageSize::Size4K as usize, PageSize::Size4K).unwrap());
    let left = SharedBackend::new(va!(0x1000), pages.clone(), object.clone());
    let right = SharedBackend::new(va!(0x2000), pages, object.clone());
    let VmBackingKind::AnonymousShared { object: left_id } = left.backing_info().kind() else {
        panic!("expected anonymous shared backing");
    };
    let VmBackingKind::AnonymousShared { object: right_id } = right.backing_info().kind() else {
        panic!("expected anonymous shared backing");
    };
    assert_eq!(left_id, object.id());
    assert_eq!(right_id, object.id());
}

#[def_test(serial)]
fn shared_alias_ptes_keep_storage_after_runtime_drop() {
    use khal::paging::{MappingFlags, PageTable};
    use memaddr::{PAGE_SIZE_4K, VirtAddrRange};

    let initial = kalloc::global_allocator().usages().get(kalloc::UsageKind::VirtMem);
    let first = va!(0x4000);
    let second = va!(0x8000);
    let pages = Arc::new(SharedPages::new(PAGE_SIZE_4K, PageSize::Size4K).unwrap());
    let object = AnonSharedObject::new();
    let left = SharedBackend::new(first, pages.clone(), object.clone());
    let right = SharedBackend::new(second, pages, object);
    let mut table = PageTable::try_new().unwrap();
    let mut edit = table.modify();
    let flags = MappingFlags::READ | MappingFlags::WRITE;
    BackendOps::map(&left, VirtAddrRange::from_start_size(first, PAGE_SIZE_4K), flags, &mut edit).unwrap();
    BackendOps::map(&right, VirtAddrRange::from_start_size(second, PAGE_SIZE_4K), flags, &mut edit).unwrap();
    edit.try_finish().unwrap();
    let physical = edit.query(first).unwrap().0;
    assert_eq!(edit.query(second).unwrap().0, physical);
    drop(left);
    drop(right);
    edit.unmap(first).unwrap();
    edit.try_finish().unwrap();
    assert_eq!(edit.query(second).unwrap().0, physical);
    assert_eq!(kalloc::global_allocator().usages().get(kalloc::UsageKind::VirtMem), initial + PAGE_SIZE_4K);
    edit.unmap(second).unwrap();
    assert_eq!(kalloc::global_allocator().usages().get(kalloc::UsageKind::VirtMem), initial + PAGE_SIZE_4K);
    edit.try_finish().unwrap();
    assert_eq!(kalloc::global_allocator().usages().get(kalloc::UsageKind::VirtMem), initial);
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{ptr, slice};

use khal::{
    mem::{PhysAddr, p2v},
    paging::{MappingFlags, PageSize, PageTable},
    trap::PageFaultFlags,
};
use memaddr::{PAGE_SIZE_4K, VirtAddr, VirtAddrRange};
use unittest::def_test;

use super::{
    BackendOps, PrivateBackend, PrivateForkMaterialize, clone_private_object_pages_for_fork,
    cow_private_object_page, map_existing_private_object_page,
};
use crate::{FaultContext, VmArea, vma::VmRuntimeOps};

fn fill_frame_prefix(pa: PhysAddr, value: u8, len: usize) {
    assert!(len <= PAGE_SIZE_4K);
    // SAFETY: Unit tests only pass allocated frames that remain owned by an
    // AnonPrivateObject for at least `len` bytes; the assertion above
    // keeps the test prefix within the 4K frame.
    unsafe {
        ptr::write_bytes(p2v(pa).as_mut_ptr(), value, len);
    }
}

fn assert_frame_prefix(pa: PhysAddr, value: u8, len: usize) {
    assert!(len <= PAGE_SIZE_4K);
    // SAFETY: Unit tests only read allocated frames that remain owned by an
    // AnonPrivateObject for at least `len` bytes; the assertion above
    // keeps the test prefix within the 4K frame.
    let bytes = unsafe { slice::from_raw_parts(p2v(pa).as_ptr(), len) };
    assert!(
        bytes.iter().all(|byte| *byte == value),
        "frame prefix should contain byte {value:#x}"
    );
}

#[def_test]
fn private_backend_madvise_then_fork_then_write_fault_refaults_clean_child_page() {
    let start = VirtAddr::from_usize(0x40_0000);
    let range = VirtAddrRange::from_start_size(start, PAGE_SIZE_4K);
    let flags = MappingFlags::READ | MappingFlags::WRITE;
    let backend = PrivateBackend::new(start, PageSize::Size4K);
    let vma = VmArea::new(
        start,
        PAGE_SIZE_4K,
        flags,
        flags,
        BackendOps::backing_info(&backend),
        0,
        None,
    );
    let mut parent_pt = PageTable::try_new().expect("allocate parent page table");

    {
        let mut pgtbl = parent_pt.modify();
        let fault = FaultContext::new(start, PageFaultFlags::WRITE);
        let completion =
            BackendOps::handle_fault(&backend, fault, flags, &mut pgtbl).expect("fault in");
        assert_eq!(
            completion.populated(),
            1,
            "initial private fault should materialize one page"
        );
    }
    assert!(
        backend.object.page_at(0).is_some(),
        "parent object should own one page"
    );
    let parent_page = backend
        .object
        .page_at(0)
        .expect("parent page should exist before MADV_DONTNEED");
    fill_frame_prefix(parent_page.phys_addr(), 0x5a, 64);

    {
        let mut pgtbl = parent_pt.modify();
        let handled = VmRuntimeOps::madvise_dontneed(&backend, &vma, range, &mut pgtbl)
            .expect("madvise DONTNEED");
        assert!(handled, "private backend should handle MADV_DONTNEED");
    }
    assert!(
        backend.object.page_at(0).is_none(),
        "madvise DONTNEED should drop parent private page state"
    );
    {
        let pgtbl = parent_pt.modify();
        assert!(
            pgtbl.query(start).is_err(),
            "madvise DONTNEED should also tear down the present PTE"
        );
    }

    let child_object = backend.object.fork_child();
    let mut child_pt = PageTable::try_new().expect("allocate child page table");
    {
        let mut old_pgtbl = parent_pt.modify();
        let mut new_pgtbl = child_pt.modify();
        clone_private_object_pages_for_fork(
            &backend.object,
            &child_object,
            0,
            PAGE_SIZE_4K,
            PrivateForkMaterialize {
                runtime_start: start,
                object_base: 0,
                cow_flags: flags - MappingFlags::WRITE,
                page_size: PageSize::Size4K,
            },
            &mut old_pgtbl,
            &mut new_pgtbl,
        )
        .expect("fork after DONTNEED on empty parent object");
    }
    assert!(
        child_object.page_at(0).is_none(),
        "forking an already-discarded private range must not seed child page state"
    );

    let child = PrivateBackend {
        start,
        object_start: 0,
        size: PageSize::Size4K,
        object: child_object,
        object_view: None,
    };
    {
        let mut pgtbl = child_pt.modify();
        let fault = FaultContext::new(start, PageFaultFlags::WRITE);
        let completion =
            BackendOps::handle_fault(&child, fault, flags, &mut pgtbl).expect("child refault");
        assert_eq!(
            completion.populated(),
            1,
            "child refault should materialize one fresh page"
        );
    }
    let child_page = child
        .object
        .page_at(0)
        .expect("child should own a fresh private page after refault");
    assert!(
        child_page.is_exclusive(),
        "fresh child refault should produce an exclusive private page"
    );
    assert_frame_prefix(child_page.phys_addr(), 0, 64);
    assert!(
        backend.object.page_at(0).is_none(),
        "parent object should stay discarded after child refault"
    );
}

#[def_test]
fn private_backend_madvise_then_same_mapping_refaults_zero_page() {
    let start = VirtAddr::from_usize(0x44_0000);
    let range = VirtAddrRange::from_start_size(start, PAGE_SIZE_4K);
    let flags = MappingFlags::READ | MappingFlags::WRITE;
    let backend = PrivateBackend::new(start, PageSize::Size4K);
    let vma = VmArea::new(
        start,
        PAGE_SIZE_4K,
        flags,
        flags,
        BackendOps::backing_info(&backend),
        0,
        None,
    );
    let mut pgtbl_owner = PageTable::try_new().expect("allocate page table");

    {
        let mut pgtbl = pgtbl_owner.modify();
        let fault = FaultContext::new(start, PageFaultFlags::WRITE);
        let completion = BackendOps::handle_fault(&backend, fault, flags, &mut pgtbl)
            .expect("initial private fault");
        assert_eq!(completion.populated(), 1);
    }
    let old_page = backend
        .object
        .page_at(0)
        .expect("initial private page should exist");
    fill_frame_prefix(old_page.phys_addr(), 0xa5, 64);
    drop(old_page);

    {
        let mut pgtbl = pgtbl_owner.modify();
        let handled = VmRuntimeOps::madvise_dontneed(&backend, &vma, range, &mut pgtbl)
            .expect("madvise DONTNEED");
        assert!(handled);
    }
    assert!(
        backend.object.page_at(0).is_none(),
        "discard must remove the private object slot"
    );

    {
        let mut pgtbl = pgtbl_owner.modify();
        let fault = FaultContext::new(start, PageFaultFlags::WRITE);
        let completion = BackendOps::handle_fault(&backend, fault, flags, &mut pgtbl)
            .expect("same mapping refault");
        assert_eq!(completion.populated(), 1);
    }
    let new_page = backend
        .object
        .page_at(0)
        .expect("same mapping should own a refaulted private page");
    assert!(new_page.is_exclusive());
    assert_frame_prefix(new_page.phys_addr(), 0, 64);
}

#[def_test]
fn private_fork_rolls_back_parent_write_protect_when_child_map_fails() {
    let start = VirtAddr::from_usize(0x50_0000);
    let flags = MappingFlags::READ | MappingFlags::WRITE;
    let backend = PrivateBackend::new(start, PageSize::Size4K);
    let mut parent_pt = PageTable::try_new().expect("allocate parent page table");

    {
        let mut pgtbl = parent_pt.modify();
        let fault = FaultContext::new(start, PageFaultFlags::WRITE);
        BackendOps::handle_fault(&backend, fault, flags, &mut pgtbl).expect("fault in");
    }
    let parent_page = backend
        .object
        .page_at(0)
        .expect("parent should own one page");
    assert!(parent_page.is_exclusive());

    let child_object = backend.object.fork_child();
    let mut child_pt = PageTable::try_new().expect("allocate child page table");
    {
        let mut pgtbl = child_pt.modify();
        pgtbl
            .map(
                start,
                PhysAddr::from_usize(0x80_0000),
                PageSize::Size4K,
                flags,
            )
            .expect("seed conflicting child mapping");
    }

    {
        let mut old_pgtbl = parent_pt.modify();
        let mut new_pgtbl = child_pt.modify();
        assert!(
            clone_private_object_pages_for_fork(
                &backend.object,
                &child_object,
                0,
                PAGE_SIZE_4K,
                PrivateForkMaterialize {
                    runtime_start: start,
                    object_base: 0,
                    cow_flags: flags - MappingFlags::WRITE,
                    page_size: PageSize::Size4K,
                },
                &mut old_pgtbl,
                &mut new_pgtbl,
            )
            .is_err(),
            "fork clone must fail on pre-existing child PTE"
        );
    }

    let (_pa, parent_flags, _size) = parent_pt
        .modify()
        .query(start)
        .expect("parent mapping must remain installed");
    assert!(
        parent_flags.contains(MappingFlags::WRITE),
        "rollback must restore parent write permission"
    );
    assert!(
        child_object.page_at(0).is_none(),
        "failed fork must not publish child object page state"
    );
    assert!(
        parent_page.is_exclusive(),
        "prepared fork references must be released on rollback"
    );
}

#[def_test]
fn private_cow_write_fault_replaces_parent_and_preserves_child_page() {
    let start = VirtAddr::from_usize(0x60_0000);
    let flags = MappingFlags::READ | MappingFlags::WRITE;
    let backend = PrivateBackend::new(start, PageSize::Size4K);
    let mut parent_pt = PageTable::try_new().expect("allocate parent page table");

    {
        let mut pgtbl = parent_pt.modify();
        let fault = FaultContext::new(start, PageFaultFlags::WRITE);
        BackendOps::handle_fault(&backend, fault, flags, &mut pgtbl).expect("fault in");
    }
    let original_page = backend
        .object
        .page_at(0)
        .expect("parent should own one page");
    let original_pa = original_page.phys_addr();

    let child_object = backend.object.fork_child();
    let mut child_pt = PageTable::try_new().expect("allocate child page table");
    {
        let mut old_pgtbl = parent_pt.modify();
        let mut new_pgtbl = child_pt.modify();
        clone_private_object_pages_for_fork(
            &backend.object,
            &child_object,
            0,
            PAGE_SIZE_4K,
            PrivateForkMaterialize {
                runtime_start: start,
                object_base: 0,
                cow_flags: flags - MappingFlags::WRITE,
                page_size: PageSize::Size4K,
            },
            &mut old_pgtbl,
            &mut new_pgtbl,
        )
        .expect("fork private page");
    }
    assert!(
        !original_page.is_exclusive(),
        "fork should share the original private page"
    );

    {
        let mut pgtbl = parent_pt.modify();
        let fault = FaultContext::new(start, PageFaultFlags::WRITE);
        let completion =
            BackendOps::handle_fault(&backend, fault, flags, &mut pgtbl).expect("COW fault");
        assert_eq!(completion.populated(), 1);
    }

    let parent_page = backend
        .object
        .page_at(0)
        .expect("parent should publish replacement page");
    let child_page = child_object
        .page_at(0)
        .expect("child should keep original shared page");
    assert_ne!(parent_page.phys_addr(), original_pa);
    assert_eq!(child_page.phys_addr(), original_pa);
    assert!(parent_page.is_exclusive());
    assert!(child_page.is_exclusive());

    let (parent_pa, parent_flags, _) = parent_pt
        .modify()
        .query(start)
        .expect("parent PTE should remain mapped");
    assert_eq!(parent_pa, parent_page.phys_addr());
    assert!(parent_flags.contains(MappingFlags::WRITE));

    let (child_pa, child_flags, _) = child_pt
        .modify()
        .query(start)
        .expect("child PTE should remain mapped");
    assert_eq!(child_pa, original_pa);
    assert!(!child_flags.contains(MappingFlags::WRITE));
}

#[def_test]
fn private_cow_write_fault_reports_retry_when_pte_changed() {
    let start = VirtAddr::from_usize(0x70_0000);
    let flags = MappingFlags::READ | MappingFlags::WRITE;
    let backend = PrivateBackend::new(start, PageSize::Size4K);
    let mut parent_pt = PageTable::try_new().expect("allocate parent page table");

    {
        let mut pgtbl = parent_pt.modify();
        let fault = FaultContext::new(start, PageFaultFlags::WRITE);
        BackendOps::handle_fault(&backend, fault, flags, &mut pgtbl).expect("fault in");
    }
    let original_page = backend
        .object
        .page_at(0)
        .expect("parent should own one page");
    let child_object = backend.object.fork_child();
    let mut child_pt = PageTable::try_new().expect("allocate child page table");
    {
        let mut old_pgtbl = parent_pt.modify();
        let mut new_pgtbl = child_pt.modify();
        clone_private_object_pages_for_fork(
            &backend.object,
            &child_object,
            0,
            PAGE_SIZE_4K,
            PrivateForkMaterialize {
                runtime_start: start,
                object_base: 0,
                cow_flags: flags - MappingFlags::WRITE,
                page_size: PageSize::Size4K,
            },
            &mut old_pgtbl,
            &mut new_pgtbl,
        )
        .expect("fork private page");

        let replacement = PhysAddr::from_usize(0x90_0000);
        old_pgtbl
            .remap(start, replacement, flags - MappingFlags::WRITE)
            .expect("simulate competing PTE replacement");
        let result = cow_private_object_page(
            &backend.object,
            0,
            start,
            flags,
            PageSize::Size4K,
            &mut old_pgtbl,
        )
        .expect("COW helper should convert changed PTE to retry");
        assert!(result.is_retry());
    }

    let current_page = backend
        .object
        .page_at(0)
        .expect("object slot should remain original");
    assert_eq!(current_page.phys_addr(), original_page.phys_addr());
    assert_eq!(
        parent_pt
            .modify()
            .query(start)
            .expect("competing PTE should remain installed")
            .0,
        PhysAddr::from_usize(0x90_0000)
    );
}

#[def_test]
fn private_cow_unmapped_write_fault_reports_retry_when_child_map_conflicts() {
    let start = VirtAddr::from_usize(0x80_0000);
    let flags = MappingFlags::READ | MappingFlags::WRITE;
    let backend = PrivateBackend::new(start, PageSize::Size4K);
    let mut parent_pt = PageTable::try_new().expect("allocate parent page table");

    {
        let mut pgtbl = parent_pt.modify();
        let fault = FaultContext::new(start, PageFaultFlags::WRITE);
        BackendOps::handle_fault(&backend, fault, flags, &mut pgtbl).expect("fault in");
    }
    let original_page = backend
        .object
        .page_at(0)
        .expect("parent should own one page");
    let child_object = backend.object.fork_child();
    let mut child_pt = PageTable::try_new().expect("allocate child page table");
    {
        let mut old_pgtbl = parent_pt.modify();
        let mut new_pgtbl = child_pt.modify();
        clone_private_object_pages_for_fork(
            &backend.object,
            &child_object,
            0,
            PAGE_SIZE_4K,
            PrivateForkMaterialize {
                runtime_start: start,
                object_base: 0,
                cow_flags: flags - MappingFlags::WRITE,
                page_size: PageSize::Size4K,
            },
            &mut old_pgtbl,
            &mut new_pgtbl,
        )
        .expect("fork private page");
    }

    let mut child_retry_pt = PageTable::try_new().expect("allocate retry page table");
    {
        let mut pgtbl = child_retry_pt.modify();
        pgtbl
            .map(
                start,
                PhysAddr::from_usize(0xa0_0000),
                PageSize::Size4K,
                flags - MappingFlags::WRITE,
            )
            .expect("simulate concurrent PTE install");
        let result = map_existing_private_object_page(
            &child_object,
            0,
            start,
            MappingFlags::WRITE,
            flags,
            PageSize::Size4K,
            &mut pgtbl,
        )
        .expect("existing private page helper");
        assert!(result.is_retry());
    }

    assert_eq!(
        child_object
            .page_at(0)
            .expect("child object slot should remain shared")
            .phys_addr(),
        original_page.phys_addr()
    );
    assert!(
        !original_page.is_exclusive(),
        "retry must not drop existing shared refs"
    );
}

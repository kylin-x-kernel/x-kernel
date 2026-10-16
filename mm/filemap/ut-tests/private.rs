// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec;
use core::slice;

use anon::AnonPrivateObject;
use khal::{
    mem::p2v,
    paging::{MappingFlags, PageSize, PageTable},
    trap::PageFaultFlags,
};
use memaddr::{PAGE_SIZE_4K, VirtAddr, VirtAddrRange};
use memspace::{
    FaultContext, VmArea, VmRuntimeOps,
    backend::private::{PrivateForkMaterialize, clone_private_object_pages_for_fork},
};
use unittest::def_test;

use super::FilePrivateRuntime;
use crate::test_support::page_cache_file;

fn assert_frame_prefix(pa: memaddr::PhysAddr, value: u8, len: usize) {
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
fn file_private_fork_child_keeps_lineage_and_splits_anon_object() {
    let file = page_cache_file("cow-lineage");
    let parent = FilePrivateRuntime {
        start: VirtAddr::from_usize(0x4000),
        len: PAGE_SIZE_4K,
        size: PageSize::Size4K,
        anon: AnonPrivateObject::new_root(),
        file,
        file_start: 0,
        file_end: None,
        mapping_view: None,
        anon_view: None,
    };

    let parent_kind = parent.backing_info_impl().kind();
    let child = parent.rebuilt_runtime(VirtAddr::from_usize(0x8000), None, None, None, true);
    let child_kind = child.backing_info_impl().kind();

    let (
        super::VmBackingKind::FilePrivate {
            file_object: parent_file,
            anon_object: parent_anon,
            anon_lineage: parent_lineage,
        },
        super::VmBackingKind::FilePrivate {
            file_object: child_file,
            anon_object: child_anon,
            anon_lineage: child_lineage,
        },
    ) = (parent_kind, child_kind)
    else {
        panic!("expected file-private backing kinds");
    };

    assert_eq!(parent_file, child_file);
    assert_eq!(parent_lineage, child_lineage);
    assert_ne!(parent_anon, child_anon);
}

#[def_test]
fn file_private_fork_madvise_then_write_fault_preserves_file_source_and_splits_anon_pages() {
    let file = page_cache_file("cow-fork-madvise-write");
    let payload = vec![0x41u8; PAGE_SIZE_4K];
    let mut pos = 0;
    let written = file
        .write_from(&payload[..], &mut pos)
        .expect("seed cached file with one page");
    assert_eq!(written, PAGE_SIZE_4K);

    let start = VirtAddr::from_usize(0x5000);
    let range = VirtAddrRange::from_start_size(start, PAGE_SIZE_4K);
    let flags = MappingFlags::READ | MappingFlags::WRITE;
    let parent = FilePrivateRuntime {
        start,
        len: PAGE_SIZE_4K,
        size: PageSize::Size4K,
        anon: AnonPrivateObject::new_root(),
        file,
        file_start: 0,
        file_end: Some(PAGE_SIZE_4K as u64),
        mapping_view: None,
        anon_view: None,
    };
    let mut parent_pt = PageTable::try_new().expect("allocate parent page table");

    {
        let mut pgtbl = parent_pt.modify();
        let fault =
            FaultContext::new(start, PageFaultFlags::WRITE).with_backing(Some(0), Some(0), Some(0));
        let completion = VmRuntimeOps::handle_fault(&parent, fault, flags, &mut pgtbl)
            .expect("parent first fault");
        assert_eq!(
            completion.populated(),
            1,
            "file-private first fault should materialize one anon-backed page"
        );
    }

    let parent_kind = parent.backing_info_impl().kind();
    let child = parent.rebuilt_runtime(start, None, None, None, true);
    let child_kind = child.backing_info_impl().kind();
    let (
        super::VmBackingKind::FilePrivate {
            file_object: parent_file,
            anon_object: parent_anon,
            anon_lineage: parent_lineage,
        },
        super::VmBackingKind::FilePrivate {
            file_object: child_file,
            anon_object: child_anon,
            anon_lineage: child_lineage,
        },
    ) = (parent_kind, child_kind)
    else {
        panic!("expected file-private backing kinds");
    };
    assert_eq!(parent_file, child_file);
    assert_eq!(parent_lineage, child_lineage);
    assert_ne!(parent_anon, child_anon);

    let mut child_pt = PageTable::try_new().expect("allocate child page table");
    {
        let mut old_pgtbl = parent_pt.modify();
        let mut new_pgtbl = child_pt.modify();
        clone_private_object_pages_for_fork(
            &parent.anon,
            &child.anon,
            0,
            PAGE_SIZE_4K,
            PrivateForkMaterialize {
                runtime_start: start,
                cow_flags: flags - MappingFlags::WRITE,
                page_size: PageSize::Size4K,
            },
            &mut old_pgtbl,
            &mut new_pgtbl,
        )
        .expect("fork file-private anon side");
    }
    let shared_child_page = child
        .anon
        .page_at(0)
        .expect("child should inherit one shared private page");
    let shared_pa = shared_child_page.phys_addr();

    let vma = VmArea::new(
        start,
        PAGE_SIZE_4K,
        flags,
        flags,
        parent.backing_info_impl(),
        0,
        None,
    );
    {
        let mut pgtbl = parent_pt.modify();
        let handled = VmRuntimeOps::madvise_dontneed(&parent, &vma, range, &mut pgtbl)
            .expect("parent MADV_DONTNEED");
        assert!(handled, "file-private runtime should handle MADV_DONTNEED");
    }
    assert!(
        parent.anon.page_at(0).is_none(),
        "parent MADV_DONTNEED should discard its private anon page"
    );

    {
        let mut pgtbl = child_pt.modify();
        let fault = FaultContext::new(start, PageFaultFlags::WRITE);
        let completion = VmRuntimeOps::handle_fault(&*child, fault, flags, &mut pgtbl)
            .expect("child write fault");
        assert_eq!(
            completion.populated(),
            1,
            "child write fault should complete via shared helper"
        );
    }
    let child_page = child
        .anon
        .page_at(0)
        .expect("child should still own a private page after write fault");
    assert!(
        shared_pa != child_page.phys_addr() || child_page.is_exclusive(),
        "child write fault should either split or retain the inherited page as the sole owner"
    );
    assert!(child_page.is_exclusive());

    {
        let mut pgtbl = parent_pt.modify();
        let fault =
            FaultContext::new(start, PageFaultFlags::READ).with_backing(Some(0), Some(0), Some(0));
        let completion =
            VmRuntimeOps::handle_fault(&parent, fault, flags, &mut pgtbl).expect("parent refault");
        assert_eq!(
            completion.populated(),
            1,
            "parent refault after MADV_DONTNEED should rematerialize from file source"
        );
    }
    let parent_page = parent
        .anon
        .page_at(0)
        .expect("parent should own a new private page after refault");
    assert_ne!(parent_page.phys_addr(), child_page.phys_addr());
    assert!(parent_page.is_exclusive());
    assert_frame_prefix(parent_page.phys_addr(), 0x41, 64);
    assert_eq!(parent.backing_info_impl().kind(), parent_kind);
    assert_eq!(child.backing_info_impl().kind(), child_kind);

    {
        let mut pgtbl = parent_pt.modify();
        parent
            .unmap_impl(range, &mut pgtbl)
            .expect("cleanup parent mapping");
    }
    {
        let mut pgtbl = child_pt.modify();
        child
            .unmap_impl(range, &mut pgtbl)
            .expect("cleanup child mapping");
    }
}

#[def_test]
fn private_refault_rechecks_i_size_before_reusing_retained_cow_page() {
    let file = page_cache_file("truncate-private-refault");
    let payload = vec![0x5au8; PAGE_SIZE_4K * 2];
    let mut pos = 0;
    file.write_from(&payload, &mut pos)
        .expect("seed two cached pages");

    let start = VirtAddr::from_usize(0x20_000);
    let fault_addr = start + PAGE_SIZE_4K;
    let flags = MappingFlags::READ | MappingFlags::WRITE;
    let runtime = FilePrivateRuntime {
        start,
        len: PAGE_SIZE_4K * 2,
        size: PageSize::Size4K,
        anon: AnonPrivateObject::new_root(),
        file: file.clone(),
        file_start: 0,
        file_end: None,
        mapping_view: None,
        anon_view: None,
    };
    let mut page_table = PageTable::try_new().expect("allocate page table");
    let fault = FaultContext::new(fault_addr, PageFaultFlags::READ).with_backing(
        Some(1),
        Some(PAGE_SIZE_4K as u64),
        Some(0),
    );

    {
        let mut pgtbl = page_table.modify();
        VmRuntimeOps::handle_fault(&runtime, fault, flags, &mut pgtbl)
            .expect("initial private fault");
    }
    file.truncate(0).expect("truncate file to zero");
    {
        let mut pgtbl = page_table.modify();
        let (_, _, page_size) = pgtbl.unmap(fault_addr).expect("remove racing private PTE");
        assert_eq!(page_size, PageSize::Size4K);
        pgtbl.finish();
    }
    assert!(
        runtime.anon.page_at(PAGE_SIZE_4K as u64).is_some(),
        "PTE invalidation alone intentionally leaves retained Rust object state"
    );

    {
        let mut pgtbl = page_table.modify();
        assert!(
            VmRuntimeOps::handle_fault(&runtime, fault, flags, &mut pgtbl).is_err(),
            "refault after truncate must not reuse the retained private page"
        );
        runtime
            .unmap_impl(
                VirtAddrRange::from_start_size(fault_addr, PAGE_SIZE_4K),
                &mut pgtbl,
            )
            .expect("release retained private page");
    }
}

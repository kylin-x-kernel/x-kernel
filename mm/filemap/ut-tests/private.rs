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
    backend::{
        alloc_frame, dealloc_frame,
        private::{PrivateForkMaterialize, clone_private_object_pages_for_fork},
    },
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
        anon_start: 0,
        file_pages: alloc::sync::Arc::new(ksync::Mutex::new(alloc::collections::BTreeMap::new())),
        file,
        file_start: 0,
        file_end: None,
        mapping_view: None,
        anon_view: None,
    };

    let parent_kind = parent.backing_info_impl().kind();
    let child = parent
        .rebuilt_runtime(
            VirtAddrRange::from_start_size(parent.start, parent.len),
            VirtAddrRange::from_start_size(VirtAddr::from_usize(0x8000), parent.len),
            None,
            None,
            None,
            true,
        )
        .expect("rebuild fork runtime");
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
        anon_start: 0,
        file_pages: alloc::sync::Arc::new(ksync::Mutex::new(alloc::collections::BTreeMap::new())),
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
    let child = parent
        .rebuilt_runtime(
            range,
            VirtAddrRange::from_start_size(start, parent.len),
            None,
            None,
            None,
            true,
        )
        .expect("rebuild fork runtime");
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
                object_base: 0,
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
    assert!(parent.anon.page_at(0).is_none());
    let parent_page = parent
        .file_pages
        .lock()
        .get(&0)
        .cloned()
        .expect("file-owned readonly page");
    let parent_page_paddr = parent_pt
        .query(start)
        .expect("file-owned page remains mapped")
        .0;
    assert!(!parent_page.matches_paddr(child_page.phys_addr()));
    assert_frame_prefix(parent_page_paddr, 0x41, 64);
    drop(parent_page);
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
        anon_start: 0,
        file_pages: alloc::sync::Arc::new(ksync::Mutex::new(alloc::collections::BTreeMap::new())),
        file: file.clone(),
        file_start: 0,
        file_end: None,
        mapping_view: None,
        anon_view: None,
    };
    let mut page_table = PageTable::try_new().expect("allocate page table");
    let fault = FaultContext::new(fault_addr, PageFaultFlags::WRITE).with_backing(
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

fn mapped_readonly_cache_pages(
    name: &str,
    count: usize,
) -> (
    alloc::sync::Arc<FilePrivateRuntime>,
    alloc::sync::Arc<ksync::Mutex<memspace::MmSpace>>,
    alloc::sync::Arc<kvfs::VfsFile>,
) {
    use alloc::{collections::BTreeMap, sync::Arc};
    let file = page_cache_file(name);
    file.write_from(&vec![0x65; count * PAGE_SIZE_4K], &mut 0)
        .expect("seed file pages");
    let mm = Arc::new(ksync::Mutex::new(
        memspace::MmSpace::new_user_empty().expect("new mm"),
    ));
    let observer = memspace::MmObserver::new(&mm);
    let mm_id = mm.lock().mm_id();
    let context = crate::runtime::FileRuntimeContext {
        mm_id,
        observer: &observer,
        invalidate: observer.invalidate_handle(),
    };
    let start = VirtAddr::from_usize(0x400_000);
    let len = count * PAGE_SIZE_4K;
    let flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
    let runtime = Arc::new(FilePrivateRuntime {
        start,
        len,
        size: PageSize::Size4K,
        anon: AnonPrivateObject::new_root(),
        anon_start: 0,
        file_pages: Arc::new(ksync::Mutex::new(BTreeMap::new())),
        file: file.clone(),
        file_start: 0,
        file_end: None,
        mapping_view: FilePrivateRuntime::register_mapping_view(
            &file,
            0,
            None,
            Some(&context),
            start,
            len,
        ),
        anon_view: None,
    });
    let vma = VmArea::new(
        start,
        len,
        flags,
        flags,
        runtime.backing_info_impl(),
        0,
        Some(memspace::FileMappingInfo {
            offset: 0,
            inode: file.inode().inode(),
            path: None,
        }),
    );
    mm.lock()
        .map_runtime_vma(
            vma,
            false,
            memspace::VmRuntimeRef::new_file_private(runtime.clone()),
        )
        .expect("install test mapping");
    for page in 0..count {
        assert!(
            mm.lock()
                .handle_page_fault(start + page * PAGE_SIZE_4K, PageFaultFlags::READ)
                .is_resolved()
        );
    }
    (runtime, mm, file)
}

#[def_test]
fn truncate_zero_releases_file_pins_without_another_fault() {
    let (runtime, mm, file) = mapped_readonly_cache_pages("truncate-release-without-refault", 2);
    assert!(runtime.anon.page_at(0).is_none());
    assert!(runtime.anon.page_at(PAGE_SIZE_4K as u64).is_none());
    file.truncate(0).expect("truncate mapped file");
    // No page fault is issued after truncate: invalidation itself releases pins.
    assert!(runtime.file_pages.lock().is_empty());
    mm.lock()
        .unmap(runtime.start, runtime.len)
        .expect("remove VMA");
}

#[def_test]
fn deferred_truncate_retains_pin_until_mm_drains_pte_invalidation() {
    let (runtime, mm, file) = mapped_readonly_cache_pages("deferred-truncate-pin", 1);
    {
        let guard = mm.lock();
        file.truncate(0).expect("truncate while MM is busy");
        assert!(runtime.file_pages.lock().contains_key(&0), "pending PTE must retain its pin");
        drop(guard);
    }
    mm.lock()
        .invalidate_present(runtime.start, runtime.len)
        .expect("drain invalidation");
    assert!(!runtime.file_pages.lock().contains_key(&0));
    mm.lock()
        .unmap(runtime.start, runtime.len)
        .expect("remove VMA");
}

#[def_test]
fn readonly_file_page_enters_anon_only_after_cow_and_does_not_dirty_file() {
    let (runtime, mm, file) = mapped_readonly_cache_pages("file-pin-to-owned-cow", 1);
    assert!(runtime.anon.page_at(0).is_none());
    assert!(
        mm.lock()
            .handle_page_fault(runtime.start, PageFaultFlags::WRITE)
            .is_resolved()
    );
    assert!(!runtime.file_pages.lock().contains_key(&0));
    let page = runtime.anon.page_at(0).expect("COW anonymous page");
    assert!(page.is_exclusive());
    assert_frame_prefix(page.phys_addr(), 0x65, PAGE_SIZE_4K);
    // SAFETY: the MM retains this exclusive anonymous COW page and no user
    // task can execute the test-owned mapping concurrently.
    unsafe {
        p2v(page.phys_addr()).as_mut_ptr().write(0x99);
    }
    let mut actual = [0];
    file.read_from(&mut actual, &mut 0)
        .expect("read original file");
    assert_eq!(actual, [0x65]);
    mm.lock()
        .unmap(runtime.start, runtime.len)
        .expect("release COW mapping");
}

#[def_test]
fn file_private_cow_rejects_a_pte_from_a_different_folio() {
    let file = page_cache_file("file-cow-unrelated-pte");
    file.write_from(&vec![0x65; PAGE_SIZE_4K], &mut 0)
        .expect("seed file page");
    let start = VirtAddr::from_usize(0x600_000);
    let range = VirtAddrRange::from_start_size(start, PAGE_SIZE_4K);
    let flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
    let runtime = FilePrivateRuntime {
        start,
        len: PAGE_SIZE_4K,
        size: PageSize::Size4K,
        anon: AnonPrivateObject::new_root(),
        anon_start: 0,
        file_pages: alloc::sync::Arc::new(ksync::Mutex::new(alloc::collections::BTreeMap::new())),
        file,
        file_start: 0,
        file_end: None,
        mapping_view: None,
        anon_view: None,
    };
    let mut page_table = PageTable::try_new().expect("allocate page table");
    let wrong_frame = alloc_frame(false, PageSize::Size4K).expect("allocate unrelated frame");

    {
        let mut pgtbl = page_table.modify();
        let read_fault = FaultContext::new(start, PageFaultFlags::READ)
            .with_backing(Some(0), Some(0), Some(0));
        VmRuntimeOps::handle_fault(&runtime, read_fault, flags, &mut pgtbl)
            .expect("map readonly file page");

        pgtbl.unmap(start).expect("replace file PTE for test");
        pgtbl.finish();
        pgtbl
            .map(start, wrong_frame, PageSize::Size4K, flags - MappingFlags::WRITE)
            .expect("map unrelated frame");

        let write_fault = FaultContext::new(start, PageFaultFlags::WRITE);
        assert!(
            VmRuntimeOps::handle_fault(&runtime, write_fault, flags, &mut pgtbl).is_err(),
            "a third-frame mismatch must fail instead of retrying without progress"
        );
        runtime
            .unmap_impl(range, &mut pgtbl)
            .expect("remove test mapping and release retained file pin");
    }
    dealloc_frame(wrong_frame, PageSize::Size4K);
}

#[def_test]
fn file_private_fork_failure_does_not_commit_anonymous_child_references() {
    use alloc::{collections::BTreeMap, sync::Arc};
    let file = page_cache_file("file-fork-mixed-failure");
    file.write_from(&vec![0x65; PAGE_SIZE_4K * 2], &mut 0)
        .expect("seed mixed mapping");
    let start = VirtAddr::from_usize(0x800_000);
    let flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
    let range = VirtAddrRange::from_start_size(start, PAGE_SIZE_4K * 2);
    let runtime = FilePrivateRuntime {
        start,
        len: range.size(),
        size: PageSize::Size4K,
        anon: AnonPrivateObject::new_root(),
        anon_start: 0,
        file_pages: Arc::new(ksync::Mutex::new(BTreeMap::new())),
        file,
        file_start: 0,
        file_end: None,
        mapping_view: None,
        anon_view: None,
    };
    let mut parent_pt = PageTable::try_new().expect("parent page table");
    {
        let mut pgtbl = parent_pt.modify();
        for (addr, access, offset) in [
            (start, PageFaultFlags::WRITE, 0),
            (start + PAGE_SIZE_4K, PageFaultFlags::READ, PAGE_SIZE_4K as u64),
        ] {
            let fault = FaultContext::new(addr, access)
                .with_backing(Some(offset / PAGE_SIZE_4K as u64), Some(offset), Some(0));
            runtime.handle_fault(fault, flags, &mut pgtbl).expect("populate mixed mapping");
        }
    }
    assert!(runtime.anon.page_at(0).expect("owned first page").is_exclusive());
    let child_mm = Arc::new(ksync::Mutex::new(
        memspace::MmSpace::new_user_empty().expect("child mm"),
    ));
    let child_mm_id = child_mm.lock().mm_id();
    // Both failure orders matter: a file mapping fails before anon commit,
    // or anon materialization fails after a file mapping was installed.
    for blocked_page in [1, 0] {
        let mut child_pt = PageTable::try_new().expect("child page table");
        let collision = alloc_frame(true, PageSize::Size4K).expect("collision frame");
        let blocked_addr = start + blocked_page * PAGE_SIZE_4K;
        {
            let mut new_pgtbl = child_pt.modify();
            new_pgtbl.map(blocked_addr, collision, PageSize::Size4K, flags)
                .expect("force a deterministic child mapping failure");
            let mut old_pgtbl = parent_pt.modify();
            assert!(runtime.clone_for_fork(
                range, flags, &mut old_pgtbl, &mut new_pgtbl,
                memspace::ForkCloneTarget {
                    new_mm_id: child_mm_id,
                    new_aspace: &child_mm,
                    invalidate: None,
                },
            ).is_err());
            assert!(runtime.anon.page_at(0).expect("parent page remains").is_exclusive(),
                "a failed fork must not leave a committed child frame reference");
            let other_addr = start + (1 - blocked_page) * PAGE_SIZE_4K;
            assert!(new_pgtbl.query(other_addr).is_err(), "rollback must remove child PTEs");
            new_pgtbl.unmap(blocked_addr).expect("remove collision");
            new_pgtbl.finish();
        }
        dealloc_frame(collision, PageSize::Size4K);
    }
    runtime.unmap_impl(range, &mut parent_pt.modify()).expect("cleanup parent mapping");
}

fn mapping_after_failed_relocation(
    name: &str,
) -> (
    alloc::sync::Arc<FilePrivateRuntime>,
    alloc::sync::Arc<ksync::Mutex<memspace::MmSpace>>,
) {
    use alloc::sync::Arc;

    let (runtime, mm, file) = mapped_readonly_cache_pages(name, 3);
    let target = runtime.start + 0x80_000;
    let collision = alloc_frame(true, PageSize::Size4K).expect("allocate destination collision");
    {
        let mut guard = mm.lock();
        let anon_addr = runtime.start + 2 * PAGE_SIZE_4K;
        assert!(
            guard
                .handle_page_fault(anon_addr, PageFaultFlags::WRITE)
                .is_resolved()
        );
        let anon_page = runtime
            .anon
            .page_at((2 * PAGE_SIZE_4K) as u64)
            .expect("existing anon page");
        assert!(anon_page.is_exclusive());
        let source = guard
            .resolve_mremap_source(runtime.start, runtime.len)
            .expect("source VMA");
        let flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
        let source_entries = (0..3)
            .map(|page| {
                guard
                    .page_table()
                    .query(runtime.start + page * PAGE_SIZE_4K)
                    .unwrap()
            })
            .collect::<alloc::vec::Vec<_>>();
        let file_refs = Arc::strong_count(&file);
        let anon_refs = Arc::strong_count(&runtime.anon);
        // There is no target VMA. An existing second leaf deterministically
        // fails target preparation after the first leaf has been installed.
        guard
            .page_table_mut()
            .modify()
            .map(target + PAGE_SIZE_4K, collision, PageSize::Size4K, flags)
            .expect("install test-only destination collision");
        assert!(
            guard
                .relocate_mapping(&source, target, runtime.len, flags, &mm)
                .is_err()
        );
        assert!(
            guard.find_vma(target).is_none(),
            "failed destination VMA must be withdrawn"
        );
        assert!(
            guard.page_table().query(target).is_err(),
            "new destination leaf must be removed"
        );
        assert_eq!(
            guard.page_table().query(target + PAGE_SIZE_4K).unwrap().0,
            collision,
            "rollback must preserve a leaf it did not install"
        );
        for (page, expected) in source_entries.into_iter().enumerate() {
            assert_eq!(
                guard
                    .page_table()
                    .query(runtime.start + page * PAGE_SIZE_4K)
                    .unwrap(),
                expected,
                "failed relocation must preserve every source PTE"
            );
        }
        assert_eq!(runtime.file_pages.lock().len(), 2);
        assert!(runtime.file_pages.lock().contains_key(&0));
        assert!(
            runtime
                .file_pages
                .lock()
                .contains_key(&(PAGE_SIZE_4K as u64))
        );
        assert!(
            anon_page.is_exclusive(),
            "failed move must not change anonymous logical references"
        );
        assert_eq!(
            Arc::strong_count(&file),
            file_refs,
            "destination runtime must release its file"
        );
        assert_eq!(
            Arc::strong_count(&runtime.anon),
            anon_refs,
            "destination must release shared backing"
        );
        let mut pgtbl = guard.page_table_mut().modify();
        pgtbl
            .unmap(target + PAGE_SIZE_4K)
            .expect("remove destination collision");
        pgtbl.finish();
    }
    dealloc_frame(collision, PageSize::Size4K);
    (runtime, mm)
}

#[def_test]
fn failed_relocation_keeps_source_readable_without_refault() {
    let (runtime, mm) = mapping_after_failed_relocation("failed-file-move-read");
    let mut byte = [0];
    mm.lock()
        .read(runtime.start, &mut byte)
        .expect("read original source PTE");
    assert_eq!(byte, [0x65]);
    assert!(runtime.anon.page_at(0).is_none());
    assert!(runtime.file_pages.lock().contains_key(&0));
    mm.lock()
        .unmap(runtime.start, runtime.len)
        .expect("cleanup readable source");
}

#[def_test]
fn failed_relocation_then_write_and_mprotect_keeps_single_page_owner() {
    let (runtime, mm) = mapping_after_failed_relocation("failed-file-move-write");
    {
        let mut guard = mm.lock();
        assert!(
            guard
                .handle_page_fault(runtime.start, PageFaultFlags::WRITE)
                .is_resolved()
        );
        assert!(!runtime.file_pages.lock().contains_key(&0));
        let page = runtime.anon.page_at(0).expect("COW owns first page");
        assert!(page.is_exclusive());
        guard
            .write(runtime.start, &[0x92])
            .expect("write private value");
        guard
            .protect(
                runtime.start,
                PAGE_SIZE_4K,
                MappingFlags::USER | MappingFlags::READ,
            )
            .expect("write-protect private page");
        guard
            .protect(
                runtime.start,
                PAGE_SIZE_4K,
                MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
            )
            .expect("restore writable VMA");
        assert!(
            guard
                .handle_page_fault(runtime.start, PageFaultFlags::WRITE)
                .is_resolved(),
            "write after mprotect must use the anonymous owner"
        );
        assert_eq!(
            runtime.anon.page_at(0).unwrap().phys_addr(),
            page.phys_addr()
        );
        assert!(!runtime.file_pages.lock().contains_key(&0));
    }
    let child = mm
        .lock()
        .try_clone()
        .expect("fork mixed file/anonymous source after write");
    let mut byte = [0];
    child
        .lock()
        .read(runtime.start, &mut byte)
        .expect("read child's private value");
    assert_eq!(byte, [0x92]);
    child
        .lock()
        .unmap(runtime.start, runtime.len)
        .expect("cleanup written child");
    mm.lock()
        .unmap(runtime.start, runtime.len)
        .expect("cleanup written source");
}

#[def_test]
fn failed_relocation_can_fork_source_before_any_refault() {
    let (runtime, mm) = mapping_after_failed_relocation("failed-file-move-fork");
    let child = mm
        .lock()
        .try_clone()
        .expect("fork original source immediately after failure");
    let mut byte = [0];
    for page in 0..3 {
        child
            .lock()
            .read(runtime.start + page * PAGE_SIZE_4K, &mut byte)
            .expect("child must retain both file and anonymous pages");
        assert_eq!(byte, [0x65]);
    }
    child
        .lock()
        .unmap(runtime.start, runtime.len)
        .expect("cleanup forked child");
    assert!(
        runtime
            .anon
            .page_at((2 * PAGE_SIZE_4K) as u64)
            .unwrap()
            .is_exclusive()
    );
    mm.lock()
        .unmap(runtime.start, runtime.len)
        .expect("cleanup fork source");
}

#[def_test]
fn file_cow_uses_retained_allocation_without_reloading_evicted_cache_entry() {
    let (runtime, mm, file) = mapped_readonly_cache_pages("retained-file-cow", 1);
    file.mapping().evict().expect("detach cache index while mapping retains its page");
    assert_eq!(file.mapping().nrpages(), 0);
    assert!(mm.lock().handle_page_fault(runtime.start, PageFaultFlags::WRITE).is_resolved());
    assert_eq!(file.mapping().nrpages(), 0, "COW must not reload the file index");
    let page = runtime.anon.page_at(0).expect("owned COW allocation");
    assert_frame_prefix(page.phys_addr(), 0x65, PAGE_SIZE_4K);
    mm.lock().unmap(runtime.start, runtime.len).expect("cleanup COW mapping");
}

#[def_test]
fn partial_unmap_keeps_other_file_pages_and_fork_pins_alive() {
    let (runtime, mm, file) = mapped_readonly_cache_pages("partial-unmap-fork-file-pins", 2);
    let child = mm.lock().try_clone().expect("fork private file mapping");
    mm.lock()
        .unmap(runtime.start, PAGE_SIZE_4K)
        .expect("unmap parent first page");
    assert!(!runtime.file_pages.lock().contains_key(&0));
    assert!(runtime.file_pages.lock().contains_key(&(PAGE_SIZE_4K as u64)));
    let mut child_byte = [0];
    child.lock().read(runtime.start, &mut child_byte).expect("child retains first file page");
    assert_eq!(child_byte, [0x65]);
    child
        .lock()
        .unmap(runtime.start, runtime.len)
        .expect("unmap child pages");
    assert!(runtime.file_pages.lock().contains_key(&(PAGE_SIZE_4K as u64)));
    file.truncate(0).expect("truncate remaining parent view");
    assert!(runtime.file_pages.lock().is_empty());
    mm.lock()
        .unmap(runtime.start + PAGE_SIZE_4K, PAGE_SIZE_4K)
        .expect("remove remaining VMA");
}

#[def_test]
fn madvise_releases_only_selected_readonly_file_pins() {
    let (runtime, mm, _file) = mapped_readonly_cache_pages("readonly-madvise-pins", 2);
    mm.lock()
        .madvise_dontneed(runtime.start, PAGE_SIZE_4K)
        .expect("discard first page");
    assert!(!runtime.file_pages.lock().contains_key(&0));
    assert!(runtime.file_pages.lock().contains_key(&(PAGE_SIZE_4K as u64)));
    assert!(runtime.anon.page_at(0).is_none());
    assert!(
        mm.lock()
            .handle_page_fault(runtime.start, PageFaultFlags::READ)
            .is_resolved()
    );
    assert!(runtime.file_pages.lock().contains_key(&0));
    assert!(runtime.anon.page_at(0).is_none());
    mm.lock()
        .unmap(runtime.start, runtime.len)
        .expect("cleanup pages");
    assert!(runtime.file_pages.lock().is_empty());
}

#[def_test]
fn sparse_unmap_cleanup_releases_only_tokens_for_missing_ptes() {
    let (runtime, mm, _file) = mapped_readonly_cache_pages("sparse-unmap-file-token-cleanup", 2);
    let range = VirtAddrRange::from_start_size(runtime.start, runtime.len);
    let second = runtime.start + PAGE_SIZE_4K;
    let second_offset = PAGE_SIZE_4K as u64;
    let mut guard = mm.lock();
    assert_eq!(runtime.file_pages.lock().len(), 2);
    let second_pte = guard.page_table().query(second).expect("second file PTE");
    {
        let mut pgtbl = guard.page_table_mut().modify();
        // Leave the first removal pending. Check that helper return leaves no
        // pending invalidation and retains only the still-present leaf's token.
        pgtbl.unmap(runtime.start).expect("clear first file PTE");
        runtime.release_unmapped_file_pages(range, &mut pgtbl);
        assert!(!pgtbl.finish().had_pending(), "cleanup must finish the pending invalidation");
        assert!(matches!(
            pgtbl.query(runtime.start),
            Err(khal::paging::PagingError::NotMapped)
        ));
        assert_eq!(pgtbl.query(second).expect("retained second PTE"), second_pte);
        let pages = runtime.file_pages.lock();
        assert_eq!(pages.len(), 1);
        assert!(!pages.contains_key(&0));
        assert!(pages.contains_key(&second_offset));
    }
    assert!(
        guard
            .handle_page_fault(runtime.start, PageFaultFlags::READ)
            .is_resolved()
    );
    assert_eq!(runtime.file_pages.lock().len(), 2);
    assert!(runtime.file_pages.lock().contains_key(&0));
    assert!(runtime.anon.page_at(0).is_none());
    assert!(
        guard
            .handle_page_fault(second, PageFaultFlags::WRITE)
            .is_resolved()
    );
    assert_eq!(runtime.file_pages.lock().len(), 1);
    assert!(!runtime.file_pages.lock().contains_key(&second_offset));
    let page = runtime
        .anon
        .page_at(second_offset)
        .expect("single private write owner");
    assert!(page.is_exclusive());
    assert_frame_prefix(page.phys_addr(), 0x65, PAGE_SIZE_4K);
    guard.clear();
    assert!(guard.find_vma(runtime.start).is_none());
    assert!(guard.page_table().query(runtime.start).is_err());
    assert!(guard.page_table().query(second).is_err());
    assert!(runtime.file_pages.lock().is_empty());
    assert!(runtime.anon.page_at(second_offset).is_none());
}

#[def_test]
fn invalidate_present_releases_private_file_token_before_read_refault() {
    let (runtime, mm, _file) = mapped_readonly_cache_pages("private-file-invalidate-refault", 1);
    assert!(runtime.file_pages.lock().contains_key(&0));
    mm.lock()
        .invalidate_present(runtime.start, runtime.len)
        .expect("clear private-file PTE through invalidation");
    assert!(runtime.file_pages.lock().is_empty());
    assert!(
        mm.lock()
            .handle_page_fault(runtime.start, PageFaultFlags::READ)
            .is_resolved(),
        "normal PTE invalidation must permit a fresh readonly file fault"
    );
    assert!(runtime.file_pages.lock().contains_key(&0));
    mm.lock()
        .unmap(runtime.start, runtime.len)
        .expect("cleanup invalidated private-file mapping");
}

#[def_test]
fn invalidate_present_then_write_and_clear_keep_one_private_page_owner() {
    let (runtime, mm, _file) = mapped_readonly_cache_pages("private-file-invalidate-write-clear", 1);
    mm.lock()
        .invalidate_present(runtime.start, runtime.len)
        .expect("clear private-file PTE before direct write");
    assert!(runtime.file_pages.lock().is_empty());
    assert!(runtime.anon.page_at(0).is_none());
    assert!(
        mm.lock()
            .handle_page_fault(runtime.start, PageFaultFlags::WRITE)
            .is_resolved(),
        "write after invalidation must publish only an anonymous owner"
    );
    assert!(runtime.file_pages.lock().is_empty());
    assert!(runtime.anon.page_at(0).expect("private write page").is_exclusive());
    mm.lock().clear();
    assert!(mm.lock().find_vma(runtime.start).is_none());
    assert!(mm.lock().page_table().query(runtime.start).is_err());
    assert!(runtime.file_pages.lock().is_empty());
    assert!(runtime.anon.page_at(0).is_none());
}

#[def_test]
fn clear_releases_private_file_tokens_with_vmas_and_ptes() {
    let (runtime, mm, _file) = mapped_readonly_cache_pages("private-file-clear-token", 1);
    assert!(runtime.file_pages.lock().contains_key(&0));
    mm.lock().clear();
    assert!(mm.lock().find_vma(runtime.start).is_none());
    assert!(mm.lock().page_table().query(runtime.start).is_err());
    assert!(runtime.file_pages.lock().is_empty());
    assert!(runtime.anon.page_at(0).is_none());
}

#[def_test]
fn mprotect_none_releases_file_pin_before_read_access_is_restored() {
    let (runtime, mm, _file) = mapped_readonly_cache_pages("mprotect-none-private-pin", 1);
    assert!(runtime.file_pages.lock().contains_key(&0));

    mm.lock()
        .protect(runtime.start, runtime.len, MappingFlags::empty())
        .expect("protect with no access");
    assert!(runtime.file_pages.lock().is_empty());

    mm.lock()
        .protect(
            runtime.start,
            runtime.len,
            MappingFlags::USER | MappingFlags::READ,
        )
        .expect("restore read access");
    assert!(
        mm.lock()
            .handle_page_fault(runtime.start, PageFaultFlags::READ)
            .is_resolved()
    );
    assert!(runtime.file_pages.lock().contains_key(&0));
    assert!(runtime.anon.page_at(0).is_none());

    mm.lock()
        .unmap(runtime.start, runtime.len)
        .expect("cleanup mapping");
}

#[def_test]
fn write_only_protect_uses_actual_pte_flags_for_private_file_ownership() {
    for (name, user_flag) in [
        ("write-only-private-file-owner-kernel", MappingFlags::empty()),
        ("write-only-private-file-owner-user", MappingFlags::USER),
    ] {
        let (runtime, mm, _file) = mapped_readonly_cache_pages(name, 2);
        let mut guard = mm.lock();
        guard.protect(runtime.start, runtime.len, user_flag | MappingFlags::WRITE)
            .expect("protect file mapping for write-only access");
        if user_flag.is_empty() {
            assert!(runtime.file_pages.lock().is_empty(),
                "removing COW WRITE leaves empty PTE flags and must retire file tokens");
            assert!(guard.page_table().query(runtime.start).is_err());
        } else {
            assert_eq!(runtime.file_pages.lock().len(), 2,
                "USER-only PTEs remain present and must keep their file tokens");
            assert!(guard.page_table().query(runtime.start).is_ok());
        }
        guard.protect(runtime.start, PAGE_SIZE_4K, MappingFlags::USER | MappingFlags::READ)
            .expect("restore read access to first page");
        assert!(guard.handle_page_fault(runtime.start, PageFaultFlags::READ).is_resolved());
        assert!(runtime.file_pages.lock().contains_key(&0));
        assert!(runtime.anon.page_at(0).is_none());
        let writable = runtime.start + PAGE_SIZE_4K;
        assert!(guard.handle_page_fault(writable, PageFaultFlags::WRITE).is_resolved());
        assert!(!runtime.file_pages.lock().contains_key(&(PAGE_SIZE_4K as u64)));
        let page = runtime.anon.page_at(PAGE_SIZE_4K as u64).expect("single write owner");
        assert!(page.is_exclusive());
        assert_frame_prefix(page.phys_addr(), 0x65, PAGE_SIZE_4K);
        guard.clear();
        assert!(guard.find_vma(runtime.start).is_none());
        assert!(guard.find_vma(writable).is_none());
        assert!(runtime.file_pages.lock().is_empty());
        assert!(runtime.anon.page_at(PAGE_SIZE_4K as u64).is_none());
    }
}

#[def_test]
fn failed_protect_retires_tokens_for_the_already_cleared_prefix() {
    let (runtime, mm, file) = mapped_readonly_cache_pages("failed-protect-file-prefix", 1);
    let next = runtime.start + PAGE_SIZE_4K;
    let readonly_file = crate::test_support::open_test_file(file.path().clone(), 0);
    let mut guard = mm.lock();
    let next_runtime = crate::shared::new_shared_runtime(crate::shared::FileSharedRuntimeSpec {
        start: next,
        len: PAGE_SIZE_4K,
        file: readonly_file,
        offset: 0,
        mm_id: guard.mm_id(),
        invalidate: guard.invalidate_handle(&mm),
    });
    let flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
    let next_vma = VmArea::new(next, PAGE_SIZE_4K, flags - MappingFlags::WRITE, flags,
        next_runtime.backing_info(), 0, Some(memspace::FileMappingInfo {
            offset: 0,
            inode: file.inode().inode(),
            path: None,
        }));
    guard.map_runtime_vma(next_vma, false, next_runtime).expect("install rejecting second VMA");
    assert_eq!(guard.protect(runtime.start, PAGE_SIZE_4K * 2, MappingFlags::WRITE),
        Err(kerrno::KError::PermissionDenied), "readonly shared file rejects write upgrade");
    assert!(guard.page_table().query(runtime.start).is_err());
    assert!(runtime.file_pages.lock().is_empty(),
        "first VMA's cleared PTE must release its token before the later VMA rejects protection");
    assert!(guard.handle_page_fault(runtime.start, PageFaultFlags::READ).is_resolved());
    assert!(runtime.file_pages.lock().contains_key(&0));
    assert!(guard.handle_page_fault(runtime.start, PageFaultFlags::WRITE).is_resolved());
    assert!(runtime.file_pages.lock().is_empty());
    assert!(runtime.anon.page_at(0).expect("private write after failed protect").is_exclusive());
    guard.clear();
}

#[def_test]
fn relocated_file_mapping_releases_pins_at_destination_without_refault() {
    let (runtime, mm, file) = mapped_readonly_cache_pages("relocated-file-pins", 2);
    let target = runtime.start + 0x20_000;
    let flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
    {
        let mut guard = mm.lock();
        let source = guard
            .resolve_mremap_source(runtime.start, runtime.len)
            .expect("source mapping");
        guard
            .relocate_mapping(&source, target, runtime.len, flags, &mm)
            .expect("relocate file mapping");
        let mut bytes = [0];
        guard.read(target, &mut bytes).expect("read moved mapping");
        assert_eq!(bytes, [0x65]);
    }
    assert!(
        runtime.file_pages.lock().is_empty(),
        "retired source runtime should release its pin references"
    );
    file.truncate(0).expect("truncate moved mapping");
    assert_eq!(
        mm.lock()
            .handle_page_fault(target, PageFaultFlags::READ),
        memspace::PageFaultOutcome::BusError,
        "truncate must invalidate and reject refault through the destination runtime"
    );
    mm.lock()
        .unmap(target, runtime.len)
        .expect("remove moved VMA");
}

#[def_test]
fn relocated_file_mapping_can_shrink_and_retains_only_destination_pins() {
    let (runtime, mm, _file) = mapped_readonly_cache_pages("relocated-file-pins-shrink", 2);
    let target = runtime.start + 0x40_000;
    let flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
    let source_range = VirtAddrRange::from_start_size(runtime.start, runtime.len);
    let target_range = VirtAddrRange::from_start_size(target, PAGE_SIZE_4K);
    let relocated = runtime
        .rebuilt_runtime(
            source_range,
            target_range,
            None,
            None,
            None,
            false,
        )
        .expect("rebuild shrunken destination runtime");
    assert_eq!(relocated.file_pages.lock().len(), 1);
    assert!(relocated.file_pages.lock().contains_key(&0));

    {
        let mut guard = mm.lock();
        let source = guard
            .resolve_mremap_source(runtime.start, runtime.len)
            .expect("source mapping");
        guard
            .unmap(runtime.start + PAGE_SIZE_4K, PAGE_SIZE_4K)
            .expect("trim source tail before FIXED relocation");
        guard
            .relocate_mapping(&source, target, PAGE_SIZE_4K, flags, &mm)
            .expect("relocate shrunken mapping");
        let mut byte = [0];
        guard.read(target, &mut byte).expect("read moved mapping");
        assert_eq!(byte, [0x65]);
    }
    assert!(runtime.file_pages.lock().is_empty());

    mm.lock()
        .unmap(target, PAGE_SIZE_4K)
        .expect("remove destination VMA");
}

#[def_test]
fn split_private_file_mapping_mremap_preserves_page_offsets_and_pins() {
    let (runtime, mm, _file) = mapped_readonly_cache_pages("split-relocated-file-pins", 2);
    let source_start = runtime.start + PAGE_SIZE_4K;
    let target = runtime.start + 0x30_000;
    let flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;

    mm.lock()
        .protect(runtime.start, PAGE_SIZE_4K, MappingFlags::USER | MappingFlags::READ)
        .expect("split mapping by protecting the first page");
    assert!(runtime.file_pages.lock().contains_key(&0));
    assert!(
        runtime
            .file_pages
            .lock()
            .contains_key(&(PAGE_SIZE_4K as u64))
    );

    {
        let mut guard = mm.lock();
        let source = guard
            .resolve_mremap_source(source_start, PAGE_SIZE_4K)
            .expect("resolve second split VMA");
        guard
            .relocate_mapping(&source, target, PAGE_SIZE_4K, flags, &mm)
            .expect("relocate second split page");
        assert!(
            guard
                .handle_page_fault(target, PageFaultFlags::WRITE)
                .is_resolved(),
            "write fault on moved second page must complete COW"
        );
    }

    assert!(runtime.file_pages.lock().contains_key(&0));
    assert!(
        !runtime
            .file_pages
            .lock()
            .contains_key(&(PAGE_SIZE_4K as u64)),
        "retired source runtime should release only the moved page pin"
    );
    assert!(
        runtime
            .anon
            .page_at(PAGE_SIZE_4K as u64)
            .is_some(),
        "COW must use the second page's anonymous object offset"
    );

    mm.lock()
        .unmap(target, PAGE_SIZE_4K)
        .expect("unmap relocated page");
    assert!(runtime.file_pages.lock().contains_key(&0));
    mm.lock()
        .unmap(runtime.start, PAGE_SIZE_4K)
        .expect("unmap remaining first page");
    assert!(runtime.file_pages.lock().is_empty());
}

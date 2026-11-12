// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec, vec::Vec};

use khal::paging::{MappingFlags, PageSize};
use ksync::Mutex;
use memaddr::{PAGE_SIZE_4K, VirtAddr};
use memspace::{MmSpace, VmBackingKind};
use unittest::def_test;
use vmobj::{
    FileObjectId, MappingView, MappingViewId, MappingViewKind, MappingViewNotifier,
    MappingViewRange, ObjectInvalidateWork, ObjectViewHit, VmObjectId,
};

use super::FileSharedRuntimeInner;
use crate::{
    new_file_private_vma, runtime::SharedFileSourceAdapter, test_support::page_cache_file,
};

#[def_test]
fn cached_file_read_observes_inode_mapping_write() {
    let file = page_cache_file("cached-read-mapping-write");
    let mut pos = 0;
    file.write_from(&b"old"[..], &mut pos).expect("seed file");
    file.mapping()
        .with_folio_or_create(0, |folio| {
            folio.data()[..3].copy_from_slice(b"new");
            folio.mark_dirty();
            Ok(())
        })
        .expect("page-cache write");

    let mut out = [0u8; 3];
    let mut pos = 0;
    let read = file
        .read_from(&mut out, &mut pos)
        .expect("cached read from mapping");

    assert_eq!(read, 3);
    assert_eq!(&out, b"new");
}

#[def_test]
fn cached_file_write_updates_inode_mapping() {
    let file = page_cache_file("cached-write-mapping");
    let mut pos = 0;
    file.write_from(&b"mapping-owned"[..], &mut pos)
        .expect("cached write");

    let mut out = [0u8; 13];
    let out_len = out.len();
    let read = file
        .mapping()
        .with_folio_or_create(0, |folio| {
            out.copy_from_slice(&folio.data()[..out_len]);
            Ok(out_len)
        })
        .expect("mapping read");

    assert_eq!(read, out.len());
    assert_eq!(&out, b"mapping-owned");
}

#[def_test]
fn shared_and_private_runtime_share_same_file_source_object() {
    let file = page_cache_file("shared-private-same-source");
    let payload = vec![0x5au8; PAGE_SIZE_4K];
    let mut pos = 0;
    let written = file
        .write_from(&payload[..], &mut pos)
        .expect("seed cached file with one page");
    assert_eq!(written, PAGE_SIZE_4K);
    let shared = FileSharedRuntimeInner {
        source: SharedFileSourceAdapter::new_without_view(
            file.clone(),
            1,
            VirtAddr::from_usize(0x10000),
            PAGE_SIZE_4K,
            0,
        ),
        shared_ranges: Mutex::new(Vec::new()),
        writable_shared_ranges: Mutex::new(Vec::new()),
    };
    let (_vma, private) = new_file_private_vma(
        VirtAddr::from_usize(0x20000),
        PAGE_SIZE_4K,
        PageSize::Size4K,
        file,
        0,
        Some(PAGE_SIZE_4K as u64),
        MappingFlags::READ | MappingFlags::WRITE,
        None,
    )
    .expect("private file vma");

    let shared_object = shared.object_id();
    let VmBackingKind::FilePrivate {
        file_object,
        anon_object,
        ..
    } = private.backing_info().kind()
    else {
        panic!("expected file-private backing kind");
    };

    assert_eq!(shared_object, file_object);
    assert!(matches!(shared_object, VmObjectId::File(_)));
    assert!(matches!(anon_object, VmObjectId::Anon(_)));
    assert_ne!(shared_object, anon_object);
}

#[def_test]
fn shared_file_source_adapter_and_private_runtime_agree_on_file_object() {
    let file = page_cache_file("shared-source-adapter-same-file-object");
    let adapter = SharedFileSourceAdapter::new_without_view(
        file.clone(),
        7,
        VirtAddr::from_usize(0x30000),
        PAGE_SIZE_4K,
        0,
    );
    let (_vma, private) = new_file_private_vma(
        VirtAddr::from_usize(0x40000),
        PAGE_SIZE_4K,
        PageSize::Size4K,
        file,
        0,
        Some(PAGE_SIZE_4K as u64),
        MappingFlags::READ | MappingFlags::WRITE,
        None,
    )
    .expect("private file vma");

    let VmBackingKind::FilePrivate { file_object, .. } = private.backing_info().kind() else {
        panic!("expected file-private backing kind");
    };

    assert_eq!(adapter.object(), file_object);
}

struct SyncTestOperations {
    calls: Mutex<Vec<(core::ops::Range<u64>, bool)>>,
    failure: Mutex<Option<kvfs::VfsError>>,
}

impl kvfs::InodeOperations for SyncTestOperations {}

struct SyncTestSuperOperations;

impl kvfs::SuperBlockOperations for SyncTestSuperOperations {
    fn statfs(
        &self,
        _super_block: &kvfs::SuperBlock,
        _dentry: &kvfs::Dentry,
    ) -> kvfs::VfsResult<kvfs::StatFs> {
        Ok(kvfs::libfs::simple_statfs(0))
    }
}

impl kvfs::FileOperations for SyncTestOperations {
    fn fsync(
        &self,
        _file: &kvfs::VfsFile,
        range: core::ops::Range<u64>,
        data_only: bool,
    ) -> kvfs::VfsResult<()> {
        self.calls.lock().push((range, data_only));
        self.failure.lock().map_or(Ok(()), Err)
    }
}

#[def_test]
fn msync_sync_dispatches_file_range_datasync_and_propagates_persistence_failure() {
    use alloc::sync::Arc;

    use kvfs::{
        Dentry, Mount, NodeFlags, NodePermission, NodeType, Path, Umode, VfsInode, VfsInodeInit,
    };
    use memspace::VmRuntimeOps;

    let operations = Arc::new(SyncTestOperations {
        calls: Mutex::new(Vec::new()),
        failure: Mutex::new(None),
    });
    let inode = VfsInode::new_file_with_operations(
        operations.clone(),
        operations.clone(),
        operations.clone(),
        NodeFlags::PRIVATE,
        VfsInodeInit::new(
            2,
            0,
            Umode::new(NodeType::RegularFile, NodePermission::default()),
        ),
    );
    static FS_TYPE: kvfs::FileSystemType = kvfs::FileSystemType::internal("filemap-sync-test");
    let super_block = kvfs::SuperBlock::new(&FS_TYPE, &SyncTestSuperOperations, |super_block| {
        let inode = super_block.get_or_init_inode(2, || inode);
        Dentry::new_file_from_inode(inode, None, "msync-range".into())
    });
    let mount = Mount::new_root(&super_block);
    let file = kvfs::dentry_open(
        Path::new(mount, super_block.root_dir()),
        0,
        kcred::initial_cred(),
    )
    .unwrap();
    let start = VirtAddr::from_usize(0x50_000);
    let runtime = super::FileSharedRuntime(Arc::new(FileSharedRuntimeInner {
        source: SharedFileSourceAdapter::new_without_view(
            file.clone(),
            1,
            start,
            3 * PAGE_SIZE_4K,
            2,
        ),
        shared_ranges: Mutex::new(Vec::new()),
        writable_shared_ranges: Mutex::new(Vec::new()),
    }));
    let range = (3 * PAGE_SIZE_4K) as u64..(4 * PAGE_SIZE_4K) as u64;
    assert_eq!(runtime.msync(range.clone()), Ok(()));
    assert_eq!(
        *operations.calls.lock(),
        vec![((3 * PAGE_SIZE_4K) as u64..(4 * PAGE_SIZE_4K) as u64, true)]
    );
    *operations.failure.lock() = Some(kvfs::VfsError::Io);
    assert_eq!(runtime.msync(range), Err(kerrno::KError::Io));
}

fn shared_relocation_releases_seal_registrations(fail_preparation: bool) {
    use alloc::sync::Arc;

    use khal::trap::PageFaultFlags;
    use linux_raw_sys::general::F_SEAL_WRITE;
    use memfs::shmem;
    use memspace::{
        MmSpace, VmArea,
        backend::{alloc_frame, dealloc_frame},
    };

    let location = shmem::create_memfd_file("shared-relocation-seals", true, kcred::initial_cred())
        .expect("create sealable relocation file")
        .into_path();
    let file = crate::test_support::open_test_file(location, 2);
    let len = PAGE_SIZE_4K * 2;
    file.write_from(&vec![0x63; len], &mut 0)
        .expect("seed shared relocation file");
    let mm = Arc::new(Mutex::new(
        MmSpace::new_user_empty().expect("new shared relocation mm"),
    ));
    let flags = MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER;
    let start = VirtAddr::from_usize(0x700_000);
    let target = start + 0x80_000;
    let mm_id = mm.lock().mm_id();
    let invalidate = mm.lock().invalidate_handle(&mm);
    let runtime = super::new_shared_runtime(super::FileSharedRuntimeSpec {
        start,
        len,
        file: file.clone(),
        offset: 0,
        mm_id,
        invalidate,
    });
    let vma = VmArea::new(
        start,
        len,
        flags,
        flags,
        runtime.backing_info(),
        0,
        Some(memspace::FileMappingInfo {
            offset: 0,
            inode: file.inode().inode(),
            path: None,
        }),
    );
    let mut guard = mm.lock();
    guard
        .map_runtime_vma(vma, false, runtime)
        .expect("install shared source");
    for page in 0..2 {
        assert!(
            guard
                .handle_page_fault(start + page * PAGE_SIZE_4K, PageFaultFlags::READ)
                .is_resolved()
        );
    }
    assert_eq!(
        shmem::add_seals_for_location(file.path(), F_SEAL_WRITE),
        Err(kvfs::VfsError::ResourceBusy),
        "active source must block write sealing"
    );
    let source = guard
        .resolve_mremap_source(start, len)
        .expect("shared source snapshot");
    if fail_preparation {
        let collision = alloc_frame(true, PageSize::Size4K).expect("destination collision frame");
        guard
            .page_table_mut()
            .modify()
            .map_raw(target + PAGE_SIZE_4K, collision, PageSize::Size4K, flags)
            .expect("force second destination PTE preparation failure");
        assert!(
            guard
                .relocate_mapping(&source, target, len, false, &mm)
                .is_err()
        );
        assert!(guard.find_vma(target).is_none());
        assert!(guard.page_table().query(target).is_err());
        assert_eq!(
            guard.page_table().query(target + PAGE_SIZE_4K).unwrap().0,
            collision
        );
        let mut pgtbl = guard.page_table_mut().modify();
        pgtbl
            .unmap(target + PAGE_SIZE_4K)
            .expect("remove test collision");
        pgtbl.finish();
        drop(pgtbl);
        dealloc_frame(collision, PageSize::Size4K);
        guard
            .unmap(start, len)
            .expect("unmap original source after failed move");
    } else {
        let target_runtime = memspace::VmRuntimeRef::new_anon_private(target, PageSize::Size4K);
        guard
            .map(target, PAGE_SIZE_4K, flags, true, target_runtime)
            .unwrap();
        let source_frame = guard.page_table().query(start).unwrap().0;
        shmem::add_seals_for_location(file.path(), linux_raw_sys::general::F_SEAL_FUTURE_WRITE)
            .expect("future-write sealing preserves existing writable mappings");
        {
            let _data = file.inode().lock_data();
            guard
                .extend_area(start, PAGE_SIZE_4K)
                .expect("existing VMA extension does not enter new-map admission");
            assert_eq!(guard.find_vma(start).unwrap().size(), len + PAGE_SIZE_4K);
            let source = guard
                .resolve_mremap_source(start, len + PAGE_SIZE_4K)
                .unwrap();
            guard
                .relocate_mapping(&source, target, PAGE_SIZE_4K, true, &mm)
                .expect("inherit permissions while replacing target and shrinking source");
        }
        assert!(guard.find_vma(start).is_none());
        assert!(guard.page_table().query(start + PAGE_SIZE_4K).is_err());
        assert_eq!(guard.page_table().query(target).unwrap().0, source_frame);
        guard
            .unmap(target, PAGE_SIZE_4K)
            .expect("unmap destination after successful move");
    }
    assert_eq!(
        shmem::add_seals_for_location(file.path(), F_SEAL_WRITE),
        Ok(()),
        "retired source and failed destination must not leak writable registrations"
    );
}

#[def_test]
fn failed_shared_relocation_releases_destination_seal_registrations() {
    shared_relocation_releases_seal_registrations(true);
}

#[def_test]
fn successful_shared_relocation_releases_source_seal_registrations() {
    shared_relocation_releases_seal_registrations(false);
}

#[def_test]
fn live_cache_invalidation_removes_shared_ptes_before_releasing_pages() {
    use alloc::sync::Arc;

    use khal::trap::PageFaultFlags;
    use memspace::{MmSpace, VmArea};

    let file = page_cache_file("live-shared-cache-invalidation");
    file.write_from(&vec![0x5a; PAGE_SIZE_4K], &mut 0).unwrap();
    let mm = Arc::new(Mutex::new(MmSpace::new_user_empty().unwrap()));
    let user = MmSpace::acquire_user(mm.clone()).unwrap();
    let start = VirtAddr::from_usize(0x780_000);
    let flags = MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER;
    let mm_id = mm.lock().mm_id();
    let invalidate = mm.lock().invalidate_handle(&mm);
    let runtime = super::new_shared_runtime(super::FileSharedRuntimeSpec {
        start,
        len: PAGE_SIZE_4K,
        file: file.clone(),
        offset: 0,
        mm_id,
        invalidate,
    });
    let vma = VmArea::new(
        start,
        PAGE_SIZE_4K,
        flags,
        flags,
        runtime.backing_info(),
        0,
        Some(memspace::FileMappingInfo {
            offset: 0,
            inode: file.inode().inode(),
            path: None,
        }),
    );
    {
        let mut guard = mm.lock();
        guard.map_runtime_vma(vma, false, runtime).unwrap();
        assert!(
            guard
                .handle_page_fault(start, PageFaultFlags::READ)
                .is_resolved()
        );
    }
    let outcome = user.resolve_page_fault(start, PageFaultFlags::WRITE);
    assert!(outcome.is_resolved(), "shared write fault: {outcome:?}");
    assert!(
        mm.lock()
            .page_table()
            .query(start)
            .unwrap()
            .1
            .contains(MappingFlags::WRITE)
    );
    file.inode().invalidate_cached_pages().unwrap();
    assert_eq!(file.mapping().nrpages(), 0);
    let mut guard = mm.lock();
    assert!(guard.page_table().query(start).is_err());
    assert!(guard.find_vma(start).is_some());
    assert!(
        guard
            .handle_page_fault(start, PageFaultFlags::READ)
            .is_resolved()
    );
    assert_eq!(file.mapping().nrpages(), 1);
}

struct NotifierFixture {
    mm: Arc<Mutex<MmSpace>>,
    notifier: Arc<crate::invalidate::MmSpaceInvalidate>,
    view: MappingView,
    object: VmObjectId,
}

fn notifier_fixture() -> NotifierFixture {
    let start = VirtAddr::from_usize(0x40_0000);
    let mm = Arc::new(Mutex::new(
        MmSpace::new_empty_user(start, PAGE_SIZE_4K * 4)
            .expect("test address space should be constructible"),
    ));
    let handle = mm.lock().invalidate_handle(&mm);
    let view = MappingView::new(
        MappingViewId::from_raw(1),
        1,
        MappingViewRange {
            vma_start: 0x40_0000,
            vma_len: PAGE_SIZE_4K,
            object_start: 0,
            object_len: PAGE_SIZE_4K,
        },
        MappingViewKind::Shared,
    );
    NotifierFixture {
        mm,
        notifier: crate::invalidate::MmSpaceInvalidate::new(handle),
        view,
        object: VmObjectId::File(FileObjectId::from_raw(11)),
    }
}

#[def_test]
fn try_invalidate_rejects_hit_without_a_whole_page() {
    let fixture = notifier_fixture();
    let hit = ObjectViewHit::new(fixture.view.clone(), 1, PAGE_SIZE_4K - 1);
    let work = ObjectInvalidateWork::new(fixture.object, 1, PAGE_SIZE_4K - 1, vec![hit.clone()]);

    assert!(
        !fixture
            .notifier
            .try_invalidate(&work, &hit)
            .expect("a partial hit is not an error")
    );
}

#[def_test]
fn try_invalidate_reports_pending_while_address_space_is_busy() {
    let fixture = notifier_fixture();
    let hit = ObjectViewHit::new(fixture.view.clone(), 0, PAGE_SIZE_4K);
    let work = ObjectInvalidateWork::new(fixture.object, 0, PAGE_SIZE_4K, vec![hit.clone()]);
    let guard = fixture.mm.lock();

    assert!(
        !fixture
            .notifier
            .try_invalidate(&work, &hit)
            .expect("a busy address space is not an error")
    );

    drop(guard);
}

#[def_test]
fn try_invalidate_reports_complete_for_a_destroyed_address_space() {
    let fixture = notifier_fixture();
    let hit = ObjectViewHit::new(fixture.view.clone(), 0, PAGE_SIZE_4K);
    let work = ObjectInvalidateWork::new(fixture.object, 0, PAGE_SIZE_4K, vec![hit.clone()]);
    drop(fixture.mm);

    assert!(
        fixture
            .notifier
            .try_invalidate(&work, &hit)
            .expect("a destroyed address space is already complete")
    );
}

fn write_only_shared_first_fault(after_protect: bool) {
    use khal::trap::PageFaultFlags;
    use memspace::VmArea;

    let file = page_cache_file(if after_protect {
        "shared-write-only-after-protect"
    } else {
        "shared-write-only-first-fault"
    });
    file.write_from(&vec![0x51; PAGE_SIZE_4K], &mut 0).unwrap();
    let mm = Arc::new(Mutex::new(MmSpace::new_user_empty().unwrap()));
    let user = MmSpace::acquire_user(mm.clone()).unwrap();
    let start = VirtAddr::from_usize(0x7f0_000);
    let maximum = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
    let writable = MappingFlags::USER | MappingFlags::WRITE;
    let initial = if after_protect {
        MappingFlags::USER | MappingFlags::READ
    } else {
        writable
    };
    let mm_id = mm.lock().mm_id();
    let invalidate = mm.lock().invalidate_handle(&mm);
    let runtime = super::new_shared_runtime(super::FileSharedRuntimeSpec {
        start,
        len: PAGE_SIZE_4K,
        file: file.clone(),
        offset: 0,
        mm_id,
        invalidate,
    });
    let vma = VmArea::new(
        start,
        PAGE_SIZE_4K,
        initial,
        maximum,
        runtime.backing_info(),
        0,
        Some(memspace::FileMappingInfo {
            offset: 0,
            inode: file.inode().inode(),
            path: None,
        }),
    );
    {
        let mut guard = mm.lock();
        guard.map_runtime_vma(vma, false, runtime).unwrap();
        if after_protect {
            guard.protect(start, PAGE_SIZE_4K, writable).unwrap();
        }
        assert!(guard.page_table().query(start).is_err());
    }
    {
        // Admission is nonblocking under MM serialization. Force a real
        // conflict so Retry coverage does not depend on parallel-test timing.
        let _data = file.inode().lock_data();
        let mut guard = mm.lock();
        assert_eq!(
            guard.handle_page_fault(start, PageFaultFlags::WRITE),
            memspace::PageFaultOutcome::Retry
        );
        assert!(guard.page_table().query(start).is_err());
    }
    // Shared tmpfs admission may also race unrelated test readers. Use the
    // task-context resolver, which releases MM before waiting and revalidates.
    let outcome = user.resolve_page_fault(start, PageFaultFlags::WRITE);
    assert!(outcome.is_resolved(), "write-only file fault: {outcome:?}");
    let mut guard = mm.lock();
    let (paddr, flags, _) = guard.page_table().query(start).unwrap();
    assert!(flags.contains(MappingFlags::WRITE));
    file.mapping()
        .with_folio_or_create(0, |folio| {
            assert!(folio.is_dirty());
            assert_eq!(folio.paddr(), paddr);
            Ok(())
        })
        .unwrap();
    let mut pgtbl = guard.page_table_mut().modify();
    assert_eq!(
        pgtbl.remap_raw(start, paddr + PAGE_SIZE_4K, writable),
        Err(khal::paging::PagingError::InvalidAddress),
        "the writable file leaf must retain its managed hold"
    );
}

#[def_test]
fn shared_write_only_mapping_resolves_first_write_without_read_permission() {
    write_only_shared_first_fault(false);
}

#[def_test]
fn shared_write_only_protection_resolves_a_previously_absent_page() {
    write_only_shared_first_fault(true);
}

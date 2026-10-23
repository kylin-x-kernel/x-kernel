// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{vec, vec::Vec};

use khal::paging::{MappingFlags, PageSize};
use ksync::Mutex;
use memaddr::{PAGE_SIZE_4K, VirtAddr};
use memspace::VmBackingKind;
use unittest::def_test;
use vmobj::VmObjectId;

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
    )
    .expect("private file vma");

    let VmBackingKind::FilePrivate { file_object, .. } = private.backing_info().kind() else {
        panic!("expected file-private backing kind");
    };

    assert_eq!(adapter.object(), file_object);
}

fn shared_relocation_releases_seal_registrations(fail_preparation: bool) {
    use alloc::sync::Arc;

    use khal::trap::PageFaultFlags;
    use linux_raw_sys::general::F_SEAL_WRITE;
    use memfs::shmem;
    use memspace::{MmSpace, VmArea, backend::{alloc_frame, dealloc_frame}};

    let location = shmem::create_memfd_file("shared-relocation-seals", true, kcred::initial_cred())
        .expect("create sealable relocation file")
        .into_path();
    let file = crate::test_support::open_test_file(location, 2);
    let len = PAGE_SIZE_4K * 2;
    file.write_from(&vec![0x63; len], &mut 0).expect("seed shared relocation file");
    let mm = Arc::new(Mutex::new(MmSpace::new_user_empty().expect("new shared relocation mm")));
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
    let vma = VmArea::new(start, len, flags, flags, runtime.backing_info(), 0,
        Some(memspace::FileMappingInfo {
            offset: 0,
            inode: file.inode().inode(),
            path: None,
        }));
    let mut guard = mm.lock();
    guard.map_runtime_vma(vma, false, runtime).expect("install shared source");
    for page in 0..2 {
        assert!(guard.handle_page_fault(start + page * PAGE_SIZE_4K, PageFaultFlags::READ).is_resolved());
    }
    assert_eq!(shmem::add_seals_for_location(file.path(), F_SEAL_WRITE),
        Err(kvfs::VfsError::ResourceBusy), "active source must block write sealing");
    let source = guard.resolve_mremap_source(start, len).expect("shared source snapshot");
    if fail_preparation {
        let collision = alloc_frame(true, PageSize::Size4K).expect("destination collision frame");
        guard.page_table_mut().modify()
            .map(target + PAGE_SIZE_4K, collision, PageSize::Size4K, flags)
            .expect("force second destination PTE preparation failure");
        assert!(guard.relocate_mapping(&source, target, len, flags, &mm).is_err());
        assert!(guard.find_vma(target).is_none());
        assert!(guard.page_table().query(target).is_err());
        assert_eq!(guard.page_table().query(target + PAGE_SIZE_4K).unwrap().0, collision);
        let mut pgtbl = guard.page_table_mut().modify();
        pgtbl.unmap(target + PAGE_SIZE_4K).expect("remove test collision");
        pgtbl.finish();
        drop(pgtbl);
        dealloc_frame(collision, PageSize::Size4K);
        guard.unmap(start, len).expect("unmap original source after failed move");
    } else {
        guard.relocate_mapping(&source, target, len, flags, &mm)
            .expect("relocate shared file successfully");
        assert!(guard.find_vma(start).is_none());
        guard.unmap(target, len).expect("unmap destination after successful move");
    }
    assert_eq!(shmem::add_seals_for_location(file.path(), F_SEAL_WRITE), Ok(()),
        "retired source and failed destination must not leak writable registrations");
}

#[def_test]
fn failed_shared_relocation_releases_destination_seal_registrations() {
    shared_relocation_releases_seal_registrations(true);
}

#[def_test]
fn successful_shared_relocation_releases_source_seal_registrations() {
    shared_relocation_releases_seal_registrations(false);
}

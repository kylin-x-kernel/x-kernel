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

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use khal::paging::MappingFlags;
use memaddr::{PAGE_SIZE_4K, VirtAddr};
use memspace::{MmObserver, VmBackingKind};
use unittest::def_test;

use super::{FileRuntimeContext, FileVmaSpec, build_file_runtime, build_file_vma};
use crate::test_support::{anonymous_location, open_test_file};

const O_RDWR: u32 = 2;

#[def_test]
fn cached_file_sources_share_inode_owned_object_identity() {
    let location = anonymous_location("cached-private-identity");
    let cached = open_test_file(location.clone(), O_RDWR);
    let reopened = open_test_file(location, O_RDWR);

    assert_eq!(cached.mapping().object_id(), reopened.mapping().object_id());
}

#[def_test]
fn private_runtime_from_reopened_file_uses_inode_owned_cached_object_identity() {
    let location = anonymous_location("reopened-private-runtime");
    let file = open_test_file(location.clone(), O_RDWR);
    let aspace = Arc::new(ksync::Mutex::new(
        memspace::MmSpace::new_user_empty().expect("mm"),
    ));
    let observer = MmObserver::new(&aspace);
    let invalidate = observer.invalidate_handle();
    let runtime = build_file_runtime(
        VirtAddr::from_usize(0x20000),
        PAGE_SIZE_4K,
        &file,
        0,
        khal::paging::PageSize::Size4K,
        crate::FileMappingMode::Private,
        FileRuntimeContext {
            mm_id: aspace.lock().mm_id(),
            observer: &observer,
            invalidate,
        },
    )
    .expect("private runtime");
    let vma = build_file_vma(
        FileVmaSpec {
            start: VirtAddr::from_usize(0x20000),
            len: PAGE_SIZE_4K,
            page_size: khal::paging::PageSize::Size4K,
            flags: MappingFlags::READ | MappingFlags::WRITE,
            max_flags: MappingFlags::READ | MappingFlags::WRITE,
            offset: 0,
            inode: location.inode().inode(),
            path: None,
        },
        runtime.backing_info(),
    );

    let expected = open_test_file(location, O_RDWR).mapping().object_id();
    let VmBackingKind::FilePrivate { file_object, .. } = vma.backing().kind() else {
        panic!("expected file-private backing");
    };
    assert_eq!(file_object, expected);
}

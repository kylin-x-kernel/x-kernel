// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

/// Memfds live on per-instance super blocks; cross-superblock copies
/// between them are served by the memfs native chunk primitive, which
/// judges its memfs-storage input itself — a real capability, not an
/// admission marker.
#[def_test]
fn memfd_files_express_file_copy_support() {
    let cred = kcred::initial_cred();
    let first = create_memfd_file("memfd-a", false, cred.clone()).unwrap();
    let second = create_memfd_file("memfd-b", false, cred).unwrap();

    let first_sb = first.location.inode().super_block().unwrap();
    let second_sb = second.location.inode().super_block().unwrap();
    assert!(!Arc::ptr_eq(&first_sb, &second_sb));

    let first_file = first.into_file(kcred::initial_cred()).unwrap();
    let second_file = second.into_file(kcred::initial_cred()).unwrap();
    assert!(matches!(
        second_file.copy_chunk_from(&first_file, 0, 0, 0),
        Some(Ok(0))
    ));
}

#[def_test]
fn shmem_write_seal_rejects_write_policy() {
    let state = ShmemObjectState::new(
        ShmemObjectKind::Memfd,
        String::from("memfd:test"),
        ShmemSealSet::WRITE | ShmemSealSet::FUTURE_WRITE,
    );

    assert_eq!(
        state.check_write_allowed(),
        Err(kvfs::VfsError::OperationNotPermitted)
    );
}

#[def_test]
fn shmem_grow_and_shrink_seals_reject_matching_resize() {
    let state = ShmemObjectState::new(
        ShmemObjectKind::Memfd,
        String::from("memfd:test"),
        ShmemSealSet::GROW | ShmemSealSet::SHRINK,
    );

    assert_eq!(
        state.check_resize_allowed(4096, 8192),
        Err(kvfs::VfsError::OperationNotPermitted)
    );
    assert_eq!(
        state.check_resize_allowed(4096, 0),
        Err(kvfs::VfsError::OperationNotPermitted)
    );
    assert_eq!(state.check_resize_allowed(4096, 4096), Ok(()));
}

#[def_test]
fn shmem_write_and_future_write_seals_reject_shared_writable_mapping() {
    let state = ShmemObjectState::new(
        ShmemObjectKind::Memfd,
        String::from("memfd:test"),
        ShmemSealSet::WRITE | ShmemSealSet::FUTURE_WRITE,
    );

    assert_eq!(
        state.check_shared_writable_mapping_allowed(),
        Err(kvfs::VfsError::OperationNotPermitted)
    );
    assert_eq!(
        state.check_shared_write_fault_allowed(),
        Err(kvfs::VfsError::OperationNotPermitted)
    );
}

#[def_test]
fn shmem_future_write_seal_allows_existing_shared_write_fault() {
    let state = ShmemObjectState::new(
        ShmemObjectKind::Memfd,
        String::from("memfd:test"),
        ShmemSealSet::FUTURE_WRITE,
    );

    assert_eq!(
        state.check_shared_writable_mapping_allowed(),
        Err(kvfs::VfsError::OperationNotPermitted)
    );
    assert_eq!(state.check_shared_write_fault_allowed(), Ok(()));
}

#[def_test]
fn shmem_add_seals_is_monotonic() {
    let state = ShmemObjectState::new(
        ShmemObjectKind::Memfd,
        String::from("memfd:test"),
        ShmemSealSet::empty(),
    );

    state.add_seals(ShmemSealSet::GROW).unwrap();
    state.add_seals(ShmemSealSet::SHRINK).unwrap();

    assert!(state.seals().contains(ShmemSealSet::GROW));
    assert!(state.seals().contains(ShmemSealSet::SHRINK));
}

#[def_test]
fn shmem_seal_seal_blocks_additions() {
    let state = ShmemObjectState::new(
        ShmemObjectKind::Memfd,
        String::from("memfd:test"),
        ShmemSealSet::SEAL,
    );

    assert_eq!(
        state.add_seals(ShmemSealSet::GROW),
        Err(kvfs::VfsError::OperationNotPermitted)
    );
    assert_eq!(state.seals(), ShmemSealSet::SEAL);
}

#[def_test]
fn shmem_write_seal_rejects_active_writable_shared_pages() {
    let state = ShmemObjectState::new(
        ShmemObjectKind::Memfd,
        String::from("memfd:test"),
        ShmemSealSet::empty(),
    );

    state.register_shared_pages(1).unwrap();
    state.register_writable_shared_pages(1).unwrap();
    assert_eq!(state.shared_pages(), 1);
    assert_eq!(state.writable_shared_pages(), 1);
    assert_eq!(
        state.add_seals(ShmemSealSet::WRITE),
        Err(kvfs::VfsError::ResourceBusy)
    );
    state.unregister_writable_shared_pages(1);
    state.unregister_shared_pages(1);
    assert_eq!(state.shared_pages(), 0);
    assert_eq!(state.writable_shared_pages(), 0);
    assert_eq!(state.add_seals(ShmemSealSet::WRITE), Ok(()));
}

#[def_test]
fn shmem_write_seal_allows_active_readonly_shared_pages() {
    let state = ShmemObjectState::new(
        ShmemObjectKind::Memfd,
        String::from("memfd:test"),
        ShmemSealSet::empty(),
    );

    state.register_shared_pages(1).unwrap();
    assert_eq!(state.add_seals(ShmemSealSet::WRITE), Ok(()));
    assert_eq!(state.shared_pages(), 1);

    state.unregister_shared_pages(1);
    assert!(state.seals().contains(ShmemSealSet::WRITE));
}

#[def_test]
fn memfd_write_seal_blocks_write_policy() {
    let shmem = create_memfd_file("memfd:test", true, kcred::initial_cred()).unwrap();
    let location = shmem.location();

    assert_eq!(check_write_allowed(location), Ok(()));
    add_seals_for_location(location, ShmemSealSet::WRITE.bits()).unwrap();

    assert_eq!(
        check_write_allowed(location),
        Err(kvfs::VfsError::OperationNotPermitted)
    );
}

#[def_test]
fn memfd_resize_seals_block_growth_and_shrink_policy() {
    let shmem = create_memfd_file("memfd:test", true, kcred::initial_cred()).unwrap();
    let location = shmem.location();

    assert_eq!(
        add_seals_for_location(location, (ShmemSealSet::GROW | ShmemSealSet::SHRINK).bits()),
        Ok(())
    );
    assert_eq!(
        check_resize_allowed(location, 4096, 8192),
        Err(kvfs::VfsError::OperationNotPermitted)
    );
    assert_eq!(
        check_resize_allowed(location, 4096, 0),
        Err(kvfs::VfsError::OperationNotPermitted)
    );
    assert_eq!(check_resize_allowed(location, 4096, 4096), Ok(()));
}

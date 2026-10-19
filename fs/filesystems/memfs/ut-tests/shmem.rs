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
fn tmpfs_context_defaults_to_half_of_total_pages() {
    let options = parse_tmpfs_context_options(None, 256).unwrap();

    assert_eq!(options.mount_page_limit(), Some(128));
    assert_eq!(options.reconfigure_page_limit(), None);
}

#[def_test]
fn tmpfs_context_parses_size_and_block_limits() {
    let options = parse_tmpfs_context_options(Some(b"size=8192\0"), 256).unwrap();
    assert_eq!(options.mount_page_limit(), Some(2));
    assert_eq!(options.reconfigure_page_limit(), Some(Some(2)));

    let options = parse_tmpfs_context_options(Some(b"size=1m\0"), 256).unwrap();
    assert_eq!(options.mount_page_limit(), Some(256));

    let options = parse_tmpfs_context_options(Some(b"size=50%\0"), 256).unwrap();
    assert_eq!(options.mount_page_limit(), Some(128));

    let options = parse_tmpfs_context_options(Some(b"nr_blocks=3\0"), 256).unwrap();
    assert_eq!(options.mount_page_limit(), Some(3));

    let options = parse_tmpfs_context_options(Some(b"nr_blocks=1k\0"), 256).unwrap();
    assert_eq!(options.mount_page_limit(), Some(1024));
}

#[def_test]
fn tmpfs_percentage_size_rounds_up_partial_pages() {
    let half = parse_tmpfs_context_options(Some(b"size=50%\0"), 257).unwrap();
    assert_eq!(half.mount_page_limit(), Some(129));
    let small = parse_tmpfs_context_options(Some(b"size=1%\0"), 128).unwrap();
    assert_eq!(small.mount_page_limit(), Some(2));
    let default = parse_tmpfs_context_options(None, 257).unwrap();
    assert_eq!(default.mount_page_limit(), Some(128));
}

#[def_test]
fn tmpfs_context_accepts_explicit_unlimited_size() {
    for data in [
        &b"size=0\0"[..],
        &b"size=0k\0"[..],
        &b"size=0%\0"[..],
        &b"nr_blocks=0\0"[..],
    ] {
        let options = parse_tmpfs_context_options(Some(data), 256).unwrap();
        assert_eq!(options.mount_page_limit(), None);
        assert_eq!(options.reconfigure_page_limit(), Some(None));
    }
}

#[def_test]
fn tmpfs_context_parses_linux_compatibility_options() {
    let options = parse_tmpfs_context_options(
        Some(b"size=4096,mode=0700,uid=1000,gid=100,noswap\0"),
        256,
    )
    .unwrap();

    assert_eq!(options.mount_page_limit(), Some(1));
    assert_eq!(options.root_mode.bits(), 0o700);
    assert_eq!(options.root_uid, 1000);
    assert_eq!(options.root_gid, 100);
}

#[def_test]
fn tmpfs_rejects_unimplemented_inode_quotas() {
    for data in [&b"nr_inodes=1\0"[..], &b"nr_inodes=1k\0"[..]] {
        assert_eq!(
            parse_tmpfs_context_options(Some(data), 256).map(|_| ()),
            Err(VfsError::Unsupported)
        );
    }
}

#[def_test]
fn tmpfs_accepts_unlimited_inode_requests() {
    let options = parse_tmpfs_context_options(Some(b"nr_inodes=0\0"), 256).unwrap();
    assert_eq!(options.mount_page_limit(), Some(128));
    assert_eq!(options.reconfigure_page_limit(), None);
}

#[def_test]
fn tmpfs_context_rejects_unknown_or_malformed_options() {
    for data in [
        &b"size=one\0"[..],
        &b"size=0x\0"[..],
        &b"nr_blocks=1x\0"[..],
        &b"nr_blocks=0x\0"[..],
        &b"nr_inodes=1%\0"[..],
        &b"mode=0800\0"[..],
        &b"mode=\0"[..],
        &b"uid=-1\0"[..],
        &b"unknown=1\0"[..],
    ] {
        assert_eq!(
            parse_tmpfs_context_options(Some(data), 256).unwrap_err(),
            VfsError::InvalidInput
        );
    }
}

#[def_test]
fn tmpfs_mount_options_apply_root_metadata() {
    let fs = new_tmpfs_with_type_and_mount_options(
        &TMPFS_TYPE,
        SuperBlockFlags::empty(),
        TmpfsMountOptions {
            page_limit: Some(2),
            root_mode: NodePermission::from_bits_truncate(0o700),
            root_uid: 1000,
            root_gid: 100,
        },
    );
    let metadata = fs.root_dir().metadata();

    assert_eq!(metadata.mode.permission().bits(), 0o700);
    assert_eq!(metadata.uid, 1000);
    assert_eq!(metadata.gid, 100);
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

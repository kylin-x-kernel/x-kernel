// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[unittest::def_test]
fn fs_owner_reservation_failure_skips_resource_initialization() {
    let initialized = core::cell::Cell::new(false);
    let permissions = core::cell::Cell::new(None);
    let result = new_fs_owner_with(
        || {
            initialized.set(true);
            Ok(17usize)
        },
        |flags| {
            permissions.set(Some(flags));
            Err(TEE_ERROR_OUT_OF_MEMORY)
        },
    );
    assert_eq!(result.unwrap_err(), TEE_ERROR_OUT_OF_MEMORY);
    assert_eq!(permissions.get(), Some(AllocFlags::NOWAIT));
    assert!(!initialized.get());
}

#[unittest::def_test]
fn fs_owner_initialization_error_preserves_error_and_cleans_resources() {
    struct Resource<'a>(&'a core::cell::Cell<usize>);
    impl Drop for Resource<'_> {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }
    let drops = core::cell::Cell::new(0);
    let result: TeeResult<KBox<usize>> = new_fs_owner(|| {
        let _resource = Resource(&drops);
        Err(TEE_ERROR_CORRUPT_OBJECT)
    });
    assert_eq!(result.unwrap_err(), TEE_ERROR_CORRUPT_OBJECT);
    assert_eq!(drops.get(), 1);
}

#[unittest::def_test]
fn fs_owner_success_uses_reserved_storage_without_reboxing() {
    let permissions = core::cell::Cell::new(None);
    let storage = KBox::<usize>::new_uninit_with_flags(AllocFlags::NOWAIT).unwrap();
    let address = core::ptr::from_ref(&*storage).cast::<()>();
    let owner = new_fs_owner_with(
        || Ok(29usize),
        |flags| {
            permissions.set(Some(flags));
            Ok(storage)
        },
    )
    .unwrap();
    assert_eq!(*owner, 29);
    assert_eq!(permissions.get(), Some(AllocFlags::NOWAIT));
    assert_eq!(core::ptr::from_ref(&*owner).cast::<()>(), address);
}

use alloc::vec::Vec;

use unittest::{assert, assert_eq};

use super::*;

#[unittest::def_test]
fn htree_reservation_failures_skip_directory_and_backend_initialization() {
    for fail_tree in [true, false] {
        let initialized = core::cell::Cell::new(false);
        let storage_attempted = core::cell::Cell::new(false);
        let tree_flags = core::cell::Cell::new(None);
        let storage_flags = core::cell::Cell::new(None);
        let result: TeeResult<()> = with_htree_owners(
            |_| {
                initialized.set(true);
                Ok(())
            },
            |flags| {
                tree_flags.set(Some(flags));
                if fail_tree {
                    Err(TeeError::from(kerrno::KError::NoMemory).into())
                } else {
                    reserve_fs_owner(flags)
                }
            },
            |flags| {
                storage_flags.set(Some(flags));
                storage_attempted.set(true);
                Err(TeeError::from(kerrno::KError::NoMemory).into())
            },
        );
        assert_eq!(result.unwrap_err(), TEE_ERROR_OUT_OF_MEMORY);
        assert_eq!(storage_attempted.get(), !fail_tree);
        assert_eq!(tree_flags.get(), Some(AllocFlags::NOWAIT));
        assert_eq!(
            storage_flags.get(),
            (!fail_tree).then_some(AllocFlags::NOWAIT)
        );
        assert!(!initialized.get());
    }
}

#[unittest::def_test]
fn htree_owners_initialize_in_reserved_storage() {
    let tree = reserve_fs_owner::<TeeFsHtree>(AllocFlags::NOWAIT).unwrap();
    let storage = reserve_fs_owner::<TeeFsFdAux>(AllocFlags::NOWAIT).unwrap();
    let tree_address = core::ptr::from_ref(&*tree).cast::<()>();
    let storage_address = core::ptr::from_ref(&*storage).cast::<()>();
    let owner = with_htree_owners(
        |owners| {
            let storage = KBox::write(owners.storage, TeeFsFdAux::new());
            Ok(KBox::write(owners.tree, TeeFsHtree::new(storage)))
        },
        |_| Ok(tree),
        |_| Ok(storage),
    )
    .unwrap();
    assert_eq!(core::ptr::from_ref(&*owner).cast::<()>(), tree_address);
    assert_eq!(
        core::ptr::from_ref(&*owner.storage).cast::<()>(),
        storage_address
    );
    tee_fs_htree_close(owner).unwrap();
}

#[unittest::def_test(user)]
fn close_releases_tree_and_backend_without_a_placeholder() {
    use crate::tee::common::file_ops::{FS_MODE_644, FS_OFLAG_DEFAULT};

    let path = "/tmp/test_htree_owner_close.bin";
    let fd = FileVariant::open(path, FS_OFLAG_DEFAULT, FS_MODE_644).unwrap();
    let storage = KBox::new_with_flags(TeeFsFdAux { fd }, AllocFlags::NOWAIT).unwrap();
    let tree = KBox::new_with_flags(TeeFsHtree::new(storage), AllocFlags::NOWAIT).unwrap();
    let mut file = TeeFsFd {
        ht: Some(tree),
        fd,
        ..Default::default()
    };
    ree_fs_close_primitive(&mut file);
    assert!(file.ht.is_none());
    assert!(fd.pread(&mut [0u8; 1], 0).is_err());
    assert_eq!(
        ree_fs_write_primitive(&mut file, 0, &[1], &[], 1).unwrap_err(),
        TEE_ERROR_BAD_STATE
    );
    assert!(TeeFsFd::default().ht.is_none());
    FileVariant::remove_file(path).unwrap();
}

#[unittest::def_test]
fn temporary_blocks_are_zeroed_and_independently_owned() {
    let mut first = get_tmp_block().unwrap();
    let second = get_tmp_block().unwrap();
    assert_eq!(first.len(), BLOCK_SIZE);
    assert_eq!(second.len(), BLOCK_SIZE);
    assert!(first.iter().all(|byte| *byte == 0));
    first.fill(0xa5);
    assert!(second.iter().all(|byte| *byte == 0));
    assert!(first.as_ptr() != second.as_ptr());
    drop(first);
    assert_eq!(second[BLOCK_SIZE - 1], 0);
}

#[unittest::def_test]
fn backing_fd_moves_without_changing_handle_identity() {
    let mut file = TeeFsFd::default();
    assert_eq!(file.fd.fd, -1);
    file.fd = FileVariant { fd: 17 };
    let auxiliary = TeeFsFdAux { fd: file.fd };
    let moved = file;
    assert_eq!(moved.fd.fd, 17);
    assert_eq!(auxiliary.fd.fd, moved.fd.fd);
}

const NODE_SIZE_TEST: usize = size_of::<super::TeeFsHtreeNodeImage>(); // 66
const HTREE_IMAGE_SIZE_TEST: usize = size_of::<super::TeeFsHtreeImage>(); // 256
const BLOCK_NODES_TEST: usize = BLOCK_SIZE / (NODE_SIZE_TEST * 2); // 4096 / (66 * 2) = 31

#[unittest::def_test]
fn test_get_offs_size_head() {
    let result = get_offs_size(TeeFsHtreeType::Head, 0, 0);
    assert_eq!(result.unwrap(), (0_usize, HTREE_IMAGE_SIZE_TEST));

    let result = get_offs_size(TeeFsHtreeType::Head, 0, 1);
    assert_eq!(
        result.unwrap(),
        (HTREE_IMAGE_SIZE_TEST, HTREE_IMAGE_SIZE_TEST)
    );

    let result = get_offs_size(TeeFsHtreeType::Head, 100, 0);
    assert_eq!(result.unwrap(), (0, HTREE_IMAGE_SIZE_TEST));
}

#[unittest::def_test]
fn test_get_offs_size_node() {
    let result = get_offs_size(TeeFsHtreeType::Node, 0, 0);
    assert_eq!(result.unwrap(), (4096, NODE_SIZE_TEST));
    let result = get_offs_size(TeeFsHtreeType::Node, 0, 1);
    assert_eq!(result.unwrap(), (4162, NODE_SIZE_TEST));
    let result = get_offs_size(TeeFsHtreeType::Node, 30, 0);
    assert_eq!(result.unwrap(), (8056, NODE_SIZE_TEST));
    let result = get_offs_size(TeeFsHtreeType::Node, 30, 1);
    assert_eq!(result.unwrap(), (8122, NODE_SIZE_TEST));
    let result = get_offs_size(TeeFsHtreeType::Node, 31, 0);
    assert_eq!(result.unwrap(), (258048, NODE_SIZE_TEST));
    let result = get_offs_size(TeeFsHtreeType::Node, 31, 1);
    assert_eq!(result.unwrap(), (258114, NODE_SIZE_TEST));
}

#[unittest::def_test]
fn test_get_offs_size_block() {
    let _block_nodes_x2_minus_1 = BLOCK_NODES_TEST * 2 - 1;
    let result = get_offs_size(TeeFsHtreeType::Block, 0, 0);
    assert_eq!(result.unwrap(), (8192, BLOCK_SIZE));
    let result = get_offs_size(TeeFsHtreeType::Block, 0, 1);
    assert_eq!(result.unwrap(), (12288, BLOCK_SIZE));
    let result = get_offs_size(TeeFsHtreeType::Block, 30, 0);
    assert_eq!(result.unwrap(), (253952, BLOCK_SIZE));
    let result = get_offs_size(TeeFsHtreeType::Block, 30, 1);
    assert_eq!(result.unwrap(), (262144, BLOCK_SIZE));
    let result = get_offs_size(TeeFsHtreeType::Block, 31, 0);
    assert_eq!(result.unwrap(), (266240, BLOCK_SIZE));
    let result = get_offs_size(TeeFsHtreeType::Block, 31, 1);
    assert_eq!(result.unwrap(), (270336, BLOCK_SIZE));
}

#[unittest::def_test]
fn test_get_offs_size_unsupported_type() {
    let result = get_offs_size(TeeFsHtreeType::UnsupportedType, 0, 0);
    assert_eq!(result.unwrap_err(), TEE_ERROR_GENERIC);
}

#[unittest::def_test(user)]
fn test_ree_fs_primitive_operations() {
    let uuid = TEE_UUID {
        timeLow: 0x12345678,
        timeMid: 0x1234,
        timeHiAndVersion: 0x5678,
        clockSeqAndNode: [0x9a, 0xbc, 0xde, 0xf0, 0x12, 0x34, 0x56, 0x78],
    };

    let dfh = TeeFsDirfileFileh {
        file_number: 0x12345678,
        hash: [0x01; TEE_FS_HTREE_HASH_SIZE],
        idx: 1,
    };

    let mut hash = [0u8; TEE_FS_HTREE_HASH_SIZE];
    let mut fdp = ree_fs_open_primitive(Some(&uuid), true, Some(&mut hash), Some(&dfh)).unwrap();

    assert_eq!(fdp.uuid, uuid);
    assert_eq!(fdp.dfh.file_number, dfh.file_number);

    let test_data = b"Hello, TEE World! This is a test message for ree_fs_primitive operations.";
    let write_result = ree_fs_write_primitive(&mut fdp, 0, test_data, &[], test_data.len());
    assert!(write_result.is_ok());

    let mut read_buffer = vec![0u8; test_data.len()];
    let mut read_len = read_buffer.len();
    let read_result = ree_fs_read_primitive(&mut fdp, 0, &mut read_buffer, &mut [], &mut read_len);
    assert!(read_result.is_ok());
    assert_eq!(read_len, test_data.len());
    assert_eq!(&read_buffer[..read_len], test_data);

    let mut partial_buffer = vec![0u8; 10];
    let mut partial_len = partial_buffer.len();
    let partial_read_result =
        ree_fs_read_primitive(&mut fdp, 0, &mut partial_buffer, &mut [], &mut partial_len);
    assert!(partial_read_result.is_ok());
    assert_eq!(partial_len, 10);
    assert_eq!(&partial_buffer[..partial_len], &test_data[..10]);

    let mut mid_buffer = vec![0u8; 15];
    let mut mid_len = mid_buffer.len();
    let mid_read_result =
        ree_fs_read_primitive(&mut fdp, 10, &mut mid_buffer, &mut [], &mut mid_len);
    assert!(mid_read_result.is_ok());
    assert_eq!(mid_len, 15);
    assert_eq!(&mid_buffer[..mid_len], &test_data[10..25]);

    let additional_data = b" Additional data appended to the file.";
    let additional_write_result = ree_fs_write_primitive(
        &mut fdp,
        test_data.len(),
        additional_data,
        &[],
        additional_data.len(),
    );
    assert!(additional_write_result.is_ok());

    let total_len = test_data.len() + additional_data.len();
    let mut full_buffer = vec![0u8; total_len];
    let mut full_len = full_buffer.len();
    let full_read_result =
        ree_fs_read_primitive(&mut fdp, 0, &mut full_buffer, &mut [], &mut full_len);
    assert!(full_read_result.is_ok());
    assert_eq!(full_len, total_len);

    let mut expected_full_data = Vec::new();
    expected_full_data.extend_from_slice(test_data);
    expected_full_data.extend_from_slice(additional_data);
    assert_eq!(&full_buffer[..full_len], expected_full_data.as_slice());
}

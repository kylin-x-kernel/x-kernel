// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kvfs::SetattrTime;
use unittest::def_test;

use super::*;

struct TestFileOperations;

impl FileOperations for TestFileOperations {}

#[def_test]
fn files_share_inode_and_inherit_superblock_dentry_operations() {
    init_anon_inodefs();
    let first = AnonInodeFs::global()
        .get_file(
            "[first]",
            Arc::new(TestFileOperations),
            Arc::new(()),
            FMode::READ,
            OpenFlags::empty(),
            kcred::initial_cred(),
        )
        .expect("first anonymous file opens");
    let second = AnonInodeFs::global()
        .get_file(
            "[second]",
            Arc::new(TestFileOperations),
            Arc::new(()),
            FMode::READ,
            OpenFlags::empty(),
            kcred::initial_cred(),
        )
        .expect("second anonymous file opens");

    assert!(Arc::ptr_eq(&first.path().inode(), &second.path().inode()));
    assert_eq!(first.path().display_path().unwrap(), "anon_inode:[first]");
    assert_eq!(
        first.path().getattr().unwrap().mode.node_type(),
        NodeType::Unknown
    );
    assert_eq!(
        first.path().filesystem_stat().unwrap().fs_type,
        ANON_INODE_FS_MAGIC
    );
}

#[def_test]
fn metadata_updates_stay_rejected_on_shared_anon_inode() {
    init_anon_inodefs();
    let file = AnonInodeFs::global()
        .get_file(
            "[meta]",
            Arc::new(TestFileOperations),
            Arc::new(()),
            FMode::READ | FMode::WRITE,
            OpenFlags::empty(),
            kcred::initial_cred(),
        )
        .expect("anonymous file opens");
    let cred = kcred::initial_cred();
    let now = SetattrTime::Explicit(ktime::realtime());

    // The singleton is shared across every process's anonymous fds, so
    // authorized metadata updates must keep failing instead of mutating
    // state other processes observe. Truncation keeps the fallback's
    // `EINVAL` for non-regular files.
    assert_eq!(
        file.path().chown(Some(1234), None, &cred),
        Err(VfsError::OperationNotSupported)
    );
    assert_eq!(
        file.path()
            .chmod(NodePermission::from_bits_truncate(0o600), &cred),
        Err(VfsError::OperationNotSupported)
    );
    assert_eq!(
        file.path().set_times(Some(now), Some(now), &cred),
        Err(VfsError::OperationNotSupported)
    );
    assert_eq!(file.truncate(8), Err(VfsError::InvalidInput));

    let metadata = file.path().getattr().unwrap();
    assert_eq!(metadata.uid, 0);
    assert_eq!(metadata.gid, 0);
    assert_eq!(
        metadata.mode.permission().bits(),
        (NodePermission::OWNER_READ | NodePermission::OWNER_WRITE).bits()
    );
}

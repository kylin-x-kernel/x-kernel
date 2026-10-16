// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use linux_raw_sys::general::{O_CREAT, O_EXCL, O_WRONLY};
use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test]
fn memory_filesystems_report_page_sized_blocks() {
    let fs = MemoryFs::new();

    assert_eq!(fs.block_size(), memaddr::PAGE_SIZE_4K as u64);
    assert_eq!(fs.stat().unwrap().block_size, memaddr::PAGE_SIZE_4K as u32);
}

#[def_test]
fn tmpfs_readonly_policy_can_be_reconfigured() {
    let fs = shmem::new_tmpfs(SuperBlockFlags::RDONLY);
    let cred = kcred::initial_cred();

    assert!(fs.flags().contains(SuperBlockFlags::RDONLY));
    let mut writable = FsContext::new_reconfigure(
        fs.as_ref(),
        None,
        None,
        SuperBlockFlags::empty(),
        SuperBlockFlags::RDONLY,
        &cred,
    )
    .unwrap();
    fs.reconfigure(&mut writable).unwrap();
    assert!(!fs.flags().contains(SuperBlockFlags::RDONLY));
    let mut readonly = FsContext::new_reconfigure(
        fs.as_ref(),
        None,
        None,
        SuperBlockFlags::RDONLY,
        SuperBlockFlags::RDONLY,
        &cred,
    )
    .unwrap();
    fs.reconfigure(&mut readonly).unwrap();
    assert!(fs.flags().contains(SuperBlockFlags::RDONLY));
}

#[def_test]
fn tmpfs_write_end_updates_vfs_inode_size() {
    let fs = shmem::new_tmpfs(SuperBlockFlags::empty());
    let root = kvfs::Path::new(kvfs::Mount::new_root(&fs), fs.root_dir());
    let file = kvfs::Filename::new("tmp")
        .open_with_flags_at(
            &root,
            &root,
            O_WRONLY | O_CREAT | O_EXCL,
            NodePermission::from_bits_truncate(0o600),
            NodePermission::empty(),
            kcred::initial_cred(),
        )
        .unwrap();
    let mut pos = 0;

    assert_eq!(file.write_from(b"hello", &mut pos).unwrap(), 5);
    assert_eq!(file.path().getattr().unwrap().size, 5);
    assert_eq!(file.inode().size(), 5);
}

#[def_test]
fn tmpfs_fsync_preserves_file_contents_and_accepts_directories() {
    let fs = shmem::new_tmpfs(SuperBlockFlags::empty());
    let root = Mount::new_root(&fs).root_path();
    let file = kvfs::Filename::new("sync")
        .open_with_flags_at(
            &root,
            &root,
            linux_raw_sys::general::O_RDWR | O_CREAT | O_EXCL,
            NodePermission::from_bits_truncate(0o600),
            NodePermission::empty(),
            kcred::initial_cred(),
        )
        .unwrap();
    assert_eq!(file.write(b"sync-data"), Ok(9));
    for data_only in [false, true] {
        assert_eq!(file.fsync(data_only), Ok(()));
    }
    let mut contents = [0; 9];
    assert_eq!(file.read_from(&mut contents, &mut 0), Ok(9));
    assert_eq!(&contents, b"sync-data");

    let directory = kvfs::dentry_open(
        root,
        linux_raw_sys::general::O_RDONLY | linux_raw_sys::general::O_DIRECTORY,
        kcred::initial_cred(),
    )
    .unwrap();
    for data_only in [false, true] {
        assert_eq!(directory.fsync(data_only), Ok(()));
    }
}

#[def_test]
fn created_inode_uses_fs_ids_and_inherits_setgid_directory() {
    let fs = MemoryFs::new();
    let root = kvfs::Path::new(kvfs::Mount::new_root(&fs), fs.root_dir());
    root.chmod(
        NodePermission::from_bits_truncate(0o777),
        &kcred::initial_cred(),
    )
    .unwrap();

    let mut cred = Cred::root();
    cred.set_resgid(Some(100), Some(200), Some(300)).unwrap();
    assert_eq!(cred.set_fsgid(300), 200);
    cred.set_resuid(Some(1000), Some(2000), Some(3000)).unwrap();
    assert_eq!(cred.set_fsuid(3000), 2000);

    let file = root
        .create("owned", NodePermission::from_bits_truncate(0o600), &cred)
        .unwrap();
    let file_metadata = file.metadata();
    assert_eq!(file_metadata.uid, 3000);
    assert_eq!(file_metadata.gid, 300);

    root.chown(None, Some(4242), &kcred::initial_cred())
        .unwrap();
    root.chmod(
        NodePermission::from_bits_truncate(0o2777),
        &kcred::initial_cred(),
    )
    .unwrap();
    let dir = root
        .mkdir(
            "inherited",
            NodePermission::from_bits_truncate(0o755),
            &cred,
        )
        .unwrap();
    let dir_metadata = dir.metadata();
    assert_eq!(dir_metadata.uid, 3000);
    assert_eq!(dir_metadata.gid, 4242);
    assert!(
        dir_metadata
            .mode
            .permission()
            .contains(NodePermission::SET_GID)
    );
}

#[def_test]
fn rename_rejects_nonempty_directory_after_child_handle_is_dropped() {
    let fs = MemoryFs::new();
    let root = kvfs::Path::new(kvfs::Mount::new_root(&fs), fs.root_dir());
    let permission = NodePermission::from_bits_truncate(0o755);
    let cred = kcred::initial_cred();
    let source = root.mkdir("source", permission, &cred).unwrap();
    let target = root.mkdir("target", permission, &cred).unwrap();
    let child = target.create("child", permission, &cred).unwrap();
    drop(child);
    drop(source);

    assert_eq!(
        root.rename("source", &root, "target", kvfs::RenameFlags::empty(), &cred,),
        Err(VfsError::DirectoryNotEmpty)
    );
}

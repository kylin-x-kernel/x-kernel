// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use linux_raw_sys::general::{O_CREAT, O_EXCL, O_RDWR, O_WRONLY};
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
fn tmpfs_page_quota_returns_enospc_and_truncate_releases() {
    let fs = shmem::new_tmpfs_with_type_and_page_limit(
        &TMPFS_TYPE,
        SuperBlockFlags::empty(),
        1,
    );
    let root = Mount::new_root(&fs).root_path();
    let file = kvfs::Filename::new("quota")
        .open_with_flags_at(
            &root,
            &root,
            linux_raw_sys::general::O_RDWR | O_CREAT | O_EXCL,
            NodePermission::from_bits_truncate(0o600),
            NodePermission::empty(),
            kcred::initial_cred(),
        )
        .unwrap();
    let mut pos = 0;

    assert_eq!(file.write_from(b"first", &mut pos), Ok(5));
    pos = memaddr::PAGE_SIZE_4K as u64;
    assert_eq!(
        file.write_from(b"second", &mut pos),
        Err(VfsError::StorageFull)
    );

    file.truncate(0).unwrap();
    assert_eq!(file.write_from(b"second", &mut pos), Ok(6));
}

#[def_test]
fn tmpfs_unlimited_page_quota_preserves_default_inode_capacity() {
    let fs =
        shmem::new_tmpfs_with_type_and_page_limit(&TMPFS_TYPE, SuperBlockFlags::empty(), 0);
    let mount = Mount::new_root(&fs);
    let root = mount.root_path();
    let file = kvfs::Filename::new("unlimited")
        .open_with_flags_at(
            &root,
            &root,
            O_RDWR | O_CREAT | O_EXCL,
            NodePermission::from_bits_truncate(0o600),
            NodePermission::empty(),
            kcred::initial_cred(),
        )
        .unwrap();

    let stat = mount.filesystem_stat().unwrap();
    assert_eq!(stat.blocks, 0);
    assert_eq!(stat.blocks_free, 0);
    assert_eq!(stat.blocks_available, 0);
    let inode_limit = (global_allocator().total_pages() / 2).min(quota::MAX_INODES) as u64;
    assert_eq!(stat.file_count, inode_limit);
    assert_eq!(stat.free_file_count, inode_limit.saturating_sub(2));
    assert_eq!(file.write(b"page"), Ok(4));
}

#[def_test]
fn tmpfs_buffered_read_hole_does_not_consume_page_quota() {
    let fs = shmem::new_tmpfs_with_type_and_page_limit(
        &TMPFS_TYPE,
        SuperBlockFlags::empty(),
        1,
    );
    let mount = Mount::new_root(&fs);
    let root = mount.root_path();
    let file = kvfs::Filename::new("hole")
        .open_with_flags_at(
            &root,
            &root,
            O_RDWR | O_CREAT | O_EXCL,
            NodePermission::from_bits_truncate(0o600),
            NodePermission::empty(),
            kcred::initial_cred(),
        )
        .unwrap();
    let mut pos = 0;
    assert_eq!(file.write_from(b"first", &mut pos), Ok(5));
    file.truncate((memaddr::PAGE_SIZE_4K * 3) as u64).unwrap();

    let mut hole = alloc::vec![0xff; memaddr::PAGE_SIZE_4K * 2];
    pos = memaddr::PAGE_SIZE_4K as u64;
    assert_eq!(
        file.read_from(&mut hole, &mut pos),
        Ok(memaddr::PAGE_SIZE_4K * 2)
    );
    assert!(hole.iter().all(|byte| *byte == 0));
    let stat = mount.filesystem_stat().unwrap();
    assert_eq!(stat.blocks_free, 0);

    pos = memaddr::PAGE_SIZE_4K as u64;
    assert_eq!(
        file.write_from(b"second", &mut pos),
        Err(VfsError::StorageFull)
    );
}

#[def_test]
fn tmpfs_batched_read_of_holes_preserves_remaining_quota() {
    let fs = shmem::new_tmpfs_with_type_and_page_limit(
        &TMPFS_TYPE,
        SuperBlockFlags::empty(),
        2,
    );
    let mount = Mount::new_root(&fs);
    let root = mount.root_path();
    let file = kvfs::Filename::new("batched-hole")
        .open_with_flags_at(
            &root,
            &root,
            O_RDWR | O_CREAT | O_EXCL,
            NodePermission::from_bits_truncate(0o600),
            NodePermission::empty(),
            kcred::initial_cred(),
        )
        .unwrap();
    let mut pos = 0;
    assert_eq!(file.write_from(b"first", &mut pos), Ok(5));
    file.truncate((memaddr::PAGE_SIZE_4K * 3) as u64).unwrap();
    assert_eq!(mount.filesystem_stat().unwrap().blocks_free, 1);
    let mut holes = alloc::vec![0xff; memaddr::PAGE_SIZE_4K * 2];
    pos = memaddr::PAGE_SIZE_4K as u64;
    assert_eq!(file.read_from(&mut holes, &mut pos), Ok(holes.len()));
    assert!(holes.iter().all(|byte| *byte == 0));
    assert_eq!(mount.filesystem_stat().unwrap().blocks_free, 1);
    pos = memaddr::PAGE_SIZE_4K as u64;
    assert_eq!(file.write_from(b"second", &mut pos), Ok(6));
    assert_eq!(mount.filesystem_stat().unwrap().blocks_free, 0);
}

#[def_test]
fn tmpfs_statfs_reports_page_quota() {
    let fs = shmem::new_tmpfs_with_type_and_page_limit(
        &TMPFS_TYPE,
        SuperBlockFlags::empty(),
        2,
    );
    let mount = Mount::new_root(&fs);
    let root = mount.root_path();
    let file = kvfs::Filename::new("quota")
        .open_with_flags_at(
            &root,
            &root,
            O_WRONLY | O_CREAT | O_EXCL,
            NodePermission::from_bits_truncate(0o600),
            NodePermission::empty(),
            kcred::initial_cred(),
        )
        .unwrap();

    let stat = mount.filesystem_stat().unwrap();
    assert_eq!(stat.blocks, 2);
    assert_eq!(stat.blocks_free, 2);
    assert_eq!(file.write(b"page"), Ok(4));
    let stat = mount.filesystem_stat().unwrap();
    assert_eq!(stat.blocks_free, 1);
    assert_eq!(stat.blocks_available, 1);
}

#[def_test]
fn tmpfs_page_limit_can_be_reconfigured() {
    let fs = shmem::new_tmpfs_with_type_and_page_limit(
        &TMPFS_TYPE,
        SuperBlockFlags::empty(),
        1,
    );
    let root = Mount::new_root(&fs).root_path();
    let file = kvfs::Filename::new("quota")
        .open_with_flags_at(
            &root,
            &root,
            linux_raw_sys::general::O_RDWR | O_CREAT | O_EXCL,
            NodePermission::from_bits_truncate(0o600),
            NodePermission::empty(),
            kcred::initial_cred(),
        )
        .unwrap();
    let mut pos = 0;
    assert_eq!(file.write_from(b"first", &mut pos), Ok(5));

    let cred = kcred::initial_cred();
    let mut context = FsContext::new_reconfigure(
        fs.as_ref(),
        None,
        Some(b"size=8192\0"),
        SuperBlockFlags::empty(),
        SuperBlockFlags::RDONLY,
        &cred,
    )
    .unwrap();
    fs.reconfigure(&mut context).unwrap();

    pos = memaddr::PAGE_SIZE_4K as u64;
    assert_eq!(file.write_from(b"second", &mut pos), Ok(6));
}

#[def_test]
fn tmpfs_page_limit_rejects_remount_below_current_use() {
    let fs = shmem::new_tmpfs_with_type_and_page_limit(
        &TMPFS_TYPE,
        SuperBlockFlags::empty(),
        2,
    );
    let root = Mount::new_root(&fs).root_path();
    let file = kvfs::Filename::new("quota")
        .open_with_flags_at(
            &root,
            &root,
            O_RDWR | O_CREAT | O_EXCL,
            NodePermission::from_bits_truncate(0o600),
            NodePermission::empty(),
            kcred::initial_cred(),
        )
        .unwrap();
    let mut pos = 0;
    assert_eq!(file.write_from(b"first", &mut pos), Ok(5));
    pos = memaddr::PAGE_SIZE_4K as u64;
    assert_eq!(file.write_from(b"second", &mut pos), Ok(6));

    let cred = kcred::initial_cred();
    let mut context = FsContext::new_reconfigure(
        fs.as_ref(),
        None,
        Some(b"size=4096\0"),
        SuperBlockFlags::empty(),
        SuperBlockFlags::RDONLY,
        &cred,
    )
    .unwrap();
    assert_eq!(fs.reconfigure(&mut context), Err(VfsError::InvalidInput));

    let stat = fs.stat().unwrap();
    assert_eq!(stat.blocks, 2);
    assert_eq!(stat.blocks_free, 0);
    pos = (memaddr::PAGE_SIZE_4K * 2) as u64;
    assert_eq!(
        file.write_from(b"third", &mut pos),
        Err(VfsError::StorageFull)
    );
}

#[def_test]
fn tmpfs_page_limit_rejects_retroactive_limit_after_unlimited() {
    let fs =
        shmem::new_tmpfs_with_type_and_page_limit(&TMPFS_TYPE, SuperBlockFlags::empty(), 0);
    let root = Mount::new_root(&fs).root_path();
    let file = kvfs::Filename::new("quota")
        .open_with_flags_at(
            &root,
            &root,
            O_RDWR | O_CREAT | O_EXCL,
            NodePermission::from_bits_truncate(0o600),
            NodePermission::empty(),
            kcred::initial_cred(),
        )
        .unwrap();
    let mut pos = 0;
    assert_eq!(file.write_from(b"first", &mut pos), Ok(5));

    let cred = kcred::initial_cred();
    let mut context = FsContext::new_reconfigure(
        fs.as_ref(),
        None,
        Some(b"size=8192\0"),
        SuperBlockFlags::empty(),
        SuperBlockFlags::RDONLY,
        &cred,
    )
    .unwrap();
    assert_eq!(fs.reconfigure(&mut context), Err(VfsError::InvalidInput));

    pos = memaddr::PAGE_SIZE_4K as u64;
    assert_eq!(file.write_from(b"second", &mut pos), Ok(6));
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

fn create_copy_test_file(root: &kvfs::Path, name: &str) -> Arc<VfsFile> {
    kvfs::Filename::new(name)
        .open_with_flags_at(root, root, O_RDWR | O_CREAT | O_EXCL,
            NodePermission::from_bits_truncate(0o600), NodePermission::empty(), kcred::initial_cred())
        .unwrap()
}

#[def_test]
fn tmpfs_sparse_copy_reuses_destination_pages_without_charging_the_source() {
    for (page_limit, pages) in [(1, 1), (66, 65)] {
        let fs = shmem::new_tmpfs_with_type_and_page_limit(&TMPFS_TYPE, SuperBlockFlags::empty(), page_limit);
        let mount = Mount::new_root(&fs);
        let root = mount.root_path();
        let input = create_copy_test_file(&root, "source");
        let output = create_copy_test_file(&root, "destination");
        let len = pages * memaddr::PAGE_SIZE_4K;
        input.truncate(len as u64).unwrap();
        assert_eq!(output.write_from(&alloc::vec![0x5a; len], &mut 0), Ok(len));
        let free_before = mount.filesystem_stat().unwrap().blocks_free;
        assert_eq!(free_before, (page_limit - pages) as u64);
        let mut input_pos = 0;
        let mut output_pos = 0;
        assert_eq!(
            kvfs::copy_file_range(&input, kvfs::TransferPosition::At(&mut input_pos),
                &output, kvfs::TransferPosition::At(&mut output_pos), len, false),
            Ok(len)
        );
        assert_eq!((input_pos, output_pos), (len as u64, len as u64));
        assert_eq!(input.mapping().nrpages(), 0);
        assert_eq!(output.mapping().nrpages(), pages as u64);
        assert_eq!(mount.filesystem_stat().unwrap().blocks_free, free_before);
        let mut bytes = alloc::vec![0xff; len];
        assert_eq!(output.read_from(&mut bytes, &mut 0), Ok(len));
        assert!(bytes.iter().all(|byte| *byte == 0));
        assert_eq!(input.mapping().nrpages(), 0);
        assert_eq!(mount.filesystem_stat().unwrap().blocks_free, free_before);
    }
}

#[def_test]
fn tmpfs_batched_mixed_read_preserves_eof_and_capacity() {
    let page = memaddr::PAGE_SIZE_4K;
    let pages = 2 * pagecache::FolioBatch::CAPACITY + 3;
    let fs = shmem::new_tmpfs_with_type_and_page_limit(&TMPFS_TYPE, SuperBlockFlags::empty(), pages);
    let mount = Mount::new_root(&fs);
    let file = create_copy_test_file(&mount.root_path(), "mixed");
    for index in (0..pages).step_by(2) {
        let mut pos = (index * page) as u64;
        assert_eq!(file.write_from(&[(index + 1) as u8], &mut pos), Ok(1));
    }
    let len = pages * page - 17;
    file.truncate(len as u64).unwrap();
    let resident_before = file.mapping().nrpages();
    let free_before = mount.filesystem_stat().unwrap().blocks_free;
    let mut bytes = alloc::vec![0xff; len];
    let mut pos = 23;
    let copied = len - pos as usize;
    assert_eq!(file.read_from(&mut bytes, &mut pos), Ok(copied));
    assert_eq!(pos, len as u64);
    for (offset, byte) in bytes[..copied].iter().enumerate() {
        let absolute = offset + 23;
        let index = absolute / page;
        let expected = if absolute.is_multiple_of(page) && index.is_multiple_of(2) { (index + 1) as u8 } else { 0 };
        assert_eq!(*byte, expected);
    }
    assert!(bytes[copied..].iter().all(|byte| *byte == 0xff));
    assert_eq!(file.mapping().nrpages(), resident_before);
    assert_eq!(mount.filesystem_stat().unwrap().blocks_free, free_before);
}

/// Linux's mqueue root directory is `S_IFDIR | S_ISVTX | S_IRWXUGO` (01777),
/// not the 0755 a fresh ramfs root gets (`ipc/mqueue.c`). Container tooling
/// mounts `/dev/mqueue` and compares that mode, and the sticky bit is what
/// lets any task create its own queues in a shared instance.
#[def_test]
fn mqueue_root_carries_linuxs_sticky_world_writable_mode() {
    let fs = new_mqueue_with_type(&MQUEUE_TYPE, SuperBlockFlags::empty());
    let permission = Mount::new_root(&fs).root_path().metadata().mode.permission();
    assert_eq!(permission.bits(), 0o1777);
    // The sticky bit is what lets any task create its own queues here.
    assert_eq!(permission.bits() & NodePermission::STICKY.bits(), 0o1000);
}

/// One mqueue tree per IPC namespace: mounts inside a namespace reuse the same
/// superblock (Linux keys it on the namespace), a read-only mount does not
/// freeze that tree's write policy, and a dead cache entry is rebuilt instead
/// of handed out.
#[def_test(user, serial)]
fn mqueue_tree_is_shared_and_rebuilt_per_ipc_namespace() {
    // The provider strips a `-o ro` request from the shared tree's own
    // flags. Exercised on a fresh unregistered tree so the assertion can
    // actually fail if the strip is removed — a cached tree would make it
    // tautological.
    let direct = new_mqueue_with_type(&MQUEUE_TYPE, SuperBlockFlags::RDONLY);
    assert!(
        !direct.flags().contains(SuperBlockFlags::RDONLY),
        "the provider strips RDONLY from the shared tree's own flags"
    );

    let shared = mqueue_tree_for_ipc_namespace(&MQUEUE_TYPE, SuperBlockFlags::empty());
    let again = mqueue_tree_for_ipc_namespace(&MQUEUE_TYPE, SuperBlockFlags::empty());
    assert!(Arc::ptr_eq(&shared, &again), "one tree per IPC namespace");
    assert_eq!(shared.stat().unwrap().fs_type, MQUEUE_MAGIC);
    assert!(!shared.flags().contains(SuperBlockFlags::RDONLY));

    // A read-only mount reuses the tree: the namespace's tree keeps its own
    // write policy regardless of the requesting mount's flags.
    let readonly = mqueue_tree_for_ipc_namespace(&MQUEUE_TYPE, SuperBlockFlags::RDONLY);
    assert!(Arc::ptr_eq(&shared, &readonly));

    // The tree is pinned for the IPC namespace's own lifetime (Linux
    // `ns->mq_mnt`): dropping every mount reference leaves it alive and
    // usable, and a later mount reuses the same tree instead of starting a
    // fresh one.
    let weak_shared = Arc::downgrade(&shared);
    drop(shared);
    drop(again);
    drop(readonly);
    let surviving = weak_shared
        .upgrade()
        .expect("the shared tree outlives its mounts");
    assert!(surviving.is_available(), "the pinned tree stays usable");
    let remounted = mqueue_tree_for_ipc_namespace(&MQUEUE_TYPE, SuperBlockFlags::empty());
    assert!(
        Arc::ptr_eq(&surviving, &remounted),
        "re-mounts reuse the pinned tree"
    );
}

/// The mqueue root is sealed: until queues exist the provider presents an
/// empty read-only directory, so child creation through the generic memfs
/// mutators is refused with `EPERM`.
#[def_test]
fn mqueue_root_rejects_child_creation() {
    let fs = mqueue_tree_for_ipc_namespace(&MQUEUE_TYPE, SuperBlockFlags::empty());
    let mount = Mount::new_root(&fs);
    let root = mount.root_path();
    let created = kvfs::Filename::new("queue").open_with_flags_at(
        &root,
        &root,
        O_RDWR | O_CREAT | O_EXCL,
        NodePermission::from_bits_truncate(0o600),
        NodePermission::empty(),
        kcred::initial_cred(),
    );
    assert!(
        created.is_err(),
        "queue file creation is refused on the sealed mqueue root"
    );
    assert!(
        root
            .mkdir(
                "subdir",
                NodePermission::from_bits_truncate(0o755),
                &kcred::initial_cred(),
            )
            .is_err(),
        "mkdir is refused on the sealed mqueue root"
    );
}

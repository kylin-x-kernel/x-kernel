// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Tmpfs inode-space and backing-inode lifetime contracts.

use kvfs::Filename;
use linux_raw_sys::general::{O_CREAT, O_EXCL, O_RDWR};
use unittest::{assert, assert_eq, def_test};

use super::*;

fn mount_tmpfs(data: &[u8]) -> VfsResult<(Arc<SuperBlock>, kvfs::Path)> {
    let lookup_fs = MemoryFs::new();
    let lookup_root = Mount::new_root(&lookup_fs).root_path();
    let cred = kcred::initial_cred();
    let mut context = FsContext::new(
        &TMPFS_TYPE,
        None,
        Some(data),
        SuperBlockFlags::empty(),
        &cred,
    )?;
    let fs = context.get_tree(&lookup_root, &lookup_root)?;
    let root = Mount::new_root(&fs).root_path();
    Ok((fs, root))
}

fn open_file(root: &kvfs::Path, name: &str) -> VfsResult<Arc<VfsFile>> {
    Filename::new(name).open_with_flags_at(
        root,
        root,
        O_RDWR | O_CREAT | O_EXCL,
        NodePermission::from_bits_truncate(0o600),
        NodePermission::empty(),
        kcred::initial_cred(),
    )
}

fn remount(fs: &Arc<SuperBlock>, data: &[u8]) -> VfsResult<()> {
    let cred = kcred::initial_cred();
    let mut context = FsContext::new_reconfigure(
        fs,
        None,
        Some(data),
        SuperBlockFlags::empty(),
        SuperBlockFlags::RDONLY,
        &cred,
    )?;
    fs.reconfigure(&mut context)
}

fn capacities(fs: &SuperBlock) -> VfsResult<(u64, u64, u64, u64)> {
    let stat = fs.stat()?;
    Ok((
        stat.blocks,
        stat.blocks_free,
        stat.file_count,
        stat.free_file_count,
    ))
}

#[def_test]
fn openrc_run_mount_accounts_root_and_accepts_800k_inodes() {
    // mount(8) passes nosuid/nodev/strictatime through MS_* flags, leaving
    // these filesystem-specific options in mount data.
    let (fs, root) = mount_tmpfs(b"mode=0755,nr_inodes=800k,size=20%\0").unwrap();
    let stat = fs.stat().unwrap();
    assert_eq!(stat.file_count, 819200);
    assert_eq!(stat.free_file_count, 819199);
    assert_eq!(root.metadata().mode.permission().bits(), 0o755);
    assert_eq!(
        stat.blocks,
        (global_allocator().total_pages() as u64 * 20).div_ceil(100)
    );
    let file = open_file(&root, "openrc-state").unwrap();
    assert_eq!(fs.stat().unwrap().free_file_count, 819198);
    assert_eq!(file.write(b"ready"), Ok(5));
}

#[def_test]
fn root_consumes_the_last_inode_and_all_node_types_return_enospc() {
    let (fs, root) = mount_tmpfs(b"nr_inodes=1").unwrap();
    let cred = kcred::initial_cred();
    let permission = NodePermission::from_bits_truncate(0o700);
    assert_eq!(fs.stat().unwrap().free_file_count, 0);
    assert_eq!(
        open_file(&root, "file").map(|_| ()),
        Err(VfsError::StorageFull)
    );
    assert_eq!(
        root.mkdir("dir", permission, &cred).map(|_| ()),
        Err(VfsError::StorageFull)
    );
    assert_eq!(
        root.symlink("link", "target", &cred).map(|_| ()),
        Err(VfsError::StorageFull)
    );
    assert_eq!(
        Filename::new("fifo")
            .mknod_at(
                &root,
                &root,
                Umode::new(NodeType::Fifo, permission),
                DeviceId::default(),
                NodePermission::empty(),
                &cred,
            )
            .map(|_| ()),
        Err(VfsError::StorageFull)
    );
    assert_eq!(fs.stat().unwrap().free_file_count, 0);
    remount(&fs, b"nr_inodes=2").unwrap();
    // Reusing the name also checks that failed creation published no entry.
    open_file(&root, "file").unwrap();
    assert_eq!(fs.stat().unwrap().free_file_count, 0);
}

#[def_test]
fn unlinked_open_inode_is_charged_until_the_last_file_reference_drops() {
    let (fs, root) = mount_tmpfs(b"size=4096,nr_inodes=2").unwrap();
    let cred = kcred::initial_cred();
    let file = open_file(&root, "file").unwrap();
    file.write(b"live").unwrap();
    let second_reference = file.clone();
    root.unlink("file", &cred).unwrap();
    assert_eq!(capacities(&fs).unwrap(), (1, 0, 2, 0));
    assert_eq!(
        open_file(&root, "next").map(|_| ()),
        Err(VfsError::StorageFull)
    );
    drop(file);
    assert_eq!(fs.stat().unwrap().free_file_count, 0);
    drop(second_reference);
    assert_eq!(capacities(&fs).unwrap(), (1, 1, 2, 1));
    open_file(&root, "next").unwrap();
}

#[def_test]
fn backing_reference_returns_inode_space_after_vfs_eviction() {
    let (fs, root) = mount_tmpfs(b"nr_inodes=2").unwrap();
    let file = open_file(&root, "file").unwrap();
    // Keep the backing reference that lookup acquires before publishing or
    // reusing a VFS identity, so its release follows the operation object's.
    let backing = file.inode().private::<MemoryNode>().unwrap().inode.clone();
    root.unlink("file", &kcred::initial_cred()).unwrap();
    drop(file);
    assert_eq!(fs.stat().unwrap().free_file_count, 0);
    drop(backing);
    assert_eq!(fs.stat().unwrap().free_file_count, 1);
    open_file(&root, "next").unwrap();
}

#[def_test]
fn retained_backing_inode_keeps_its_number_until_final_release() {
    let (fs, root) = mount_tmpfs(b"nr_inodes=3").unwrap();
    let file = open_file(&root, "file").unwrap();
    let ino = file.inode().inode();
    let backing = file.inode().private::<MemoryNode>().unwrap().inode.clone();
    root.unlink("file", &kcred::initial_cred()).unwrap();
    drop(file);
    let other = open_file(&root, "other").unwrap();
    assert!(other.inode().inode() != ino);
    assert_eq!(fs.stat().unwrap().free_file_count, 0);
    drop(backing);
    let replacement = open_file(&root, "replacement").unwrap();
    assert_eq!(replacement.inode().inode(), ino);
    assert_eq!(fs.stat().unwrap().free_file_count, 0);
}

#[def_test]
fn unpublished_backing_inode_returns_its_allocation_charge() {
    let (fs, _root) = mount_tmpfs(b"nr_inodes=2").unwrap();
    let memory_fs = fs.private::<Arc<MemoryFs>>().unwrap();
    let inode = Inode::new(
        memory_fs,
        None,
        NodeType::RegularFile,
        NodePermission::from_bits_truncate(0o600),
        0,
        0,
        DeviceId::default(),
    )
    .unwrap();
    assert_eq!(fs.stat().unwrap().free_file_count, 0);
    drop(inode);
    assert_eq!(fs.stat().unwrap().free_file_count, 1);
}

#[def_test]
fn hard_links_consume_inode_space_and_unlink_returns_only_the_extra_charge() {
    let (fs, root) = mount_tmpfs(b"nr_inodes=4").unwrap();
    let cred = kcred::initial_cred();
    let file = open_file(&root, "file").unwrap();
    let alias = root.link("alias", file.path(), &cred).unwrap();
    assert_eq!(alias.inode().inode(), file.inode().inode());
    assert_eq!(fs.stat().unwrap().free_file_count, 1);
    remount(&fs, b"nr_inodes=3").unwrap();
    assert_eq!(fs.stat().unwrap().file_count, 3);
    assert_eq!(remount(&fs, b"nr_inodes=2"), Err(VfsError::InvalidInput));
    assert_eq!(fs.stat().unwrap().file_count, 3);
    assert_eq!(fs.stat().unwrap().free_file_count, 0);
    assert_eq!(
        root.link("overflow", file.path(), &cred).map(|_| ()),
        Err(VfsError::StorageFull)
    );
    assert_eq!(file.inode().link_count(), 2);
    drop(alias);
    root.unlink("file", &cred).unwrap();
    assert_eq!(fs.stat().unwrap().free_file_count, 1);
    root.unlink("alias", &cred).unwrap();
    assert_eq!(fs.stat().unwrap().free_file_count, 1);
    drop(file);
    assert_eq!(fs.stat().unwrap().free_file_count, 2);
}

#[def_test]
fn directory_hard_link_rejection_preserves_inode_capacity() {
    let (fs, root) = mount_tmpfs(b"nr_inodes=3").unwrap();
    let cred = kcred::initial_cred();
    let dir = root
        .mkdir("dir", NodePermission::from_bits_truncate(0o700), &cred)
        .unwrap();
    let before = capacities(&fs).unwrap();
    assert_eq!(
        root.link("alias", &dir, &cred).map(|_| ()),
        Err(VfsError::OperationNotPermitted)
    );
    assert_eq!(capacities(&fs).unwrap(), before);
    assert_eq!(dir.inode().link_count(), 2);
    open_file(&root, "alias").unwrap();
    assert_eq!(fs.stat().unwrap().free_file_count, 0);
}

#[def_test]
fn directory_dot_links_do_not_consume_extra_inode_space() {
    let (fs, root) = mount_tmpfs(b"nr_inodes=3").unwrap();
    let cred = kcred::initial_cred();
    let dir = root
        .mkdir("dir", NodePermission::from_bits_truncate(0o700), &cred)
        .unwrap();
    assert_eq!(fs.stat().unwrap().free_file_count, 1);
    let child = open_file(&dir, "child").unwrap();
    assert_eq!(fs.stat().unwrap().free_file_count, 0);
    assert_eq!(root.rmdir("dir", &cred), Err(VfsError::DirectoryNotEmpty));
    dir.unlink("child", &cred).unwrap();
    drop(child);
    assert_eq!(fs.stat().unwrap().free_file_count, 1);
    root.rmdir("dir", &cred).unwrap();
    assert_eq!(fs.stat().unwrap().free_file_count, 1);
    drop(dir);
    assert_eq!(fs.stat().unwrap().free_file_count, 2);
}

#[def_test]
fn rename_preserves_source_charge_and_defers_open_victim_release() {
    let (fs, root) = mount_tmpfs(b"nr_inodes=3").unwrap();
    let cred = kcred::initial_cred();
    let source = open_file(&root, "source").unwrap();
    let victim = open_file(&root, "victim").unwrap();
    root.rename("source", &root, "victim", kvfs::RenameFlags::empty(), &cred)
        .unwrap();
    assert_eq!(fs.stat().unwrap().free_file_count, 0);
    drop(victim);
    assert_eq!(fs.stat().unwrap().free_file_count, 1);
    root.unlink("victim", &cred).unwrap();
    drop(source);
    assert_eq!(fs.stat().unwrap().free_file_count, 2);
}

#[def_test]
fn remount_validates_both_limits_before_changing_either() {
    let (fs, root) = mount_tmpfs(b"size=16384,nr_inodes=4").unwrap();
    let file = open_file(&root, "file").unwrap();
    file.write(&[0x5a; 8192]).unwrap();
    assert_eq!(capacities(&fs).unwrap(), (4, 2, 4, 2));
    assert_eq!(
        remount(&fs, b"size=32768,nr_inodes=1"),
        Err(VfsError::InvalidInput)
    );
    assert_eq!(capacities(&fs).unwrap(), (4, 2, 4, 2));
    assert_eq!(
        remount(&fs, b"size=4096,nr_inodes=8"),
        Err(VfsError::InvalidInput)
    );
    assert_eq!(capacities(&fs).unwrap(), (4, 2, 4, 2));
    remount(&fs, b"size=8192,nr_inodes=2").unwrap();
    assert_eq!(capacities(&fs).unwrap(), (2, 0, 2, 0));
    remount(&fs, b"nr_inodes=3").unwrap();
    assert_eq!(capacities(&fs).unwrap(), (2, 0, 3, 1));
    remount(&fs, b"size=16384").unwrap();
    assert_eq!(capacities(&fs).unwrap(), (4, 2, 3, 1));
}

#[def_test]
fn unlimited_remount_cannot_be_retroactively_limited() {
    let (fs, root) = mount_tmpfs(b"size=4096,nr_inodes=2").unwrap();
    let cred = kcred::initial_cred();
    let file = open_file(&root, "file").unwrap();
    file.write(b"data").unwrap();
    remount(&fs, b"size=0,nr_inodes=0").unwrap();
    assert_eq!(capacities(&fs).unwrap(), (0, 0, 0, 0));
    assert_eq!(remount(&fs, b"nr_inodes=100"), Err(VfsError::InvalidInput));
    assert_eq!(remount(&fs, b"size=65536"), Err(VfsError::InvalidInput));
    remount(&fs, b"nr_inodes=0").unwrap();
    root.unlink("file", &cred).unwrap();
    drop(file);
    open_file(&root, "next").unwrap();
    assert_eq!(capacities(&fs).unwrap(), (0, 0, 0, 0));
}

#[def_test]
fn ramfs_and_anonymous_tmpfs_remain_unlimited() {
    for fs in [
        MemoryFs::new(),
        shmem::new_anonymous_tmpfs(SuperBlockFlags::empty()),
    ] {
        let root = Mount::new_root(&fs).root_path();
        open_file(&root, "file").unwrap();
        assert_eq!(capacities(&fs).unwrap(), (0, 0, 0, 0));
    }
}

#[def_test]
fn rename_over_a_hard_link_releases_only_its_directory_entry_charge() {
    let (fs, root) = mount_tmpfs(b"nr_inodes=4").unwrap();
    let cred = kcred::initial_cred();
    let source = open_file(&root, "source").unwrap();
    let victim = open_file(&root, "victim").unwrap();
    drop(root.link("alias", victim.path(), &cred).unwrap());
    assert_eq!(fs.stat().unwrap().free_file_count, 0);
    root.rename("source", &root, "alias", kvfs::RenameFlags::empty(), &cred)
        .unwrap();
    assert_eq!(fs.stat().unwrap().free_file_count, 1);
    assert_eq!(victim.inode().link_count(), 1);
    root.unlink("victim", &cred).unwrap();
    assert_eq!(fs.stat().unwrap().free_file_count, 1);
    drop(victim);
    assert_eq!(fs.stat().unwrap().free_file_count, 2);
    root.unlink("alias", &cred).unwrap();
    drop(source);
    assert_eq!(fs.stat().unwrap().free_file_count, 3);
}

#[def_test]
fn inode_capacity_is_per_superblock_and_symlink_release_restores_it() {
    let (first, first_root) = mount_tmpfs(b"nr_inodes=2").unwrap();
    let (second, second_root) = mount_tmpfs(b"nr_inodes=2").unwrap();
    let cred = kcred::initial_cred();
    let link = first_root.symlink("link", "missing", &cred).unwrap();
    assert_eq!(first.stat().unwrap().free_file_count, 0);
    assert_eq!(second.stat().unwrap().free_file_count, 1);
    open_file(&second_root, "independent").unwrap();
    first_root.unlink("link", &cred).unwrap();
    drop(link);
    assert_eq!(first.stat().unwrap().free_file_count, 1);
    assert_eq!(second.stat().unwrap().free_file_count, 0);
}

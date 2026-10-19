// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;

use alloc::{string::String, sync::Arc};

use ktime_types::SystemTime;
use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::{
    Dentry, DirContext, FileDirOperations, FileOperations, FsContextPurpose, InodeDirOperations,
    InodeOperations, LockedDentry, Metadata, MetadataUpdate, NodePermission, NodeType, StatFs,
    StatFsFlags, SuperBlockFlags, SuperBlockOperations, VfsError, VfsFile, VfsInode, VfsInodeInit,
    VfsResult,
};

fn lookup_child_in_mount(path: &Path, name: &str) -> VfsResult<Path> {
    path.dentry
        .as_dir()?
        .lookup(name)
        .map(|dentry| path.with_dentry(dentry))
}

fn lookup_no_follow(path: &Path, name: &str) -> VfsResult<Path> {
    use crate::path::{DOT, DOTDOT};

    Ok(match name {
        DOT => path.clone(),
        DOTDOT => path.parent().unwrap_or_else(|| path.clone()),
        _ => lookup_child_in_mount(path, name)?.resolve_final_mount(),
    })
}

fn mount_filesystem(path: &Path, fs: &Arc<SuperBlock>) -> VfsResult<Arc<Mount>> {
    path.mount_filesystem_with_requested_superblock_flags(fs, MountFlags::empty(), None, None)
}

struct MockFilesystem;

static MOCK_SUPER_OPERATIONS: MockFilesystem = MockFilesystem;

struct MockSuperPrivate {
    mount_flags: StatFsFlags,
}

impl SuperBlockOperations for MockFilesystem {
    fn statfs(&self, _super_block: &SuperBlock) -> VfsResult<StatFs> {
        statfs()
    }

    fn timestamp_limits(&self, _super_block: &SuperBlock) -> crate::TimestampLimits {
        crate::TimestampLimits::NANOSECOND
    }
}

fn mock_get_tree(
    _context: &mut FsContext<'_>,
    _lookup_root: &Path,
    _lookup_pwd: &Path,
) -> VfsResult<Arc<SuperBlock>> {
    Err(VfsError::NoSuchDevice)
}

fn mock_reconfigure(context: &mut FsContext<'_>) -> VfsResult<()> {
    let super_block = context.super_block()?;
    if context.private::<FsContextPurpose>()? != &FsContextPurpose::Reconfigure
        || context.super_private::<SuperBlockFlags>()? != &context.sb_flags()
    {
        return Err(VfsError::InvalidInput);
    }
    if super_block
        .private::<MockSuperPrivate>()?
        .mount_flags
        .contains(StatFsFlags::RDONLY)
        && !context.sb_flags().contains(SuperBlockFlags::RDONLY)
    {
        return Err(VfsError::ReadOnlyFilesystem);
    }
    Ok(())
}

static MOCK_CONTEXT_OPERATIONS: crate::FsContextOperations =
    crate::FsContextOperations::with_reconfigure(mock_get_tree, mock_reconfigure);

fn init_mock_context(context: &mut FsContext<'_>) -> VfsResult<()> {
    context.set_operations(&MOCK_CONTEXT_OPERATIONS);
    context.set_private(context.purpose());
    context.set_super_private(context.sb_flags());
    Ok(())
}

static MOCK_FILE_SYSTEM_TYPE: crate::FileSystemType =
    crate::FileSystemType::nodev("mockfs", init_mock_context);

struct MockDirOps {
    mount_flags: StatFsFlags,
    inode: u64,
}

impl MockDirOps {
    fn new(mount_flags: StatFsFlags, inode: u64) -> Self {
        Self { mount_flags, inode }
    }
}

impl InodeOperations for MockDirOps {
    fn directory_operations(&self) -> Option<&dyn InodeDirOperations> {
        Some(self)
    }

    fn getattr(
        &self,
        _idmap: &crate::MountIdmap,
        _path: Option<&crate::Path>,
        _request_mask: crate::GetattrRequestMask,
        _query_flags: crate::GetattrQueryFlags,
    ) -> VfsResult<Metadata> {
        Ok(Metadata {
            device: 0,
            inode: self.inode,
            nlink: 1,
            mode: crate::Umode::new(NodeType::Directory, NodePermission::default()),
            uid: 0,
            gid: 0,
            size: 0,
            block_size: 512,
            blocks: 1,
            rdev: Default::default(),
            atime: SystemTime::UNIX_EPOCH,
            mtime: SystemTime::UNIX_EPOCH,
            ctime: SystemTime::UNIX_EPOCH,
        })
    }

    fn setattr(
        &self,
        _idmap: &crate::MountIdmap,
        _dentry: &Dentry,
        update: MetadataUpdate,
    ) -> VfsResult<MetadataUpdate> {
        Ok(update)
    }
}

impl InodeDirOperations for MockDirOps {
    fn lookup(
        &self,
        _dir: &VfsInode,
        dentry: &LockedDentry<'_>,
        _flags: crate::InodeLookupFlags,
    ) -> VfsResult<Option<Dentry>> {
        let name = dentry.name();
        if name != "mnt" {
            return Ok(None);
        }

        let inode = VfsInode::new_openable_dir(
            Arc::new(MockDirOps::new(self.mount_flags, self.inode + 1)),
            inode_init(self.inode + 1),
        );
        dentry.instantiate_or_alias(inode)
    }

    fn create(
        &self,
        _idmap: &crate::MountIdmap,
        _dir: &VfsInode,
        _dentry: &LockedDentry<'_>,
        _mode: crate::Umode,
        _exclusive: bool,
        _cred: &Cred,
    ) -> VfsResult<()> {
        Err(VfsError::OperationNotSupported)
    }

    fn mkdir(
        &self,
        _idmap: &crate::MountIdmap,
        dir: &VfsInode,
        dentry: &LockedDentry<'_>,
        mode: crate::Umode,
        cred: &Cred,
    ) -> VfsResult<()> {
        let (mode, uid, gid) = crate::inode_init_owner(dir, mode, cred);
        let inode = self.inode + 1;
        let init = VfsInodeInit::new(inode, 0, mode)
            .with_owner_links_and_rdev(uid, gid, 1, Default::default())
            .with_stat_data(
                512,
                1,
                SystemTime::UNIX_EPOCH,
                SystemTime::UNIX_EPOCH,
                SystemTime::UNIX_EPOCH,
            );
        let inode =
            VfsInode::new_openable_dir(Arc::new(MockDirOps::new(self.mount_flags, inode)), init);
        dentry.instantiate(inode)
    }

    fn link(
        &self,
        _old_dentry: &Dentry,
        _dir: &VfsInode,
        _new_dentry: &LockedDentry<'_>,
    ) -> VfsResult<()> {
        Err(VfsError::OperationNotSupported)
    }

    fn unlink(&self, _dir: &VfsInode, _dentry: &LockedDentry<'_>) -> VfsResult<()> {
        Err(VfsError::OperationNotSupported)
    }

    fn rename(
        &self,
        _idmap: &crate::MountIdmap,
        _old_dir: &VfsInode,
        _old_dentry: &LockedDentry<'_>,
        _new_dir: &VfsInode,
        _new_dentry: &LockedDentry<'_>,
        _flags: crate::RenameFlags,
    ) -> VfsResult<()> {
        Err(VfsError::OperationNotSupported)
    }
}

fn inode_init(inode: u64) -> VfsInodeInit {
    VfsInodeInit::new(
        inode,
        0,
        crate::Umode::new(NodeType::Directory, NodePermission::default()),
    )
    .with_owner_links_and_rdev(0, 0, 1, Default::default())
    .with_stat_data(
        512,
        1,
        SystemTime::UNIX_EPOCH,
        SystemTime::UNIX_EPOCH,
        SystemTime::UNIX_EPOCH,
    )
}

impl FileOperations for MockDirOps {
    fn dir_operations(&self) -> Option<&dyn FileDirOperations> {
        Some(self)
    }

    fn supports_read(&self) -> bool {
        true
    }

    fn read(&self, _file: &VfsFile, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::IsADirectory)
    }
}

impl FileDirOperations for MockDirOps {
    fn iterate_shared(&self, _file: &VfsFile, _ctx: &mut DirContext<'_>) -> VfsResult<usize> {
        Ok(0)
    }
}

fn mock_filesystem(mount_flags: StatFsFlags) -> Arc<SuperBlock> {
    let inode =
        VfsInode::new_openable_dir(Arc::new(MockDirOps::new(mount_flags, 1)), inode_init(1));
    let root = Dentry::new_dir_from_inode(inode, None, String::new());
    let mut superblock_flags = SuperBlockFlags::empty();
    if mount_flags.contains(StatFsFlags::RDONLY) {
        superblock_flags.insert(SuperBlockFlags::RDONLY);
    }
    SuperBlock::new_with_flags_and_private(
        &MOCK_FILE_SYSTEM_TYPE,
        &MOCK_SUPER_OPERATIONS,
        MockSuperPrivate { mount_flags },
        superblock_flags,
        1,
        crate::MAX_LFS_FILESIZE,
        |_| root,
    )
}

fn statfs() -> VfsResult<StatFs> {
    Ok(StatFs {
        fs_type: 0,
        block_size: 0,
        blocks: 0,
        blocks_free: 0,
        blocks_available: 0,
        file_count: 0,
        free_file_count: 0,
        name_length: 255,
        fragment_size: 0,
    })
}

#[def_test]
fn test_mountpoint_thread_safety() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Mount>();
}

#[def_test]
fn test_root_mount_defaults_to_writable() {
    let fs = mock_filesystem(StatFsFlags::empty());
    let mount = Mount::new_root(&fs);
    let root = mount.root_path();

    assert_eq!(mount.flags(), MountFlags::empty());
    assert!(!mount.is_readonly());
    assert!(!root.is_mount_readonly());
    assert!(!root.is_effectively_readonly());
    assert_eq!(root.check_writable_mount(), Ok(()));
}

#[def_test]
fn test_root_mount_can_be_readonly() {
    let fs = mock_filesystem(StatFsFlags::empty());
    let mount = Mount::new_root_with_flags(&fs, MountFlags::RDONLY);
    let root = mount.root_path();

    assert!(mount.flags().contains(MountFlags::RDONLY));
    assert!(mount.is_readonly());
    assert!(root.is_mount_readonly());
    assert!(root.is_effectively_readonly());
    assert_eq!(
        root.check_writable_mount(),
        Err(VfsError::ReadOnlyFilesystem)
    );
}

#[def_test]
fn test_child_mount_flags_are_independent_from_parent() {
    let root_fs = mock_filesystem(StatFsFlags::empty());
    let child_fs = mock_filesystem(StatFsFlags::empty());
    let root_mount = Mount::new_root_with_flags(&root_fs, MountFlags::RDONLY);
    let mount_dir = lookup_no_follow(&root_mount.root_path(), "mnt").unwrap();
    let child_mount = mount_dir
        .mount_filesystem_with_requested_superblock_flags(
            &child_fs,
            MountFlags::empty(),
            None,
            None,
        )
        .unwrap();
    let child_root = child_mount.root_path();

    assert!(root_mount.is_readonly());
    assert!(!child_mount.is_readonly());
    assert!(!child_root.is_mount_readonly());
    assert!(!child_root.is_effectively_readonly());
}

#[def_test]
fn test_filesystem_stat_readonly_makes_location_effectively_readonly() {
    let fs = mock_filesystem(StatFsFlags::RDONLY);
    let mount = Mount::new_root(&fs);
    let root = mount.root_path();

    assert!(!mount.is_readonly());
    assert!(!root.is_mount_readonly());
    assert!(root.is_effectively_readonly());
    assert_eq!(
        root.check_writable_mount(),
        Err(VfsError::ReadOnlyFilesystem)
    );
}

#[def_test]
fn test_mount_flags_combine() {
    let flags =
        MountFlags::RDONLY | MountFlags::NOSUID | MountFlags::NOEXEC | MountFlags::NOSYMFOLLOW;
    assert!(flags.contains(MountFlags::RDONLY));
    assert!(flags.contains(MountFlags::NOSUID));
    assert!(flags.contains(MountFlags::NOEXEC));
    assert!(flags.contains(MountFlags::NOSYMFOLLOW));
    assert!(!flags.contains(MountFlags::NODEV));
    assert!(!flags.contains(MountFlags::NOATIME));
}

#[def_test]
fn test_mount_flags_relatime() {
    let flags = MountFlags::RELATIME | MountFlags::NOSYMFOLLOW;
    assert!(flags.contains(MountFlags::RELATIME));
    assert!(!flags.contains(MountFlags::RDONLY));
    assert!(flags.contains(MountFlags::NOSYMFOLLOW));
}

#[def_test]
fn test_initial_namespace_layers_visible_root_over_structural_root() {
    let root_fs = mock_filesystem(StatFsFlags::empty());
    let namespace = MntNamespace::build_initial_mount_tree(&root_fs).unwrap();

    let structural_root = namespace.root_path();
    let visible_root = namespace.visible_root_path();

    assert_eq!(structural_root.mount().filesystem_name(), "nullfs");
    assert_eq!(visible_root.mount().filesystem_name(), "mockfs");
    assert!(!structural_root.ptr_eq(&visible_root));
    assert_eq!(visible_root.absolute_path(), Ok(PathBuf::from("/")));
}

#[def_test]
fn test_overmount_hides_previous_and_restores_on_unmount() {
    let root_fs = mock_filesystem(StatFsFlags::empty());
    let fs_a = mock_filesystem(StatFsFlags::empty());
    let fs_b = mock_filesystem(StatFsFlags::empty());

    let root_mount = Mount::new_root(&root_fs);
    let mnt_loc = lookup_no_follow(&root_mount.root_path(), "mnt").unwrap();

    let mount_a = mount_filesystem(&mnt_loc, &fs_a).unwrap();
    let loc_a = lookup_no_follow(&root_mount.root_path(), "mnt").unwrap();
    assert!(Arc::ptr_eq(loc_a.mount(), &mount_a));

    let mount_b = mnt_loc
        .mount_filesystem_with_requested_superblock_flags(&fs_b, MountFlags::RDONLY, None, None)
        .unwrap();
    let loc_b = lookup_no_follow(&root_mount.root_path(), "mnt").unwrap();
    assert!(Arc::ptr_eq(loc_b.mount(), &mount_b));
    assert!(loc_b.is_mount_readonly());

    mount_b.root_path().unmount().unwrap();
    let loc_a_again = lookup_no_follow(&root_mount.root_path(), "mnt").unwrap();
    assert!(Arc::ptr_eq(loc_a_again.mount(), &mount_a));
}

#[def_test]
fn test_mount_root_keeps_root_dentry_name_and_dotdot_crosses_mount() {
    let root_fs = mock_filesystem(StatFsFlags::empty());
    let child_fs = mock_filesystem(StatFsFlags::empty());
    let root_mount = Mount::new_root(&root_fs);
    let mount_dir = lookup_no_follow(&root_mount.root_path(), "mnt").unwrap();
    let child_mount = mount_filesystem(&mount_dir, &child_fs).unwrap();
    let child_root = child_mount.root_path();
    let child_path = child_root.absolute_path().unwrap();

    assert_eq!(child_root.name(), "");
    assert_eq!(child_path.as_str(), "/mnt");
    assert_eq!(
        child_root.render_from(&root_mount.root_path()),
        Ok(RenderedPath::Reachable(PathBuf::from("/mnt")))
    );
    assert!(child_root.dentry().is_root_of_mount());
    assert!(
        lookup_no_follow(&child_root, ".")
            .unwrap()
            .ptr_eq(&child_root)
    );
    assert_eq!(
        lookup_no_follow(&child_root, "..").unwrap().absolute_path(),
        Ok(PathBuf::from("/"))
    );
}

#[def_test]
fn test_nested_mount_root_dotdot_crosses_to_parent_mount() {
    let root_fs = mock_filesystem(StatFsFlags::empty());
    let child_fs = mock_filesystem(StatFsFlags::empty());
    let grandchild_fs = mock_filesystem(StatFsFlags::empty());

    let root_mount = Mount::new_root(&root_fs);
    let first_mount_dir = lookup_no_follow(&root_mount.root_path(), "mnt").unwrap();
    let child_mount = mount_filesystem(&first_mount_dir, &child_fs).unwrap();
    let second_mount_dir = lookup_no_follow(&child_mount.root_path(), "mnt").unwrap();
    let grandchild_mount = mount_filesystem(&second_mount_dir, &grandchild_fs).unwrap();
    let grandchild_root = grandchild_mount.root_path();
    let grandchild_path = grandchild_root.absolute_path().unwrap();

    assert_eq!(grandchild_path.as_str(), "/mnt/mnt");
    assert_eq!(
        grandchild_root.render_from(&root_mount.root_path()),
        Ok(RenderedPath::Reachable(PathBuf::from("/mnt/mnt")))
    );
    assert_eq!(grandchild_root.name(), "");
    assert!(grandchild_root.dentry().is_root_of_mount());
    assert!(
        lookup_no_follow(&grandchild_root, "..")
            .unwrap()
            .absolute_path()
            == Ok(PathBuf::from("/mnt"))
    );
}

#[def_test]
fn test_render_from_reports_unreachable_mount_tree() {
    let first = Mount::new_root(&mock_filesystem(StatFsFlags::empty())).root_path();
    let second = Mount::new_root(&mock_filesystem(StatFsFlags::empty())).root_path();

    assert_eq!(
        second.render_from(&first),
        Ok(RenderedPath::Unreachable(PathBuf::from("/")))
    );
}

#[def_test]
fn test_mountpoint_child_tracking_updates_for_mount_and_unmount() {
    let root_fs = mock_filesystem(StatFsFlags::empty());
    let child_fs = mock_filesystem(StatFsFlags::empty());

    let root_mount = Mount::new_root(&root_fs);
    assert_eq!(root_mount.children().len(), 0);

    let mount_dir = lookup_no_follow(&root_mount.root_path(), "mnt").unwrap();
    let child_mount = mount_filesystem(&mount_dir, &child_fs).unwrap();

    let children = root_mount.children();
    assert_eq!(children.len(), 1);
    assert!(Arc::ptr_eq(&children[0], &child_mount));

    child_mount.root_path().unmount().unwrap();
    assert_eq!(root_mount.children().len(), 0);
}

#[def_test]
fn test_bind_mount_unmount_preserves_source_dentry() {
    let root_fs = mock_filesystem(StatFsFlags::empty());
    let namespace = MntNamespace::new_root(&root_fs, kcred::initial_user_namespace());
    let root = namespace.root_path();
    let mountpoint = lookup_child_in_mount(&root, "mnt").unwrap();

    let bind = namespace.attach_bind(&root, &mountpoint).unwrap();
    assert_eq!(bind.flags(), root.mount().flags());
    assert_eq!(bind.root_path().inode_key(), root.inode_key());
    assert!(Arc::ptr_eq(
        lookup_no_follow(&root, "mnt").unwrap().mount(),
        &bind
    ));

    namespace.detach(&bind.root_path()).unwrap();
    assert_eq!(
        lookup_no_follow(&root, "mnt").unwrap().node_type(),
        NodeType::Directory
    );
    assert_eq!(root.node_type(), NodeType::Directory);
}

#[def_test]
fn test_remount_updates_mount_and_shared_superblock_flags() {
    let root_fs = mock_filesystem(StatFsFlags::empty());
    let child_fs = mock_filesystem(StatFsFlags::empty());
    let namespace = MntNamespace::new_root(&root_fs, kcred::initial_user_namespace());
    let root = namespace.root_path();
    let mountpoint = lookup_child_in_mount(&root, "mnt").unwrap();
    let cred = kcred::initial_cred();
    let mut readonly_context = FsContext::new_reconfigure(
        child_fs.as_ref(),
        None,
        None,
        SuperBlockFlags::RDONLY,
        SuperBlockFlags::RDONLY,
        &cred,
    )
    .unwrap();

    assert_eq!(
        namespace.remount(&mountpoint, &mut readonly_context, MountFlags::RDONLY),
        Err(VfsError::InvalidInput)
    );

    let child = namespace.attach(&mountpoint, &child_fs).unwrap();
    let child_root = child.root_path();
    let mut mount_context = FsContext::new(
        &MOCK_FILE_SYSTEM_TYPE,
        None,
        None,
        SuperBlockFlags::RDONLY,
        &cred,
    )
    .unwrap();
    assert_eq!(
        namespace.remount(&child_root, &mut mount_context, MountFlags::RDONLY),
        Err(VfsError::InvalidInput)
    );
    assert_eq!(child.flags(), MountFlags::empty());
    assert_eq!(child.super_block_flags(), SuperBlockFlags::empty());

    let bind_mountpoint = lookup_child_in_mount(&child_root, "mnt").unwrap();
    let bind = namespace
        .attach_bind(&child_root, &bind_mountpoint)
        .unwrap();
    let bind_root = bind.root_path();

    namespace
        .remount(
            &child_root,
            &mut readonly_context,
            MountFlags::RDONLY | MountFlags::NOEXEC,
        )
        .unwrap();

    assert_eq!(child.flags(), MountFlags::RDONLY | MountFlags::NOEXEC);
    assert!(!bind.flags().contains(MountFlags::RDONLY));
    assert_eq!(
        child_root.check_writable_mount(),
        Err(VfsError::ReadOnlyFilesystem)
    );
    assert_eq!(
        bind_root.check_writable_mount(),
        Err(VfsError::ReadOnlyFilesystem)
    );

    let mut writable_context = FsContext::new_reconfigure(
        child_fs.as_ref(),
        None,
        None,
        SuperBlockFlags::empty(),
        SuperBlockFlags::RDONLY,
        &cred,
    )
    .unwrap();
    namespace
        .remount(&child_root, &mut writable_context, MountFlags::NOEXEC)
        .unwrap();
    assert!(child_root.check_writable_mount().is_ok());
    assert!(bind_root.check_writable_mount().is_ok());

    namespace
        .reconfigure_mount(&bind_root, MountFlags::RDONLY)
        .unwrap();
    assert!(child_root.check_writable_mount().is_ok());
    assert_eq!(
        bind_root.check_writable_mount(),
        Err(VfsError::ReadOnlyFilesystem)
    );
}

#[def_test]
fn test_reconfigure_rw_rejects_backend_readonly_superblock() {
    let root_fs = mock_filesystem(StatFsFlags::empty());
    let child_fs = mock_filesystem(StatFsFlags::RDONLY);
    let namespace = MntNamespace::new_root(&root_fs, kcred::initial_user_namespace());
    let root = namespace.root_path();
    let mountpoint = lookup_child_in_mount(&root, "mnt").unwrap();
    let child = namespace.attach(&mountpoint, &child_fs).unwrap();
    let cred = kcred::initial_cred();
    let mut writable_context = FsContext::new_reconfigure(
        child_fs.as_ref(),
        None,
        None,
        SuperBlockFlags::empty(),
        SuperBlockFlags::RDONLY,
        &cred,
    )
    .unwrap();

    assert_eq!(
        namespace.remount(
            &child.root_path(),
            &mut writable_context,
            MountFlags::empty()
        ),
        Err(VfsError::ReadOnlyFilesystem)
    );
}

#[def_test]
fn test_mount_namespace_clone_copies_tree_and_retargets_paths() {
    let root_fs = mock_filesystem(StatFsFlags::empty());
    let child_fs = mock_filesystem(StatFsFlags::empty());
    let overmount_fs = mock_filesystem(StatFsFlags::empty());

    let namespace = MntNamespace::new_root(&root_fs, kcred::initial_user_namespace());
    let root = namespace.root_path();
    let mountpoint = lookup_child_in_mount(&root, "mnt").unwrap();
    let child_mount = namespace.attach(&mountpoint, &child_fs).unwrap();
    let pwd = lookup_no_follow(&root, "mnt").unwrap();

    let cloned = namespace.clone_with_root_and_pwd(&root, &pwd).unwrap();

    assert!(!Arc::ptr_eq(
        namespace.root_mount(),
        cloned.namespace.root_mount()
    ));
    assert!(!Arc::ptr_eq(pwd.mount(), cloned.pwd.mount()));
    assert!(!Arc::ptr_eq(&child_mount, cloned.pwd.mount()));
    assert_eq!(
        cloned.pwd.mount().filesystem_name(),
        child_mount.filesystem_name()
    );

    let overmount = namespace.attach(&mountpoint, &overmount_fs).unwrap();
    let old_visible = lookup_no_follow(&root, "mnt").unwrap();
    let cloned_visible = lookup_no_follow(&cloned.root, "mnt").unwrap();

    assert!(Arc::ptr_eq(old_visible.mount(), &overmount));
    assert!(Arc::ptr_eq(cloned_visible.mount(), cloned.pwd.mount()));
}

#[def_test]
fn test_pivot_root_swaps_visible_root_and_relocates_old() {
    let root_fs = mock_filesystem(StatFsFlags::empty());
    let new_root_fs = mock_filesystem(StatFsFlags::empty());
    let namespace = MntNamespace::build_initial_mount_tree(&root_fs).unwrap();
    let old_visible_root = namespace.visible_root_path();
    let old_root_mount = old_visible_root.mount().clone();
    let cred = kcred::initial_cred();

    // New root: a fresh filesystem mounted at a directory of the old root
    // (the OCI pattern mounts the container rootfs there and binds it).
    let mnt_dir = lookup_child_in_mount(&old_visible_root, "mnt").unwrap();
    let new_mnt = namespace.attach(&mnt_dir, &new_root_fs).unwrap();

    // put_old lives inside the new root's own filesystem (`/.pivot_root`).
    let put_old = new_mnt
        .root_path()
        .mkdir("oldroot", NodePermission::default(), &cred)
        .unwrap();

    namespace
        .pivot_root(
            &new_mnt.root_path(),
            &put_old,
            &namespace.visible_root_path(),
        )
        .unwrap();

    // `/` now resolves to the new root's filesystem.
    assert!(Arc::ptr_eq(namespace.visible_root_path().mount(), &new_mnt));
    // The old root is reachable at put_old, inside the new root.
    let relocated = lookup_child_in_mount(&new_mnt.root_path(), "oldroot")
        .unwrap()
        .resolve_final_mount();
    assert!(Arc::ptr_eq(relocated.mount(), &old_root_mount));
    // The structural root resolves through the new mount without gaps.
    assert!(Arc::ptr_eq(
        namespace.root_path().resolve_final_mount().mount(),
        &new_mnt
    ));
    assert!(!Arc::ptr_eq(
        namespace.visible_root_path().mount(),
        &old_root_mount
    ));
}

#[def_test]
fn test_pivot_root_rejects_plain_directory_new_root() {
    let root_fs = mock_filesystem(StatFsFlags::empty());
    let namespace = MntNamespace::build_initial_mount_tree(&root_fs).unwrap();
    let visible = namespace.visible_root_path();
    let cred = kcred::initial_cred();
    let plain_dir = visible
        .mkdir("plain", NodePermission::default(), &cred)
        .unwrap();
    let put_old = visible
        .mkdir("oldroot", NodePermission::default(), &cred)
        .unwrap();

    assert_eq!(
        namespace.pivot_root(&plain_dir, &put_old, &namespace.visible_root_path()),
        Err(VfsError::InvalidInput),
        "new_root must be a mount root"
    );
}

#[def_test]
fn test_pivot_root_rejects_mounted_put_old() {
    let root_fs = mock_filesystem(StatFsFlags::empty());
    let new_root_fs = mock_filesystem(StatFsFlags::empty());
    let put_old_fs = mock_filesystem(StatFsFlags::empty());
    let namespace = MntNamespace::build_initial_mount_tree(&root_fs).unwrap();
    let visible = namespace.visible_root_path();
    let cred = kcred::initial_cred();
    let new_mnt = namespace
        .attach(
            &visible
                .mkdir("first", NodePermission::default(), &cred)
                .unwrap(),
            &new_root_fs,
        )
        .unwrap();
    let put_old_mountpoint = visible
        .mkdir("second", NodePermission::default(), &cred)
        .unwrap();
    namespace.attach(&put_old_mountpoint, &put_old_fs).unwrap();

    assert_eq!(
        namespace.pivot_root(
            &new_mnt.root_path(),
            &put_old_mountpoint,
            &namespace.visible_root_path()
        ),
        Err(VfsError::ResourceBusy),
        "put_old inside the old root's filesystem is a loop (Linux EBUSY)"
    );
}

#[def_test]
fn test_pivot_root_allows_put_old_equal_to_new_root() {
    // runc's `pivot_root(".", ".")` pattern: the relocated old root
    // temporarily covers the new root until the caller detaches it.
    let root_fs = mock_filesystem(StatFsFlags::empty());
    let new_root_fs = mock_filesystem(StatFsFlags::empty());
    let namespace = MntNamespace::build_initial_mount_tree(&root_fs).unwrap();
    let old_visible_root = namespace.visible_root_path();
    let old_root_mount = old_visible_root.mount().clone();

    let mnt_dir = lookup_child_in_mount(&old_visible_root, "mnt").unwrap();
    let new_mnt = namespace.attach(&mnt_dir, &new_root_fs).unwrap();
    let new_root_path = new_mnt.root_path();

    namespace
        .pivot_root(
            &new_root_path,
            &new_root_path,
            &namespace.visible_root_path(),
        )
        .unwrap();

    // The old root now overmounts the new root's own root dentry, so `/`
    // fully resolves into the old root exactly as on Linux...
    assert!(Arc::ptr_eq(
        namespace.visible_root_path().mount(),
        &old_root_mount
    ));
    // ...and the caller-detach (umount(".", MNT_DETACH)) uncovers it.
    namespace
        .detach_tree(&namespace.visible_root_path())
        .unwrap();
    assert!(Arc::ptr_eq(namespace.visible_root_path().mount(), &new_mnt));
}

#[def_test]
fn test_unresolved_root_path_resolves_covering_mount_after_pivot() {
    // `umount` receives a `.`-style lookup that does not cross a covering
    // mount; it must resolve the covering mount before detaching, as
    // `sys_umount2` now does, so `umount(".", MNT_DETACH)` after
    // `pivot_root(".", ".")` detaches the relocated old root.
    let root_fs = mock_filesystem(StatFsFlags::empty());
    let new_root_fs = mock_filesystem(StatFsFlags::empty());
    let namespace = MntNamespace::build_initial_mount_tree(&root_fs).unwrap();
    let old_visible_root = namespace.visible_root_path();
    let old_root_mount = old_visible_root.mount().clone();

    let mnt_dir = lookup_child_in_mount(&old_visible_root, "mnt").unwrap();
    let new_mnt = namespace.attach(&mnt_dir, &new_root_fs).unwrap();
    let new_root_path = new_mnt.root_path();

    namespace
        .pivot_root(
            &new_root_path,
            &new_root_path,
            &namespace.visible_root_path(),
        )
        .unwrap();

    // The unresolved path still names the new root...
    assert!(Arc::ptr_eq(new_root_path.mount(), &new_mnt));
    // ...while resolving follows the covering old root that `umount` must
    // detach instead.
    let covering = new_root_path.resolve_final_mount();
    assert!(Arc::ptr_eq(covering.mount(), &old_root_mount));
    namespace.detach_tree(&covering).unwrap();
    assert!(Arc::ptr_eq(namespace.visible_root_path().mount(), &new_mnt));
}

#[def_test]
fn test_pivot_root_rejects_put_old_outside_new_root() {
    let root_fs = mock_filesystem(StatFsFlags::empty());
    let new_root_fs = mock_filesystem(StatFsFlags::empty());
    let other_fs = mock_filesystem(StatFsFlags::empty());
    let namespace = MntNamespace::build_initial_mount_tree(&root_fs).unwrap();
    let visible = namespace.visible_root_path();
    let cred = kcred::initial_cred();
    let new_mnt = namespace
        .attach(
            &visible
                .mkdir("newroot", NodePermission::default(), &cred)
                .unwrap(),
            &new_root_fs,
        )
        .unwrap();
    let other_mnt = namespace
        .attach(
            &visible
                .mkdir("other", NodePermission::default(), &cred)
                .unwrap(),
            &other_fs,
        )
        .unwrap();
    let put_old = other_mnt
        .root_path()
        .mkdir("oldroot", NodePermission::default(), &cred)
        .unwrap();

    assert_eq!(
        namespace.pivot_root(
            &new_mnt.root_path(),
            &put_old,
            &namespace.visible_root_path()
        ),
        Err(VfsError::InvalidInput),
        "put_old must be reachable from new_root"
    );
}

#[def_test]
fn test_pivot_root_rejects_current_visible_root_as_new_root() {
    let root_fs = mock_filesystem(StatFsFlags::empty());
    let namespace = MntNamespace::build_initial_mount_tree(&root_fs).unwrap();
    let put_old = namespace
        .visible_root_path()
        .mkdir("oldroot", NodePermission::default(), &kcred::initial_cred())
        .unwrap();

    assert_eq!(
        namespace.pivot_root(
            &namespace.visible_root_path(),
            &put_old,
            &namespace.visible_root_path()
        ),
        Err(VfsError::ResourceBusy),
        "pivoting onto the current visible root is the same-filesystem loop (Linux EBUSY)"
    );
}

#[def_test]
fn test_unmount_rejects_non_mount_root_path() {
    let root_fs = mock_filesystem(StatFsFlags::empty());
    let root_mount = Mount::new_root(&root_fs);
    let mount_dir = lookup_no_follow(&root_mount.root_path(), "mnt").unwrap();

    assert_eq!(mount_dir.unmount(), Err(VfsError::InvalidInput));
    assert_eq!(mount_dir.unmount_tree(), Err(VfsError::InvalidInput));
}

#[def_test]
fn test_make_mount_private_accepts_visible_initial_root() {
    let root_fs = mock_filesystem(StatFsFlags::empty());
    let namespace = MntNamespace::build_initial_mount_tree(&root_fs).unwrap();
    let visible_root = namespace.visible_root_path();

    assert!(!visible_root.ptr_eq(&namespace.root_path()));
    assert_eq!(namespace.make_mount_private(&visible_root), Ok(()));
}

#[def_test]
fn test_make_mount_private_rejects_non_mount_root_path() {
    let root_fs = mock_filesystem(StatFsFlags::empty());
    let namespace = MntNamespace::new_root(&root_fs, kcred::initial_user_namespace());
    let mount_dir = lookup_no_follow(&namespace.root_path(), "mnt").unwrap();

    assert_eq!(
        namespace.make_mount_private(&mount_dir),
        Err(VfsError::InvalidInput)
    );
}

#[def_test]
fn test_unmount_rejects_mount_with_nested_children() {
    let root_fs = mock_filesystem(StatFsFlags::empty());
    let child_fs = mock_filesystem(StatFsFlags::empty());
    let grandchild_fs = mock_filesystem(StatFsFlags::empty());

    let root_mount = Mount::new_root(&root_fs);
    let first_mount_dir = lookup_no_follow(&root_mount.root_path(), "mnt").unwrap();
    let child_mount = mount_filesystem(&first_mount_dir, &child_fs).unwrap();
    let second_mount_dir = lookup_no_follow(&child_mount.root_path(), "mnt").unwrap();
    let grandchild_mount = mount_filesystem(&second_mount_dir, &grandchild_fs).unwrap();

    assert_eq!(
        child_mount.root_path().unmount(),
        Err(VfsError::ResourceBusy)
    );
    assert_eq!(child_mount.children().len(), 1);
    assert_eq!(root_mount.children().len(), 1);

    grandchild_mount.root_path().unmount().unwrap();
    child_mount.root_path().unmount().unwrap();
    assert_eq!(root_mount.children().len(), 0);
}

#[def_test]
fn test_unmount_all_recursively_clears_nested_mounts() {
    let root_fs = mock_filesystem(StatFsFlags::empty());
    let child_fs = mock_filesystem(StatFsFlags::empty());
    let grandchild_fs = mock_filesystem(StatFsFlags::empty());

    let root_mount = Mount::new_root(&root_fs);
    let first_mount_dir = lookup_no_follow(&root_mount.root_path(), "mnt").unwrap();
    let child_mount = mount_filesystem(&first_mount_dir, &child_fs).unwrap();
    let second_mount_dir = lookup_no_follow(&child_mount.root_path(), "mnt").unwrap();
    let grandchild_mount = mount_filesystem(&second_mount_dir, &grandchild_fs).unwrap();

    child_mount.root_path().unmount_tree().unwrap();

    assert_eq!(root_mount.children().len(), 0);
    assert_eq!(child_mount.children().len(), 0);
    assert_eq!(
        lookup_no_follow(&root_mount.root_path(), "mnt")
            .unwrap()
            .absolute_path(),
        Ok(PathBuf::from("/mnt"))
    );
    assert_eq!(grandchild_mount.children().len(), 0);
}

#[def_test]
fn test_hidden_mount_cannot_be_unmounted_while_overmounted() {
    let root_fs = mock_filesystem(StatFsFlags::empty());
    let fs_a = mock_filesystem(StatFsFlags::empty());
    let fs_b = mock_filesystem(StatFsFlags::empty());

    let root_mount = Mount::new_root(&root_fs);
    let mount_dir = lookup_no_follow(&root_mount.root_path(), "mnt").unwrap();
    let mount_a = mount_filesystem(&mount_dir, &fs_a).unwrap();
    let mount_b = mount_filesystem(&mount_dir, &fs_b).unwrap();

    assert_eq!(mount_a.root_path().unmount(), Err(VfsError::InvalidInput));

    mount_b.root_path().unmount().unwrap();
    mount_a.root_path().unmount().unwrap();
    assert_eq!(root_mount.children().len(), 0);
}

#[def_test]
fn test_metadata_uses_synthetic_device_identity() {
    let root_fs = mock_filesystem(StatFsFlags::empty());
    let child_fs = mock_filesystem(StatFsFlags::empty());

    let root_mount = Mount::new_root(&root_fs);
    let mount_dir = lookup_no_follow(&root_mount.root_path(), "mnt").unwrap();
    let child_mount = mount_filesystem(&mount_dir, &child_fs).unwrap();
    let child_root = child_mount.root_path();

    let root_metadata = root_mount.root_path().getattr().unwrap();
    let child_metadata = child_root.getattr().unwrap();

    assert_eq!(root_metadata.inode, 1);
    assert_eq!(child_metadata.inode, 1);
    assert_eq!(root_metadata.device, root_mount.synthetic_device_id());
    assert_eq!(child_metadata.device, child_mount.synthetic_device_id());
    assert!(root_metadata.device != child_metadata.device);
}

#[def_test]
fn test_readonly_mount_blocks_create() {
    let fs = mock_filesystem(StatFsFlags::empty());
    let mount = Mount::new_root_with_flags(&fs, MountFlags::RDONLY);
    let root = mount.root_path();

    let result = root.create("test", NodePermission::default(), &kcred::initial_cred());
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), VfsError::ReadOnlyFilesystem);
}

#[def_test]
fn test_readonly_mount_blocks_unlink() {
    let fs = mock_filesystem(StatFsFlags::empty());
    let mount = Mount::new_root_with_flags(&fs, MountFlags::RDONLY);
    let root = mount.root_path();

    let result = root.unlink("test", &kcred::initial_cred());
    assert_eq!(result, Err(VfsError::ReadOnlyFilesystem));
}

#[def_test]
fn test_readonly_mount_blocks_rename() {
    let fs = mock_filesystem(StatFsFlags::empty());
    let mount = Mount::new_root_with_flags(&fs, MountFlags::RDONLY);
    let root = mount.root_path();

    let result = root.rename(
        "a",
        &root,
        "b",
        RenameFlags::empty(),
        &kcred::initial_cred(),
    );
    assert_eq!(result, Err(VfsError::ReadOnlyFilesystem));
}

#[def_test]
fn test_writable_mount_allows_operations_to_reach_filesystem() {
    let fs = mock_filesystem(StatFsFlags::empty());
    let mount = Mount::new_root(&fs);
    let root = mount.root_path();

    let result = root.create("test", NodePermission::default(), &kcred::initial_cred());
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), VfsError::OperationNotSupported);

    assert_eq!(
        root.unlink("test", &kcred::initial_cred()),
        Err(VfsError::NotFound)
    );
}

#[def_test]
fn test_mkdir_strips_requested_setid_bits_before_owner_initialization() {
    let fs = mock_filesystem(StatFsFlags::empty());
    let root = Mount::new_root(&fs).root_path();
    let requested = NodePermission::from_bits_truncate(0o7777);

    let ordinary = root
        .mkdir("ordinary", requested, &kcred::initial_cred())
        .unwrap();
    let ordinary_permission = ordinary.metadata().mode.permission();
    assert!(!ordinary_permission.contains(NodePermission::SET_UID));
    assert!(!ordinary_permission.contains(NodePermission::SET_GID));
    assert!(ordinary_permission.contains(NodePermission::STICKY));

    root.dentry
        .update_metadata(MetadataUpdate {
            mode: Some(NodePermission::from_bits_truncate(0o2777)),
            owner: Some((0, 1234)),
            ..Default::default()
        })
        .unwrap();
    let inherited = root
        .mkdir("inherited", requested, &kcred::initial_cred())
        .unwrap();
    let inherited_metadata = inherited.metadata();
    assert!(
        !inherited_metadata
            .mode
            .permission()
            .contains(NodePermission::SET_UID)
    );
    assert!(
        inherited_metadata
            .mode
            .permission()
            .contains(NodePermission::SET_GID)
    );
    assert_eq!(inherited_metadata.gid, 1234);
}

#[def_test]
fn test_sticky_directory_rejects_unrelated_user_delete() {
    let fs = mock_filesystem(StatFsFlags::empty());
    let mount = Mount::new_root(&fs);
    let root = mount.root_path();
    root.dentry
        .update_metadata(MetadataUpdate {
            mode: Some(NodePermission::from_bits_truncate(0o1777)),
            owner: Some((1000, 1000)),
            ..Default::default()
        })
        .unwrap();
    let victim = lookup_child_in_mount(&root, "mnt").unwrap();
    victim
        .dentry
        .update_metadata(MetadataUpdate {
            owner: Some((2000, 2000)),
            ..Default::default()
        })
        .unwrap();

    assert_eq!(
        root.rmdir("mnt", &kcred::Cred::new(3000, 3000)),
        Err(VfsError::OperationNotPermitted)
    );
}

#[def_test]
fn test_metadata_owner_and_group_authorization() {
    let fs = mock_filesystem(StatFsFlags::empty());
    let root = Mount::new_root(&fs).root_path();
    root.dentry
        .update_metadata(MetadataUpdate {
            mode: Some(NodePermission::from_bits_truncate(0o666)),
            owner: Some((1000, 100)),
            ..Default::default()
        })
        .unwrap();

    let mut owner = Cred::new(1000, 100);
    owner.set_supplementary_groups(vec![200]);
    let other = Cred::new(2000, 300);

    assert_eq!(
        root.chmod(NodePermission::from_bits_truncate(0o600), &other),
        Err(VfsError::OperationNotPermitted)
    );
    assert_eq!(
        root.chown(Some(2000), None, &owner),
        Err(VfsError::OperationNotPermitted)
    );
    assert_eq!(
        root.chown(None, Some(300), &owner),
        Err(VfsError::OperationNotPermitted)
    );

    root.chown(None, Some(200), &owner).unwrap();
    assert_eq!(root.metadata().gid, 200);
    root.chmod(NodePermission::from_bits_truncate(0o2660), &owner)
        .unwrap();
    assert!(
        root.metadata()
            .mode
            .permission()
            .contains(NodePermission::SET_GID)
    );
}

#[def_test]
fn test_timestamp_authorization_distinguishes_current_and_explicit_values() {
    let fs = mock_filesystem(StatFsFlags::empty());
    let root = Mount::new_root(&fs).root_path();
    root.dentry
        .update_metadata(MetadataUpdate {
            mode: Some(NodePermission::from_bits_truncate(0o666)),
            owner: Some((1000, 100)),
            ..Default::default()
        })
        .unwrap();
    let other = Cred::new(2000, 200);

    root.set_times(
        Some(SetattrTime::Current(SystemTime::from_unix_seconds(10))),
        Some(SetattrTime::Current(SystemTime::from_unix_seconds(11))),
        &other,
    )
    .unwrap();
    let metadata = root.metadata();
    assert_eq!(metadata.atime, SystemTime::from_unix_seconds(10));
    assert_eq!(metadata.mtime, SystemTime::from_unix_seconds(11));
    assert_eq!(
        root.set_times(
            Some(SetattrTime::Explicit(SystemTime::from_unix_seconds(20))),
            None,
            &other,
        ),
        Err(VfsError::OperationNotPermitted)
    );
    assert_eq!(
        root.set_times(
            Some(SetattrTime::Current(SystemTime::from_unix_seconds(30))),
            None,
            &other,
        ),
        Err(VfsError::OperationNotPermitted)
    );
}

#[def_test]
fn test_stat_rdonly_combined_with_mount_rdonly_is_readonly() {
    let fs = mock_filesystem(StatFsFlags::RDONLY);
    let mount = Mount::new_root_with_flags(&fs, MountFlags::RDONLY);
    let root = mount.root_path();

    assert!(mount.is_readonly());
    assert!(root.is_effectively_readonly());
    assert_eq!(
        root.check_writable_mount(),
        Err(VfsError::ReadOnlyFilesystem)
    );
}

#[def_test]
fn test_statfs_flags_values() {
    assert_eq!(StatFsFlags::RDONLY.bits(), 0x0001);
    assert_eq!(StatFsFlags::NOSUID.bits(), 0x0002);
    assert_eq!(StatFsFlags::NODEV.bits(), 0x0004);
    assert_eq!(StatFsFlags::NOEXEC.bits(), 0x0008);
    assert_eq!(StatFsFlags::NOATIME.bits(), 0x0400);
    assert_eq!(StatFsFlags::NODIRATIME.bits(), 0x0800);
    assert_eq!(StatFsFlags::RELATIME.bits(), 0x1000);
    assert_eq!(StatFsFlags::NOSYMFOLLOW.bits(), 0x2000);
}

#[def_test]
fn test_mount_flags_constants_values() {
    assert_eq!(MountFlags::NOSUID.bits(), 0x01);
    assert_eq!(MountFlags::NODEV.bits(), 0x02);
    assert_eq!(MountFlags::NOEXEC.bits(), 0x04);
    assert_eq!(MountFlags::NOATIME.bits(), 0x08);
    assert_eq!(MountFlags::NODIRATIME.bits(), 0x10);
    assert_eq!(MountFlags::RELATIME.bits(), 0x20);
    assert_eq!(MountFlags::RDONLY.bits(), 0x40);
    assert_eq!(MountFlags::NOSYMFOLLOW.bits(), 0x80);
}

/// Seeds all three resident timestamps to `time` through the direct
/// setters, bypassing setattr policy.
fn seed_times(root: &Path, time: SystemTime) {
    let inode = root.inode();
    inode.set_accessed_at(time);
    inode.set_modified_at(time);
    inode.set_changed_at(time);
}

#[def_test]
fn test_inode_times_returns_seeded_timestamps() {
    let fs = mock_filesystem(StatFsFlags::empty());
    let root = Mount::new_root(&fs).root_path();

    let accessed_at = SystemTime::from_unix_parts(11, 111).unwrap();
    let modified_at = SystemTime::from_unix_parts(12, 222).unwrap();
    let changed_at = SystemTime::from_unix_parts(13, 333).unwrap();

    let inode = root.inode();
    inode.set_accessed_at(accessed_at);
    inode.set_modified_at(modified_at);
    inode.set_changed_at(changed_at);

    let times = inode.times();
    assert_eq!(times.accessed_at, accessed_at);
    assert_eq!(times.modified_at, modified_at);
    assert_eq!(times.changed_at, changed_at);
}

#[def_test]
fn test_relatime_future_atime_skips_touch_atime() {
    let fs = mock_filesystem(StatFsFlags::empty());
    let root = Mount::new_root_with_flags(&fs, MountFlags::RELATIME).root_path();

    let accessed_at = SystemTime::from_unix_parts(4_102_444_800, 0).unwrap();
    let inode = root.inode();
    inode.set_accessed_at(accessed_at);
    inode.set_modified_at(SystemTime::from_unix_seconds(1));
    inode.set_changed_at(SystemTime::from_unix_seconds(1));

    root.touch_atime();

    let times = inode.times();
    assert_eq!(times.accessed_at, accessed_at);
    assert_eq!(times.modified_at, SystemTime::from_unix_seconds(1));
    assert_eq!(times.changed_at, SystemTime::from_unix_seconds(1));
}

#[def_test]
fn test_relatime_equal_times_updates_atime() {
    let fs = mock_filesystem(StatFsFlags::empty());
    let root = Mount::new_root_with_flags(&fs, MountFlags::RELATIME).root_path();
    seed_times(&root, SystemTime::from_unix_seconds(1));

    let before = root.inode().current_time();
    root.touch_atime();

    let times = root.inode().times();
    assert!(times.accessed_at >= before);
    assert_eq!(times.modified_at, SystemTime::from_unix_seconds(1));
    assert_eq!(times.changed_at, SystemTime::from_unix_seconds(1));
}

#[def_test]
fn test_update_cmtime_advances_mtime_and_ctime() {
    let fs = mock_filesystem(StatFsFlags::empty());
    let root = Mount::new_root(&fs).root_path();
    seed_times(&root, SystemTime::from_unix_seconds(1));

    let before = root.inode().current_time();
    assert_eq!(root.update_cmtime(), Ok(()));

    let times = root.inode().times();
    assert!(times.modified_at >= before);
    assert_eq!(times.modified_at, times.changed_at);
    assert_eq!(times.accessed_at, SystemTime::from_unix_seconds(1));
}

#[def_test]
fn test_readonly_mount_skips_time_updates() {
    let fs = mock_filesystem(StatFsFlags::empty());
    let root = Mount::new_root_with_flags(&fs, MountFlags::RDONLY).root_path();
    seed_times(&root, SystemTime::from_unix_seconds(1));

    assert_eq!(root.update_cmtime(), Ok(()));
    root.touch_atime();

    let times = root.inode().times();
    assert_eq!(times.accessed_at, SystemTime::from_unix_seconds(1));
    assert_eq!(times.modified_at, SystemTime::from_unix_seconds(1));
    assert_eq!(times.changed_at, SystemTime::from_unix_seconds(1));
}

#[def_test]
fn test_non_relatime_overwrites_future_atime() {
    let fs = mock_filesystem(StatFsFlags::empty());
    let root = Mount::new_root(&fs).root_path();

    let inode = root.inode();
    inode.set_accessed_at(SystemTime::from_unix_parts(4_102_444_800, 0).unwrap());
    inode.set_modified_at(SystemTime::from_unix_seconds(1));
    inode.set_changed_at(SystemTime::from_unix_seconds(1));

    let before = root.inode().current_time();
    root.touch_atime();

    assert!(inode.times().accessed_at >= before);
}

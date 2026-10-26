// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::String, sync::Arc, vec::Vec};

use hashbrown::HashMap;
use ktime_types::SystemTime;
use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::{
    Dentry, DirContext, FileDirOperations, FileOperations, InodeDirOperations, InodeOperations,
    InodeSymlinkOperations, LockedDentry, MagicLinkOps, Metadata, MetadataUpdate, Mount,
    NodePermission, NodeType, ResolvedObject, StatFs, SuperBlock, SuperBlockOperations, VfsError,
    VfsFile, VfsInode, VfsInodeInit,
};

#[def_test]
fn lookup_flags_are_typed_not_raw_bools() {
    let flags = LookupFlags::follow() | LookupFlags::NO_MAGIC_LINKS;
    assert!(flags.follows_final());
    assert!(flags.rejects_magic_links());
    assert_eq!(DEFAULT_MAX_LINKS, 40);
}

struct TestSuperBlock;

static TEST_SUPER_BLOCK_OPERATIONS: TestSuperBlock = TestSuperBlock;
static TEST_FILE_SYSTEM_TYPE: crate::FileSystemType = crate::FileSystemType::internal("namei-test");

impl SuperBlockOperations for TestSuperBlock {
    fn statfs(&self, _super_block: &SuperBlock) -> VfsResult<StatFs> {
        Ok(StatFs {
            fs_type: 0,
            block_size: 4096,
            blocks: 0,
            blocks_free: 0,
            blocks_available: 0,
            file_count: 0,
            free_file_count: 0,
            name_length: 255,
            fragment_size: 4096,
        })
    }
}

struct TestDir {
    inode: u64,
    children: crate::Mutex<HashMap<String, Dentry>>,
    creations: crate::Mutex<Vec<CreateRecord>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CreateRecord {
    mode: Umode,
    exclusive: bool,
    device: DeviceId,
}

impl TestDir {
    fn new(inode: u64) -> Self {
        Self {
            inode,
            children: crate::Mutex::default(),
            creations: crate::Mutex::default(),
        }
    }

    fn insert(&self, name: &str, entry: Dentry) {
        self.children.lock().insert(String::from(name), entry);
    }

    fn creations(&self) -> Vec<CreateRecord> {
        self.creations.lock().clone()
    }

    fn instantiate_created(
        &self,
        dentry: &LockedDentry<'_>,
        mode: Umode,
        device: DeviceId,
    ) -> VfsResult<()> {
        let inode = self.inode + self.creations.lock().len() as u64 + 100;
        let init = VfsInodeInit::new(inode, 0, mode)
            .with_owner_links_and_rdev(0, 0, 1, device)
            .with_stat_data(
                4096,
                1,
                SystemTime::UNIX_EPOCH,
                SystemTime::UNIX_EPOCH,
                SystemTime::UNIX_EPOCH,
            );
        let inode = VfsInode::new_file(
            Arc::new(TestFile::new(inode, mode.node_type(), &[], None)),
            init,
        );
        dentry.instantiate(inode)
    }
}

impl InodeOperations for TestDir {
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
        Ok(kstat(self.inode, NodeType::Directory, 0))
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

impl InodeDirOperations for TestDir {
    fn lookup(
        &self,
        _dir: &VfsInode,
        dentry: &LockedDentry<'_>,
        _flags: crate::InodeLookupFlags,
    ) -> VfsResult<Option<Dentry>> {
        let entry = self.children.lock().get(dentry.name()).cloned();
        let Some(entry) = entry else {
            return Ok(None);
        };
        dentry.instantiate_or_alias(entry.vfs_inode())
    }

    fn create(
        &self,
        _idmap: &crate::MountIdmap,
        _dir: &VfsInode,
        dentry: &LockedDentry<'_>,
        mode: crate::Umode,
        exclusive: bool,
        _cred: &Cred,
    ) -> VfsResult<()> {
        self.creations.lock().push(CreateRecord {
            mode,
            exclusive,
            device: DeviceId::default(),
        });
        self.instantiate_created(dentry, mode, DeviceId::default())
    }

    fn mkdir(
        &self,
        _idmap: &crate::MountIdmap,
        _dir: &VfsInode,
        dentry: &LockedDentry<'_>,
        mode: Umode,
        _cred: &Cred,
    ) -> VfsResult<()> {
        self.creations.lock().push(CreateRecord {
            mode,
            exclusive: true,
            device: DeviceId::default(),
        });
        let inode = self.inode + self.creations.lock().len() as u64 + 100;
        let inode = VfsInode::new_openable_dir(
            Arc::new(TestDir::new(inode)),
            VfsInodeInit::new(inode, 0, mode)
                .with_owner_links_and_rdev(0, 0, 1, DeviceId::default())
                .with_stat_data(
                    4096,
                    1,
                    SystemTime::UNIX_EPOCH,
                    SystemTime::UNIX_EPOCH,
                    SystemTime::UNIX_EPOCH,
                ),
        );
        dentry.instantiate(inode)
    }

    fn mknod(
        &self,
        _idmap: &crate::MountIdmap,
        _dir: &VfsInode,
        dentry: &LockedDentry<'_>,
        mode: Umode,
        device: DeviceId,
        _cred: &Cred,
    ) -> VfsResult<()> {
        self.creations.lock().push(CreateRecord {
            mode,
            exclusive: true,
            device,
        });
        self.instantiate_created(dentry, mode, device)
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

impl FileOperations for TestDir {
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

impl FileDirOperations for TestDir {
    fn iterate_shared(
        &self,
        _file: &crate::VfsFile,
        _ctx: &mut DirContext<'_>,
    ) -> VfsResult<usize> {
        Ok(0)
    }
}

struct TestFile {
    inode: u64,
    node_type: NodeType,
    data: crate::Mutex<Vec<u8>>,
    release_write_counts: crate::Mutex<Vec<usize>>,
    magic_link: Option<Arc<TestMagicLink>>,
}

impl TestFile {
    fn new(
        inode: u64,
        node_type: NodeType,
        data: &[u8],
        magic_link: Option<Arc<TestMagicLink>>,
    ) -> Self {
        Self {
            inode,
            node_type,
            data: crate::Mutex::new(data.to_vec()),
            release_write_counts: crate::Mutex::new(Vec::new()),
            magic_link,
        }
    }
}

impl InodeOperations for TestFile {
    fn symlink_operations(&self) -> Option<&dyn InodeSymlinkOperations> {
        if self.node_type == NodeType::Symlink {
            Some(self)
        } else {
            None
        }
    }

    fn getattr(
        &self,
        _idmap: &crate::MountIdmap,
        _path: Option<&crate::Path>,
        _request_mask: crate::GetattrRequestMask,
        _query_flags: crate::GetattrQueryFlags,
    ) -> VfsResult<Metadata> {
        Ok(kstat(
            self.inode,
            self.node_type,
            self.data.lock().len() as u64,
        ))
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

impl InodeSymlinkOperations for TestFile {
    fn get_link(
        &self,
        _dentry: Option<&Dentry>,
        _inode: &VfsInode,
        _done: &mut crate::DelayedCall,
    ) -> VfsResult<String> {
        String::from_utf8(self.data.lock().clone()).map_err(|_| VfsError::InvalidData)
    }
}

impl FileOperations for TestFile {
    fn supports_read(&self) -> bool {
        true
    }

    fn supports_write(&self) -> bool {
        true
    }

    fn read(&self, _file: &VfsFile, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let data = self.data.lock();
        let offset = offset as usize;
        if offset >= data.len() {
            return Ok(0);
        }
        let count = buf.len().min(data.len() - offset);
        buf[..count].copy_from_slice(&data[offset..offset + count]);
        Ok(count)
    }

    fn write(&self, _file: &VfsFile, buf: &[u8], offset: u64) -> VfsResult<usize> {
        let offset = offset as usize;
        let mut data = self.data.lock();
        if offset + buf.len() > data.len() {
            data.resize(offset + buf.len(), 0);
        }
        data[offset..offset + buf.len()].copy_from_slice(buf);
        Ok(buf.len())
    }

    fn release(&self, inode: &VfsInode, file: &VfsFile) -> VfsResult<()> {
        if file.mode().contains(FMode::WRITE) {
            self.release_write_counts.lock().push(inode.write_count());
        }
        Ok(())
    }

    fn magic_link(self: Arc<Self>) -> Option<Arc<dyn MagicLinkOps>> {
        self.magic_link
            .clone()
            .map(|link| link as Arc<dyn MagicLinkOps>)
    }
}

struct TestMagicLink {
    display: String,
    target: Path,
    last_intent: crate::Mutex<Option<LookupIntent>>,
    follow_count: crate::Mutex<usize>,
}

impl TestMagicLink {
    fn new(display: &str, target: Path) -> Self {
        Self {
            display: String::from(display),
            target,
            last_intent: crate::Mutex::new(None),
            follow_count: crate::Mutex::new(0),
        }
    }

    fn follow_count(&self) -> usize {
        *self.follow_count.lock()
    }

    fn last_intent(&self) -> Option<LookupIntent> {
        *self.last_intent.lock()
    }
}

impl MagicLinkOps for TestMagicLink {
    fn readlink_display(&self) -> VfsResult<String> {
        Ok(self.display.clone())
    }

    fn follow(&self, intent: LookupIntent, flags: LookupFlags) -> VfsResult<ResolvedObject> {
        if flags.rejects_magic_links() || !flags.follows_final() {
            return Err(VfsError::FilesystemLoop);
        }
        *self.last_intent.lock() = Some(intent);
        *self.follow_count.lock() += 1;
        Ok(ResolvedObject::location(self.target.clone()))
    }
}

struct TestTree {
    fs: Arc<SuperBlock>,
    root: Path,
    base: Path,
    target: Path,
    magic_file: Arc<TestMagicLink>,
    magic_dir: Arc<TestMagicLink>,
}

fn kstat(inode: u64, node_type: NodeType, size: u64) -> Metadata {
    Metadata {
        device: 0,
        inode,
        nlink: 1,
        mode: crate::Umode::new(node_type, NodePermission::default()),
        uid: 0,
        gid: 0,
        size,
        block_size: 4096,
        blocks: 1,
        rdev: Default::default(),
        atime: SystemTime::UNIX_EPOCH,
        mtime: SystemTime::UNIX_EPOCH,
        ctime: SystemTime::UNIX_EPOCH,
    }
}

fn file_entry(
    inode: u64,
    parent: &Dentry,
    name: &str,
    node_type: NodeType,
    data: &[u8],
    magic_link: Option<Arc<TestMagicLink>>,
) -> Dentry {
    let inode = VfsInode::new_file(
        Arc::new(TestFile::new(inode, node_type, data, magic_link)),
        inode_init(inode, node_type, data.len() as u64),
    );
    Dentry::new_file_from_inode(inode, Some(parent.clone()), String::from(name))
}

fn fifo_entry(inode: u64, parent: &Dentry, name: &str) -> Dentry {
    let node = Arc::new(TestFile::new(inode, NodeType::Fifo, &[], None));
    let inode = VfsInode::new_special(
        node,
        crate::NodeFlags::empty(),
        inode_init(inode, NodeType::Fifo, 0),
    );
    Dentry::new_file_from_inode(inode, Some(parent.clone()), String::from(name))
}

fn dir_entry(inode: u64, parent: &Dentry, name: &str) -> (Dentry, Arc<TestDir>) {
    let ops = Arc::new(TestDir::new(inode));
    let inode = VfsInode::new_openable_dir(ops.clone(), inode_init(inode, NodeType::Directory, 0));
    let entry = Dentry::new_dir_from_inode(inode, Some(parent.clone()), String::from(name));
    (entry, ops)
}

fn inode_init(inode: u64, node_type: NodeType, size: u64) -> VfsInodeInit {
    VfsInodeInit::new(
        inode,
        size,
        crate::Umode::new(node_type, NodePermission::default()),
    )
    .with_owner_links_and_rdev(0, 0, 1, Default::default())
    .with_stat_data(
        4096,
        1,
        SystemTime::UNIX_EPOCH,
        SystemTime::UNIX_EPOCH,
        SystemTime::UNIX_EPOCH,
    )
}

fn test_tree() -> TestTree {
    let root_ops = Arc::new(TestDir::new(1));
    let root_inode =
        VfsInode::new_openable_dir(root_ops.clone(), inode_init(1, NodeType::Directory, 0));
    let root = Dentry::new_dir_from_inode(root_inode, None, String::new());
    let target = file_entry(2, &root, "target", NodeType::RegularFile, b"target", None);
    root_ops.insert("target", target.clone());

    let link = file_entry(3, &root, "link", NodeType::Symlink, b"/target", None);
    root_ops.insert("link", link);

    let (target_dir, target_dir_ops) = dir_entry(4, &root, "dir");
    let leaf = file_entry(5, &target_dir, "leaf", NodeType::RegularFile, b"leaf", None);
    target_dir_ops.insert("leaf", leaf.clone());
    root_ops.insert("dir", target_dir.clone());

    let dir_link = file_entry(6, &root, "dirlink", NodeType::Symlink, b"/dir", None);
    root_ops.insert("dirlink", dir_link);

    let autodir_inode = VfsInode::new_dir(
        Arc::new(TestFile::new(9, NodeType::Directory, &[], None)),
        inode_init(9, NodeType::Directory, 0),
    );
    let autodir =
        Dentry::new_dir_from_inode(autodir_inode, Some(root.clone()), String::from("autodir"));
    root_ops.insert("autodir", autodir);

    let fs = SuperBlock::new(&TEST_FILE_SYSTEM_TYPE, &TEST_SUPER_BLOCK_OPERATIONS, |_| {
        root.clone()
    });
    let mount = Mount::new_root(&fs);
    let root_location = mount.root_path();
    let target_location = Path::new(mount.clone(), target);
    let target_dir_location = Path::new(mount.clone(), target_dir);

    let magic_file = Arc::new(TestMagicLink::new(
        "/display/target",
        target_location.clone(),
    ));
    let magic_entry = file_entry(
        7,
        &root,
        "magic",
        NodeType::Symlink,
        b"/display/target",
        Some(magic_file.clone()),
    );
    root_ops.insert("magic", magic_entry);

    let magic_dir = Arc::new(TestMagicLink::new(
        "/display/dir",
        target_dir_location.clone(),
    ));
    let magic_dir_entry = file_entry(
        8,
        &root,
        "magicdir",
        NodeType::Symlink,
        b"/display/dir",
        Some(magic_dir.clone()),
    );
    root_ops.insert("magicdir", magic_dir_entry);

    TestTree {
        fs,
        root: root_location.clone(),
        base: root_location,
        target: target_location,
        magic_file,
        magic_dir,
    }
}

#[def_test]
fn negative_cached_dentry_is_not_a_pathwalk_target() {
    let tree = test_tree();
    let negative = Dentry::new_negative(Some(tree.root.dentry().clone()), String::from("missing"));
    tree.root
        .dentry()
        .insert_cache(String::from("missing"), negative);

    let err = Filename::new("/missing")
        .lookup_at(
            &tree.root,
            &tree.base,
            LookupIntent::Open,
            LookupFlags::follow(),
            &kcred::initial_cred(),
        )
        .unwrap_err();
    assert_eq!(err, VfsError::NotFound);
}

#[def_test]
fn parent_at_preserves_root_final_type() {
    let tree = test_tree();

    let lookup = Filename::new("/")
        .parent_at(
            &tree.root,
            &tree.base,
            LookupIntent::Open,
            &kcred::initial_cred(),
        )
        .unwrap();

    assert!(lookup.parent().ptr_eq(&tree.root));
    assert_eq!(lookup.name(), "/");
    assert_eq!(lookup.last_type(), LastType::Root);
}

#[def_test]
fn mkdir_at_reports_existing_root() {
    let tree = test_tree();

    let err = Filename::new("/")
        .mkdir_at(
            &tree.root,
            &tree.base,
            NodePermission::default(),
            NodePermission::empty(),
            &kcred::initial_cred(),
        )
        .unwrap_err();

    assert_eq!(err, VfsError::AlreadyExists);
}

#[def_test]
fn create_operations_preserve_trailing_slash_policy() {
    let tree = test_tree();

    let err = Filename::new("/missing/")
        .symlink_at(&tree.root, &tree.base, "target", &kcred::initial_cred())
        .unwrap_err();
    assert_eq!(err, VfsError::NotFound);

    let created = Filename::new("/missing/")
        .mkdir_at(
            &tree.root,
            &tree.base,
            NodePermission::default(),
            NodePermission::empty(),
            &kcred::initial_cred(),
        )
        .unwrap();
    assert!(created.parent().unwrap().ptr_eq(&tree.root));
    assert_eq!(created.name(), "missing");
}

#[def_test]
fn may_mknod_assigns_linux_errors_to_decoded_types() {
    assert_eq!(
        may_mknod(Umode::from_bits(0o600)),
        Ok(NodeType::RegularFile)
    );
    assert_eq!(
        may_mknod(Umode::from_bits(0o040700)),
        Err(VfsError::OperationNotPermitted)
    );
    assert_eq!(
        may_mknod(Umode::from_bits(0o120777)),
        Err(VfsError::InvalidInput)
    );
    assert_eq!(
        may_mknod(Umode::from_bits(0o030600)),
        Err(VfsError::InvalidInput)
    );
}

#[def_test]
fn mknod_at_dispatches_types_and_prepares_callback_arguments() {
    let tree = test_tree();
    let root_ops = tree.root.downcast_node::<TestDir>().unwrap();
    let cred = kcred::initial_cred();
    let umask = NodePermission::from_bits_truncate(0o027);

    let regular = Filename::new("/regular")
        .mknod_at(
            &tree.root,
            &tree.base,
            Umode::new(
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o6754),
            ),
            DeviceId::new(8, 1),
            umask,
            &cred,
        )
        .unwrap();
    assert_eq!(regular.node_type(), NodeType::RegularFile);

    let fifo = Filename::new("/fifo")
        .mknod_at(
            &tree.root,
            &tree.base,
            Umode::new(NodeType::Fifo, NodePermission::from_bits_truncate(0o666)),
            DeviceId::new(8, 2),
            umask,
            &cred,
        )
        .unwrap();
    assert_eq!(fifo.node_type(), NodeType::Fifo);

    let device = DeviceId::new(8, 3);
    let character = Filename::new("/character")
        .mknod_at(
            &tree.root,
            &tree.base,
            Umode::new(
                NodeType::CharacterDevice,
                NodePermission::from_bits_truncate(0o666),
            ),
            device,
            umask,
            &cred,
        )
        .unwrap();
    assert_eq!(character.node_type(), NodeType::CharacterDevice);

    let creations = root_ops.creations();
    assert_eq!(creations.len(), 3);
    assert_eq!(creations[0].mode, Umode::from_bits(0o106750));
    assert!(creations[0].exclusive);
    assert_eq!(creations[0].device, DeviceId::default());
    assert_eq!(creations[1].mode, Umode::from_bits(0o010640));
    assert_eq!(creations[1].device, DeviceId::default());
    assert_eq!(creations[2].mode, Umode::from_bits(0o020640));
    assert_eq!(creations[2].device, device);
}

#[def_test]
fn fifo_open_preserves_resolved_path_and_inode_identity() {
    let tree = test_tree();
    let root_ops = tree.root.downcast_node::<TestDir>().unwrap();
    let fifo = fifo_entry(20, tree.root.dentry(), "fifo-open");
    let expected = tree.root.with_dentry(fifo.clone());
    root_ops.insert("fifo-open", fifo);

    let file = Filename::new("/fifo-open")
        .open_with_flags_at(
            &tree.root,
            &tree.base,
            linux_raw_sys::general::O_RDWR | linux_raw_sys::general::O_NONBLOCK,
            NodePermission::empty(),
            NodePermission::empty(),
            kcred::initial_cred(),
        )
        .unwrap();

    assert_eq!(file.path().display_path().unwrap(), "/fifo-open");
    assert!(Arc::ptr_eq(file.inode(), &expected.inode()));
    assert_eq!(file.node_type(), NodeType::Fifo);
    assert_eq!(file.path().metadata().inode, 20);
    assert!(file.is_stream());
    assert!(file.mode().contains(FMode::READ | FMode::WRITE));
}

#[def_test]
fn mkdir_at_uses_locked_vfs_mode_preparation() {
    let tree = test_tree();
    let cred = kcred::initial_cred();
    let created = Filename::new("/created-dir/")
        .mkdir_at(
            &tree.root,
            &tree.base,
            NodePermission::from_bits_truncate(0o7777),
            NodePermission::from_bits_truncate(0o027),
            &cred,
        )
        .unwrap();

    assert!(created.is_dir());
    assert_eq!(
        created.metadata().mode.permission().bits(),
        NodePermission::from_bits_truncate(0o1750).bits()
    );

    let existing = Filename::new("/target").mkdir_at(
        &tree.root,
        &tree.base,
        NodePermission::from_bits_truncate(0o755),
        NodePermission::empty(),
        &cred,
    );
    assert!(matches!(existing, Err(VfsError::AlreadyExists)));
}

#[def_test]
fn mknod_at_preserves_linux_error_precedence() {
    let tree = test_tree();
    tree.root
        .dentry()
        .update_metadata(MetadataUpdate {
            mode: Some(NodePermission::from_bits_truncate(0o777)),
            ..Default::default()
        })
        .unwrap();
    let cred = kcred::Cred::new(1000, 1000);

    let existing = Filename::new("/target").mknod_at(
        &tree.root,
        &tree.base,
        Umode::new(
            NodeType::CharacterDevice,
            NodePermission::from_bits_truncate(0o600),
        ),
        DeviceId::new(1, 3),
        NodePermission::empty(),
        &cred,
    );
    assert!(matches!(existing, Err(VfsError::AlreadyExists)));

    let existing_with_trailing_slash = Filename::new("/target/").mknod_at(
        &tree.root,
        &tree.base,
        Umode::new(
            NodeType::RegularFile,
            NodePermission::from_bits_truncate(0o600),
        ),
        DeviceId::default(),
        NodePermission::empty(),
        &cred,
    );
    assert!(matches!(
        existing_with_trailing_slash,
        Err(VfsError::AlreadyExists)
    ));

    let denied = Filename::new("/device").mknod_at(
        &tree.root,
        &tree.base,
        Umode::new(
            NodeType::CharacterDevice,
            NodePermission::from_bits_truncate(0o600),
        ),
        DeviceId::new(1, 3),
        NodePermission::empty(),
        &cred,
    );
    assert!(matches!(denied, Err(VfsError::OperationNotPermitted)));

    let trailing = Filename::new("/absent/").mknod_at(
        &tree.root,
        &tree.base,
        Umode::new(
            NodeType::RegularFile,
            NodePermission::from_bits_truncate(0o600),
        ),
        DeviceId::default(),
        NodePermission::empty(),
        &cred,
    );
    assert!(matches!(trailing, Err(VfsError::NotFound)));

    let readonly_mount = Mount::new_root_with_flags(&tree.fs, crate::MountFlags::RDONLY);
    let readonly_root = readonly_mount.root_path();
    let existing_readonly = Filename::new("/target").mknod_at(
        &readonly_root,
        &readonly_root,
        Umode::new(
            NodeType::RegularFile,
            NodePermission::from_bits_truncate(0o600),
        ),
        DeviceId::default(),
        NodePermission::empty(),
        &cred,
    );
    assert!(matches!(existing_readonly, Err(VfsError::AlreadyExists)));
    let absent_readonly = Filename::new("/readonly").mknod_at(
        &readonly_root,
        &readonly_root,
        Umode::new(
            NodeType::RegularFile,
            NodePermission::from_bits_truncate(0o600),
        ),
        DeviceId::default(),
        NodePermission::empty(),
        &cred,
    );
    assert!(matches!(absent_readonly, Err(VfsError::ReadOnlyFilesystem)));
}

#[def_test]
fn mknod_at_strips_setgid_like_linux_vfs_prepare_mode() {
    let tree = test_tree();
    tree.root
        .dentry()
        .update_metadata(MetadataUpdate {
            mode: Some(NodePermission::from_bits_truncate(0o2777)),
            owner: Some((0, 4242)),
            ..Default::default()
        })
        .unwrap();
    let cred = kcred::Cred::new(1000, 1000);

    let stripped = Filename::new("/stripped")
        .mknod_at(
            &tree.root,
            &tree.base,
            Umode::new(
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o2770),
            ),
            DeviceId::default(),
            NodePermission::empty(),
            &cred,
        )
        .unwrap();
    assert_eq!(
        stripped.metadata().mode.permission().bits(),
        NodePermission::from_bits_truncate(0o770).bits()
    );

    let preserved = Filename::new("/preserved")
        .mknod_at(
            &tree.root,
            &tree.base,
            Umode::new(
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o2760),
            ),
            DeviceId::default(),
            NodePermission::empty(),
            &cred,
        )
        .unwrap();
    assert_eq!(
        preserved.metadata().mode.permission().bits(),
        NodePermission::from_bits_truncate(0o2760).bits()
    );

    tree.root
        .dentry()
        .update_metadata(MetadataUpdate {
            mode: Some(NodePermission::from_bits_truncate(0o0777)),
            owner: Some((0, 4242)),
            ..Default::default()
        })
        .unwrap();
    let preserved_without_parent_setgid = Filename::new("/ordinary-parent")
        .mknod_at(
            &tree.root,
            &tree.base,
            Umode::new(
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o2770),
            ),
            DeviceId::default(),
            NodePermission::empty(),
            &cred,
        )
        .unwrap();
    assert_eq!(
        preserved_without_parent_setgid
            .metadata()
            .mode
            .permission()
            .bits(),
        NodePermission::from_bits_truncate(0o2770).bits()
    );
}

#[def_test]
fn open_create_uses_shared_vfs_mode_preparation() {
    let tree = test_tree();
    tree.root
        .dentry()
        .update_metadata(MetadataUpdate {
            mode: Some(NodePermission::from_bits_truncate(0o2777)),
            owner: Some((0, 4242)),
            ..Default::default()
        })
        .unwrap();
    let file = Filename::new("/open-created")
        .open_with_flags_at(
            &tree.root,
            &tree.base,
            linux_raw_sys::general::O_CREAT
                | linux_raw_sys::general::O_EXCL
                | linux_raw_sys::general::O_WRONLY,
            NodePermission::from_bits_truncate(0o2776),
            NodePermission::from_bits_truncate(0o006),
            Arc::new(kcred::Cred::new(1000, 1000)),
        )
        .unwrap();

    assert_eq!(
        file.path().metadata().mode.permission().bits(),
        NodePermission::from_bits_truncate(0o770).bits()
    );
}

#[def_test]
fn open_create_existing_precedes_create_only_errors() {
    let tree = test_tree();
    let mount = Mount::new_root_with_flags(&tree.fs, crate::MountFlags::RDONLY);
    let root = mount.root_path();

    let existing = Filename::new("/target").open_with_flags_at(
        &root,
        &root,
        linux_raw_sys::general::O_CREAT | linux_raw_sys::general::O_RDONLY,
        NodePermission::empty(),
        NodePermission::empty(),
        kcred::initial_cred(),
    );
    assert_eq!(existing.map(|_| ()), Ok(()));

    let tree = test_tree();
    let mount = Mount::new_root_with_flags(&tree.fs, crate::MountFlags::RDONLY);
    let root = mount.root_path();
    let exclusive = Filename::new("/target").open_with_flags_at(
        &root,
        &root,
        linux_raw_sys::general::O_CREAT
            | linux_raw_sys::general::O_EXCL
            | linux_raw_sys::general::O_RDONLY,
        NodePermission::empty(),
        NodePermission::empty(),
        kcred::initial_cred(),
    );
    assert!(matches!(exclusive, Err(VfsError::AlreadyExists)));
}

#[def_test]
fn final_symlink_follow_policy_is_typed() {
    let tree = test_tree();

    let followed = Filename::new("/link")
        .lookup_at(
            &tree.root,
            &tree.base,
            LookupIntent::Open,
            LookupFlags::follow(),
            &kcred::initial_cred(),
        )
        .unwrap();
    assert!(followed.dentry().is_same_inode(tree.target.dentry()));

    let link = Filename::new("/link")
        .lookup_at(
            &tree.root,
            &tree.base,
            LookupIntent::Open,
            LookupFlags::no_follow(),
            &kcred::initial_cred(),
        )
        .unwrap();
    assert_eq!(link.dentry().node_type(), NodeType::Symlink);
    assert_eq!(link.dentry().read_link().unwrap(), "/target");
}

#[def_test]
fn non_final_symlink_is_followed_even_when_final_no_follow() {
    let tree = test_tree();

    let leaf = Filename::new("/dirlink/leaf")
        .lookup_at(
            &tree.root,
            &tree.base,
            LookupIntent::Open,
            LookupFlags::no_follow(),
            &kcred::initial_cred(),
        )
        .unwrap();
    assert_eq!(leaf.name(), "leaf");
    assert_eq!(leaf.dentry().node_type(), NodeType::RegularFile);
}

#[def_test]
fn magic_link_follow_uses_lookup_intent() {
    let tree = test_tree();

    let followed = Filename::new("/magic")
        .lookup_at(
            &tree.root,
            &tree.base,
            LookupIntent::Exec,
            LookupFlags::follow(),
            &kcred::initial_cred(),
        )
        .unwrap();
    assert!(followed.ptr_eq(&tree.target));
    assert_eq!(tree.magic_file.last_intent(), Some(LookupIntent::Exec));
    assert_eq!(tree.magic_file.follow_count(), 1);
}

#[def_test]
fn open_magic_link_uses_captured_path_not_display_name() {
    let tree = test_tree();
    // The display name is not a path in this tree. Following the link must
    // use its retained Path, just as procfd retains an unlinked inode.
    let file = Filename::new("/magic")
        .open_with_flags_at(
            &tree.root,
            &tree.base,
            linux_raw_sys::general::O_RDONLY,
            NodePermission::empty(),
            NodePermission::empty(),
            kcred::initial_cred(),
        )
        .unwrap();
    assert!(file.path().ptr_eq(&tree.target));
    assert_eq!(tree.magic_file.last_intent(), Some(LookupIntent::Open));
    assert_eq!(tree.magic_file.follow_count(), 1);
}

#[def_test]
fn open_magic_link_preserves_no_follow_and_exclusive_create() {
    let tree = test_tree();
    for (flags, expected) in [
        (linux_raw_sys::general::O_NOFOLLOW, VfsError::FilesystemLoop),
        (
            linux_raw_sys::general::O_CREAT | linux_raw_sys::general::O_EXCL,
            VfsError::AlreadyExists,
        ),
    ] {
        let result = Filename::new("/magic").open_with_flags_at(
            &tree.root,
            &tree.base,
            flags,
            NodePermission::empty(),
            NodePermission::empty(),
            kcred::initial_cred(),
        );
        assert_eq!(result.err(), Some(expected));
    }
    let file = Filename::new("/magic")
        .open_with_flags_at(
            &tree.root,
            &tree.base,
            linux_raw_sys::general::O_PATH | linux_raw_sys::general::O_NOFOLLOW,
            NodePermission::empty(),
            NodePermission::empty(),
            kcred::initial_cred(),
        )
        .unwrap();
    assert_eq!(file.inode().node_type(), NodeType::Symlink);
    assert_eq!(tree.magic_file.follow_count(), 0);
}

#[def_test]
fn magic_link_no_follow_and_readlink_do_not_follow_target() {
    let tree = test_tree();

    let link = Filename::new("/magic")
        .lookup_at(
            &tree.root,
            &tree.base,
            LookupIntent::Open,
            LookupFlags::no_follow(),
            &kcred::initial_cred(),
        )
        .unwrap();
    assert_eq!(link.dentry().node_type(), NodeType::Symlink);
    assert_eq!(tree.magic_file.follow_count(), 0);

    assert_eq!(
        Filename::new("/magic")
            .readlink_at(&tree.root, &tree.base, &kcred::initial_cred())
            .unwrap(),
        "/display/target"
    );
    assert_eq!(tree.magic_file.follow_count(), 0);
}

#[def_test]
fn magic_link_rejection_applies_to_final_and_non_final_components() {
    let tree = test_tree();

    let err = Filename::new("/magic")
        .lookup_at(
            &tree.root,
            &tree.base,
            LookupIntent::Open,
            LookupFlags::follow() | LookupFlags::NO_MAGIC_LINKS,
            &kcred::initial_cred(),
        )
        .unwrap_err();
    assert_eq!(err, VfsError::FilesystemLoop);

    let err = Filename::new("/magicdir/leaf")
        .lookup_at(
            &tree.root,
            &tree.base,
            LookupIntent::Open,
            LookupFlags::no_follow() | LookupFlags::NO_MAGIC_LINKS,
            &kcred::initial_cred(),
        )
        .unwrap_err();
    assert_eq!(err, VfsError::FilesystemLoop);
    assert_eq!(tree.magic_dir.follow_count(), 0);
}

#[def_test]
fn non_final_magic_link_is_followed_by_namei() {
    let tree = test_tree();

    let leaf = Filename::new("/magicdir/leaf")
        .lookup_at(
            &tree.root,
            &tree.base,
            LookupIntent::Stat,
            LookupFlags::no_follow(),
            &kcred::initial_cred(),
        )
        .unwrap();
    assert_eq!(leaf.name(), "leaf");
    assert_eq!(tree.magic_dir.last_intent(), Some(LookupIntent::Stat));
}

#[def_test]
fn pathname_walk_requires_search_permission_on_intermediate_directory() {
    let tree = test_tree();
    tree.root
        .dentry()
        .update_metadata(MetadataUpdate {
            mode: Some(NodePermission::from_bits_truncate(0o711)),
            ..Default::default()
        })
        .unwrap();
    let dir = Filename::new("/dir")
        .lookup_at(
            &tree.root,
            &tree.base,
            LookupIntent::Open,
            LookupFlags::follow(),
            &kcred::initial_cred(),
        )
        .unwrap();
    dir.dentry()
        .update_metadata(MetadataUpdate {
            mode: Some(NodePermission::from_bits_truncate(0o600)),
            owner: Some((1000, 1000)),
            ..Default::default()
        })
        .unwrap();

    let cred = kcred::Cred::new(1000, 1000);
    let result = Filename::new("/dir/leaf").lookup_at(
        &tree.root,
        &tree.base,
        LookupIntent::Open,
        LookupFlags::follow(),
        &cred,
    );

    assert_eq!(result.unwrap_err(), VfsError::PermissionDenied);
}

#[def_test]
fn open_checks_final_inode_and_captures_opening_credential() {
    let tree = test_tree();
    tree.root
        .dentry()
        .update_metadata(MetadataUpdate {
            mode: Some(NodePermission::from_bits_truncate(0o711)),
            ..Default::default()
        })
        .unwrap();
    tree.target
        .dentry()
        .update_metadata(MetadataUpdate {
            mode: Some(NodePermission::from_bits_truncate(0o600)),
            owner: Some((1000, 1000)),
            ..Default::default()
        })
        .unwrap();

    let denied = Filename::new("/target").open_with_flags_at(
        &tree.root,
        &tree.base,
        linux_raw_sys::general::O_RDONLY,
        NodePermission::empty(),
        NodePermission::empty(),
        Arc::new(kcred::Cred::new(2000, 2000)),
    );
    assert!(matches!(denied, Err(VfsError::PermissionDenied)));

    let cred = Arc::new(kcred::Cred::new(1000, 1000));
    let file = Filename::new("/target")
        .open_with_flags_at(
            &tree.root,
            &tree.base,
            linux_raw_sys::general::O_RDONLY,
            NodePermission::empty(),
            NodePermission::empty(),
            cred.clone(),
        )
        .unwrap();
    assert!(Arc::ptr_eq(file.cred(), &cred));
}

#[def_test]
fn o_path_directory_rejects_non_directory() {
    let tree = test_tree();
    let flags = linux_raw_sys::general::O_PATH | linux_raw_sys::general::O_DIRECTORY;

    let regular = Filename::new("/target").open_with_flags_at(
        &tree.root,
        &tree.base,
        flags,
        NodePermission::empty(),
        NodePermission::empty(),
        kcred::initial_cred(),
    );
    assert!(matches!(regular, Err(VfsError::NotADirectory)));

    let directory = Filename::new("/dir")
        .open_with_flags_at(
            &tree.root,
            &tree.base,
            flags,
            NodePermission::empty(),
            NodePermission::empty(),
            kcred::initial_cred(),
        )
        .unwrap();
    assert_eq!(directory.node_type(), NodeType::Directory);

    let autodir = Filename::new("/autodir").open_with_flags_at(
        &tree.root,
        &tree.base,
        flags,
        NodePermission::empty(),
        NodePermission::empty(),
        kcred::initial_cred(),
    );
    assert!(matches!(autodir, Err(VfsError::NotADirectory)));
}

#[def_test]
fn o_path_trailing_slash_follows_symlink_and_requires_directory() {
    let tree = test_tree();

    let regular = Filename::new("/target/").open_with_flags_at(
        &tree.root,
        &tree.base,
        linux_raw_sys::general::O_PATH,
        NodePermission::empty(),
        NodePermission::empty(),
        kcred::initial_cred(),
    );
    assert!(matches!(regular, Err(VfsError::NotADirectory)));

    let flags = linux_raw_sys::general::O_PATH | linux_raw_sys::general::O_NOFOLLOW;
    let directory = Filename::new("/dirlink/")
        .open_with_flags_at(
            &tree.root,
            &tree.base,
            flags,
            NodePermission::empty(),
            NodePermission::empty(),
            kcred::initial_cred(),
        )
        .unwrap();
    assert_eq!(directory.node_type(), NodeType::Directory);

    let regular_link = Filename::new("/link/").open_with_flags_at(
        &tree.root,
        &tree.base,
        flags,
        NodePermission::empty(),
        NodePermission::empty(),
        kcred::initial_cred(),
    );
    assert!(matches!(regular_link, Err(VfsError::NotADirectory)));
}

#[def_test]
fn writable_open_lifetime_updates_inode_write_count() {
    let tree = test_tree();
    let inode = tree.target.inode().clone();
    let file_ops = inode.downcast::<TestFile>().unwrap();

    let first = dentry_open(
        tree.target.clone(),
        OpenFlags::WRITE_ONLY.bits(),
        kcred::initial_cred(),
    )
    .unwrap();
    assert_eq!(inode.write_count(), 1);
    let second = dentry_open(
        tree.target.clone(),
        OpenFlags::WRITE_ONLY.bits(),
        kcred::initial_cred(),
    )
    .unwrap();
    assert_eq!(inode.write_count(), 2);

    drop(first);
    assert_eq!(inode.write_count(), 1);
    {
        let release_write_counts = file_ops.release_write_counts.lock();
        assert_eq!(release_write_counts.as_slice(), &[2]);
    }
    drop(second);
    assert_eq!(inode.write_count(), 0);
    let release_write_counts = file_ops.release_write_counts.lock();
    assert_eq!(release_write_counts.as_slice(), &[2, 1]);
}

#[def_test]
fn trailing_dot_over_regular_file_is_not_a_directory() {
    let tree = test_tree();

    // `target` is a regular file. Linux rejects `target/.` with ENOTDIR because
    // the walk requires the component a `.` follows to be a directory
    // (`d_can_lookup` in fs/namei.c `link_path_walk`); dropping the component
    // made this path resolve as plain `target` and succeed.
    for intent in [LookupIntent::Open, LookupIntent::Stat] {
        let err = Filename::new("/target/.")
            .lookup_at(
                &tree.root,
                &tree.base,
                intent,
                LookupFlags::follow(),
                &kcred::initial_cred(),
            )
            .unwrap_err();
        assert_eq!(err, VfsError::NotADirectory);
    }

    // An open that would create or truncate through the file is rejected too.
    for flags in [
        linux_raw_sys::general::O_RDONLY,
        linux_raw_sys::general::O_WRONLY,
        linux_raw_sys::general::O_CREAT | linux_raw_sys::general::O_WRONLY,
    ] {
        let err = Filename::new("/target/.")
            .open_with_flags_at(
                &tree.root,
                &tree.base,
                flags,
                NodePermission::default(),
                NodePermission::empty(),
                kcred::initial_cred(),
            )
            .map(|_| ())
            .unwrap_err();
        assert_eq!(err, VfsError::NotADirectory);
    }

    // A directory still accepts the same shape.
    Filename::new("/dir/.")
        .lookup_at(
            &tree.root,
            &tree.base,
            LookupIntent::Open,
            LookupFlags::follow(),
            &kcred::initial_cred(),
        )
        .unwrap();
}

#[def_test]
fn component_under_regular_file_is_not_a_directory() {
    let tree = test_tree();

    // Linux runs `d_can_lookup` (ENOTDIR) before `may_lookup` (EACCES), so a
    // last component under a regular file reports ENOTDIR for every mode.
    let err = Filename::new("/target/leaf")
        .lookup_at(
            &tree.root,
            &tree.base,
            LookupIntent::Open,
            LookupFlags::follow(),
            &kcred::initial_cred(),
        )
        .unwrap_err();
    assert_eq!(err, VfsError::NotADirectory);
}

#[def_test]
fn trailing_slash_survives_final_symlink_expansion() {
    let tree = test_tree();

    // `link` points at the regular file `target`: the trailing slash still
    // demands a directory, and the demand must not be lost when the final
    // symlink is expanded (Linux keeps it in `nd->flags`).
    let err = Filename::new("/link/")
        .open_with_flags_at(
            &tree.root,
            &tree.base,
            linux_raw_sys::general::O_RDONLY,
            NodePermission::default(),
            NodePermission::empty(),
            kcred::initial_cred(),
        )
        .map(|_| ())
        .unwrap_err();
    assert_eq!(err, VfsError::NotADirectory);

    // A symlink to a directory keeps accepting the trailing slash.
    Filename::new("/dirlink/")
        .open_with_flags_at(
            &tree.root,
            &tree.base,
            linux_raw_sys::general::O_RDONLY,
            NodePermission::default(),
            NodePermission::empty(),
            kcred::initial_cred(),
        )
        .map(|_| ())
        .unwrap();
}

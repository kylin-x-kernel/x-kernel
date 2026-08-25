// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Mountpoints and location resolution for the VFS.
use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::{
    iter,
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
};

use hashbrown::HashMap;
use kcred::{Cred, NamespaceId, UserNamespace, initial_user_namespace};
use klazy::Once;
use ktime_types::TimeSpan;

/// Mount idmapping context passed into inode namespace operations.
#[derive(Debug)]
pub struct MountIdmap;

use crate::{
    Dentry, DentryKey, DeviceId, FMode, FileOperations, FsContext, GetattrQueryFlags,
    GetattrRequestMask, InodeUpdateTime, Metadata, MetadataUpdate, Mutex, NodeFlags,
    NodePermission, NodeType, OpenFlags, Permission, RenameFlags, SetattrTime, StatFs, SuperBlock,
    SuperBlockFlags, Umode, VfsError, VfsFile, VfsFileBuilder, VfsInode, VfsResult, nullfs,
    path::PathBuf,
};

bitflags::bitflags! {
    /// Per-mount flags, converted from the user-visible `MS_*` constants
    /// during the `mount(2)` syscall.
    ///
    /// See [`per_mount_flags`] in `posix/fs/src/mount.rs` for the mapping.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct MountFlags: u32 {
        /// Mount read-only.
        const RDONLY = 0x40;
        /// Ignore set-user-ID and set-group-ID bits on executables.
        const NOSUID = 0x01;
        /// Disallow access to device special files on this mount.
        const NODEV = 0x02;
        /// Disallow program execution from this mount.
        const NOEXEC = 0x04;
        /// Do not update file access times.
        const NOATIME = 0x08;
        /// Do not update directory access times.
        const NODIRATIME = 0x10;
        /// Update access time relative to mtime/ctime (the default).
        const RELATIME = 0x20;
        /// Do not follow symlinks on this mount.
        const NOSYMFOLLOW = 0x80;
    }
}

/// `struct vfsmount`.
///
/// Releasing the final object may synchronously run final superblock shutdown
/// and sleep. It must therefore happen in sleepable task context, without
/// holding a non-sleepable lock or a lock required by filesystem shutdown.
#[derive(Debug)]
pub struct VfsMount {
    mnt_root: Dentry,
    mnt_sb: Arc<SuperBlock>,
    mnt_flags: AtomicU32,
}

impl VfsMount {
    fn try_new(
        fs: &Arc<SuperBlock>,
        flags: MountFlags,
        requested_superblock_flags: Option<SuperBlockFlags>,
    ) -> VfsResult<Option<Self>> {
        Ok(fs
            .try_activate_mount(requested_superblock_flags)?
            .then(|| Self {
                mnt_root: fs.root_dir(),
                mnt_sb: fs.clone(),
                mnt_flags: AtomicU32::new(flags.bits()),
            }))
    }

    fn clone_from_path(source: &Path) -> Self {
        let super_block = source.super_block().clone();
        super_block.activate_mount();
        Self {
            mnt_root: source.dentry.clone(),
            mnt_sb: super_block,
            mnt_flags: AtomicU32::new(source.mount().flags().bits()),
        }
    }

    // Mount flags neither publish nor guard other state, so relaxed ordering
    // is sufficient for both snapshots and replacements.
    fn flags(&self) -> MountFlags {
        MountFlags::from_bits_truncate(self.mnt_flags.load(Ordering::Relaxed))
    }

    fn set_flags(&self, flags: MountFlags) {
        self.mnt_flags.store(flags.bits(), Ordering::Relaxed);
    }
}

impl Drop for VfsMount {
    fn drop(&mut self) {
        self.mnt_sb.deactivate_mount();
    }
}

static INIT_MNT_NS: Once<Arc<MntNamespace>> = Once::new();
static MOUNT_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

fn new_initial_nullfs_superblock() -> Arc<SuperBlock> {
    nullfs::new_superblock()
}

/// `struct mnt_namespace`.
#[derive(Debug)]
pub struct MntNamespace {
    id: NamespaceId,
    user_ns: Arc<UserNamespace>,
    root: Arc<Mount>,
    mounts: Mutex<HashMap<u64, Arc<Mount>>>,
}

impl MntNamespace {
    /// Initializes the global initial mount namespace mount tree.
    pub fn init_mount_tree(root_fs: &Arc<SuperBlock>) -> Arc<Self> {
        Arc::clone(INIT_MNT_NS.call_once(|| {
            Self::build_initial_mount_tree(root_fs)
                .expect("initial mount namespace tree must be created")
        }))
    }

    /// Returns the global initial mount namespace.
    pub fn initial() -> VfsResult<Arc<Self>> {
        INIT_MNT_NS.get().cloned().ok_or(VfsError::InvalidInput)
    }

    /// Creates a mount namespace rooted at the given filesystem.
    pub fn new_root(root_fs: &Arc<SuperBlock>, user_ns: Arc<UserNamespace>) -> Arc<Self> {
        let root = Mount::new_root(root_fs);
        let namespace = Arc::new(Self {
            id: NamespaceId::new(),
            user_ns,
            root: root.clone(),
            mounts: Mutex::default(),
        });
        namespace.mounts.lock().insert(root.mount_id(), root);
        namespace
    }

    fn build_initial_mount_tree(root_fs: &Arc<SuperBlock>) -> VfsResult<Arc<Self>> {
        let namespace_root = new_initial_nullfs_superblock();
        let namespace = Self::new_root(&namespace_root, initial_user_namespace());
        namespace.attach_with_flags_and_devname(
            &namespace.root_path(),
            root_fs,
            MountFlags::RELATIME,
            None,
        )?;
        Ok(namespace)
    }

    /// Creates a private copy of this mount namespace and retargets paths into it.
    ///
    /// The cloned namespace receives new [`Mount`] objects that point at the same
    /// mounted filesystem roots as the source tree. Dentries and superblocks are
    /// shared with the source filesystem instances, matching Linux's mount-tree
    /// copy semantics.
    pub fn clone_with_root_and_pwd(&self, root: &Path, pwd: &Path) -> VfsResult<NamespaceClone> {
        let _mounts_guard = self.mounts.lock();
        let mut mount_map = HashMap::new();
        let cloned_root = clone_mount_tree(&self.root, None, &mut mount_map);
        let namespace = Arc::new(Self {
            id: NamespaceId::new(),
            user_ns: self.user_ns.clone(),
            root: cloned_root,
            mounts: Mutex::default(),
        });
        {
            let mut mounts = namespace.mounts.lock();
            for mount in mount_map.values() {
                mounts.insert(mount.mount_id(), mount.clone());
            }
        }

        Ok(NamespaceClone {
            namespace,
            root: retarget_path(root, &mount_map)?,
            pwd: retarget_path(pwd, &mount_map)?,
        })
    }

    /// Returns this namespace's ID.
    pub fn id(&self) -> NamespaceId {
        self.id
    }

    /// Returns the user namespace that owns this mount namespace.
    pub fn user_ns(&self) -> &Arc<UserNamespace> {
        &self.user_ns
    }

    /// Returns this namespace's root mount.
    pub fn root_mount(&self) -> &Arc<Mount> {
        &self.root
    }

    /// Returns this namespace's root path.
    pub fn root_path(&self) -> Path {
        self.root.root_path()
    }

    /// Returns the path seen as `/` by tasks attached to this namespace.
    ///
    /// The initial namespace keeps a non-user-visible structural root mount and
    /// layers the mutable root filesystem over it, matching Linux
    /// `init_mount_tree()`. Process `fs_struct` root and pwd should use this
    /// visible path, not the namespace's structural root path.
    pub fn visible_root_path(&self) -> Path {
        self.root_path().resolve_final_mount()
    }

    /// Mounts a filesystem at a resolved mountpoint without per-mount flags.
    pub fn attach(&self, mountpoint: &Path, fs: &Arc<SuperBlock>) -> VfsResult<Arc<Mount>> {
        self.attach_with_flags_and_devname(mountpoint, fs, MountFlags::empty(), None)
    }

    /// Mounts a filesystem with per-mount flags and device name at a resolved mountpoint.
    pub fn attach_with_flags_and_devname(
        &self,
        mountpoint: &Path,
        fs: &Arc<SuperBlock>,
        flags: MountFlags,
        devname: Option<&str>,
    ) -> VfsResult<Arc<Mount>> {
        self.attach_with_requested_superblock_flags(mountpoint, fs, flags, devname, None)
    }

    fn attach_with_requested_superblock_flags(
        &self,
        mountpoint: &Path,
        fs: &Arc<SuperBlock>,
        flags: MountFlags,
        devname: Option<&str>,
        requested_superblock_flags: Option<SuperBlockFlags>,
    ) -> VfsResult<Arc<Mount>> {
        let mut mounts = self.mounts.lock();
        Self::require_mount(&mounts, mountpoint.mount())?;
        let mount = mountpoint.mount_filesystem_with_requested_superblock_flags(
            fs,
            flags,
            devname,
            requested_superblock_flags,
        )?;
        mounts.insert(mount.mount_id(), mount.clone());
        Ok(mount)
    }

    /// Creates and attaches a filesystem selected by a filesystem context.
    ///
    /// Filesystem construction completes before the namespace lock is
    /// acquired, so `get_tree` may perform blocking device I/O. If the
    /// selected superblock enters final shutdown before activation, lookup is
    /// repeated just as Linux retries after losing the `sget` race.
    pub fn mount_new(
        &self,
        mountpoint: &Path,
        mount_flags: MountFlags,
        context: &mut FsContext<'_>,
        lookup_root: &Path,
        lookup_pwd: &Path,
    ) -> VfsResult<Arc<Mount>> {
        loop {
            let super_block = context.get_tree(lookup_root, lookup_pwd)?;
            let result = self.attach_with_requested_superblock_flags(
                mountpoint,
                &super_block,
                mount_flags,
                context.source(),
                Some(context.sb_flags()),
            );
            if matches!(&result, Err(VfsError::ResourceBusy)) && !super_block.is_available() {
                continue;
            }
            return result;
        }
    }

    /// Creates a non-recursive bind mount of `source` at `mountpoint`.
    ///
    /// The new mount shares the source dentry and superblock and inherits the
    /// source mount's per-mount flags.
    pub fn attach_bind(&self, source: &Path, mountpoint: &Path) -> VfsResult<Arc<Mount>> {
        let mut mounts = self.mounts.lock();
        Self::require_mount(&mounts, source.mount())?;
        Self::require_mount(&mounts, mountpoint.mount())?;
        let mount = mountpoint.mount_bind(source)?;
        mounts.insert(mount.mount_id(), mount.clone());
        Ok(mount)
    }

    /// Remounts `target` with independent superblock and per-mount flags.
    ///
    /// Returns [`VfsError::InvalidInput`] unless `target` is the root path of
    /// a mount registered in this namespace.
    pub fn remount(
        &self,
        target: &Path,
        context: &mut FsContext<'_>,
        mount_flags: MountFlags,
    ) -> VfsResult<()> {
        let mounts = self.mounts.lock();
        let mount = Self::registered_mount_at(&mounts, target)?;
        mount.super_block().reconfigure(context)?;
        mount.set_flags(mount_flags);
        Ok(())
    }

    /// Replaces only the per-mount flags of the mount rooted at `target`.
    pub fn reconfigure_mount(&self, target: &Path, flags: MountFlags) -> VfsResult<()> {
        let mounts = self.mounts.lock();
        let mount = Self::registered_mount_at(&mounts, target)?;
        mount.set_flags(flags);
        Ok(())
    }

    fn registered_mount_at<'a>(
        mounts: &HashMap<u64, Arc<Mount>>,
        target: &'a Path,
    ) -> VfsResult<&'a Arc<Mount>> {
        if !target.is_mount_root() {
            return Err(VfsError::InvalidInput);
        }

        let mount = target.mount();
        Self::require_mount(mounts, mount)?;
        Ok(mount)
    }

    fn require_mount(mounts: &HashMap<u64, Arc<Mount>>, mount: &Arc<Mount>) -> VfsResult<()> {
        mounts
            .get(&mount.mount_id())
            .is_some_and(|registered| Arc::ptr_eq(registered, mount))
            .then_some(())
            .ok_or(VfsError::InvalidInput)
    }

    /// Unmounts one visible mount from this namespace.
    pub fn detach(&self, path: &Path) -> VfsResult<()> {
        let mut mounts = self.mounts.lock();
        let removed = Self::registered_mount_at(&mounts, path)?.clone();
        path.unmount()?;
        mounts.remove(&removed.mount_id());
        drop(mounts);
        Ok(())
    }

    /// Unmounts a visible mount tree from this namespace.
    pub fn detach_tree(&self, path: &Path) -> VfsResult<()> {
        let mut mounts = self.mounts.lock();
        let root = Self::registered_mount_at(&mounts, path)?.clone();
        let removed = root.collect_subtree();
        for mount in &removed {
            Self::require_mount(&mounts, mount)?;
        }

        path.detach_mount_subtree(&removed)?;
        for mount in &removed {
            mounts.remove(&mount.mount_id());
        }
        drop(mounts);
        Ok(())
    }
}

/// Result of cloning a mount namespace and retargeting filesystem context paths.
#[derive(Debug)]
pub struct NamespaceClone {
    /// The cloned mount namespace.
    pub namespace: Arc<MntNamespace>,
    /// The old filesystem root path retargeted into the cloned namespace.
    pub root: Path,
    /// The old current working directory path retargeted into the cloned namespace.
    pub pwd: Path,
}

/// `struct mount`.
#[derive(Debug)]
pub struct Mount {
    mnt: VfsMount,
    mnt_location: Mutex<Option<Path>>,
    mnt_mounts: Mutex<HashMap<DentryKey, Weak<Self>>>,
    mnt_id: u64,
    /// Mount that this one covers (for overmount).
    ///
    /// When a new mount is created at a location that already has a mount,
    /// the old mount is stored here so it can be restored on unmount.
    covers: Mutex<Option<Arc<Self>>>,
    mnt_devname: Option<String>,
}

impl Mount {
    /// Creates a root mount with per-mount flags and device name.
    pub fn new_root_with_flags_and_devname(
        fs: &Arc<SuperBlock>,
        flags: MountFlags,
        devname: Option<&str>,
    ) -> Arc<Self> {
        Arc::new(Self::new_detached(fs, flags, devname))
    }

    fn new_detached(fs: &Arc<SuperBlock>, flags: MountFlags, devname: Option<&str>) -> Self {
        Self::try_new_detached(fs, flags, devname, None)
            .expect("unchecked mount activation cannot reject flags")
            .expect("a dying or dead superblock cannot be mounted")
    }

    fn try_new_detached(
        fs: &Arc<SuperBlock>,
        flags: MountFlags,
        devname: Option<&str>,
        requested_superblock_flags: Option<SuperBlockFlags>,
    ) -> VfsResult<Option<Self>> {
        let Some(mnt) = VfsMount::try_new(fs, flags, requested_superblock_flags)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            mnt,
            mnt_location: Mutex::default(),
            mnt_mounts: Mutex::default(),
            mnt_id: MOUNT_ID_COUNTER.fetch_add(1, Ordering::Relaxed),
            covers: Mutex::default(),
            mnt_devname: devname.map(|s| s.to_string()),
        }))
    }

    fn clone_mnt(source: &Path) -> Self {
        Self {
            mnt: VfsMount::clone_from_path(source),
            mnt_location: Mutex::default(),
            mnt_mounts: Mutex::default(),
            mnt_id: MOUNT_ID_COUNTER.fetch_add(1, Ordering::Relaxed),
            covers: Mutex::default(),
            mnt_devname: source.mount().mnt_devname.clone(),
        }
    }

    fn into_arc_at(mut self, location_in_parent: Option<Path>) -> Arc<Self> {
        self.mnt_location = Mutex::new(location_in_parent);
        Arc::new(self)
    }

    /// Creates a root mount for a filesystem.
    pub fn new_root(fs: &Arc<SuperBlock>) -> Arc<Self> {
        Self::new_root_with_flags_and_devname(fs, MountFlags::empty(), None)
    }

    /// Creates a root mount with per-mount flags.
    pub fn new_root_with_flags(fs: &Arc<SuperBlock>, flags: MountFlags) -> Arc<Self> {
        Self::new_root_with_flags_and_devname(fs, flags, None)
    }

    /// Return a `Path` representing the mount root.
    pub fn root_path(self: &Arc<Self>) -> Path {
        Path::new(self.clone(), self.mnt.mnt_root.clone())
    }

    /// Returns the superblock mounted at this mount object.
    pub fn super_block(&self) -> &Arc<SuperBlock> {
        &self.mnt.mnt_sb
    }

    /// Returns the VFS-wide flags for this mount's superblock.
    pub fn super_block_flags(&self) -> SuperBlockFlags {
        self.super_block().flags()
    }

    /// Returns the filesystem type name for this mount.
    pub fn filesystem_name(&self) -> &str {
        self.super_block().name()
    }

    /// Returns filesystem statistics for this mount.
    pub fn filesystem_stat(&self) -> VfsResult<StatFs> {
        self.super_block().stat()
    }

    /// Allocates an opened pseudo file on this mount.
    pub fn alloc_file_pseudo(
        self: &Arc<Self>,
        inode: Arc<VfsInode>,
        name: &str,
        flags: FMode,
        open_flags: OpenFlags,
        f_op: Arc<dyn FileOperations>,
        cred: Arc<Cred>,
    ) -> VfsResult<Arc<VfsFile>> {
        let dentry = Dentry::new_file_from_inode(inode, None, name.to_string());
        dentry.bind_super_block(self.super_block());
        let path = Path::new(self.clone(), dentry);
        let mut file = VfsFileBuilder::from_path_state(path, flags, open_flags, f_op, cred);
        file.mark_opened()?;
        file.finish()
    }

    /// Returns the path in the parent mount.
    pub fn location(&self) -> Option<Path> {
        self.mnt_location.lock().clone()
    }

    /// Returns whether this mount has no parent mount.
    pub fn is_root(&self) -> bool {
        self.mnt_location.lock().is_none()
    }

    /// Returns the mount namespace identity assigned to this mount object.
    pub fn mount_id(self: &Arc<Self>) -> u64 {
        self.mnt_id
    }

    /// Returns the device name for this mount, if set.
    pub fn devname(&self) -> Option<&str> {
        self.mnt_devname.as_deref()
    }

    /// Returns the temporary VFS device identity exposed for inode metadata.
    ///
    /// X-Kernel does not yet have a superblock-level `s_dev` model, so mounted
    /// filesystems use the mount ID as a stable synthetic device number.
    pub fn synthetic_device_id(self: &Arc<Self>) -> u64 {
        self.mnt_id
    }

    /// Returns this mount's flags.
    pub fn flags(&self) -> MountFlags {
        self.mnt.flags()
    }

    fn set_flags(&self, flags: MountFlags) {
        self.mnt.set_flags(flags);
    }

    /// Returns whether this mountpoint is mounted read-only.
    pub fn is_readonly(&self) -> bool {
        self.flags().contains(MountFlags::RDONLY)
    }

    /// Returns a snapshot of direct child mounts currently attached here.
    pub fn children(self: &Arc<Self>) -> Vec<Arc<Self>> {
        self.mnt_mounts
            .lock()
            .values()
            .filter_map(Weak::upgrade)
            .collect()
    }

    fn covered_mount(&self) -> Option<Arc<Self>> {
        self.covers.lock().clone()
    }

    fn collect_subtree(self: &Arc<Self>) -> Vec<Arc<Self>> {
        let mut mounts = vec![self.clone()];
        let mut index = 0;
        while index < mounts.len() {
            let mount = mounts[index].clone();
            for child in mount.children() {
                let mut layer = Some(child);
                while let Some(mount) = layer {
                    layer = mount.covered_mount();
                    mounts.push(mount);
                }
            }
            index += 1;
        }
        mounts
    }

    fn child_mount_at(&self, dentry: &Dentry) -> Option<Arc<Self>> {
        let key = dentry.key();
        let mut mounts = self.mnt_mounts.lock();
        let mount = mounts.get(&key).and_then(Weak::upgrade);
        if mount.is_none() {
            mounts.remove(&key);
        }
        mount
    }

    fn install_child_mount(&self, mountpoint: &Dentry, mount: &Arc<Self>) -> Option<Arc<Self>> {
        self.mnt_mounts
            .lock()
            .insert(mountpoint.key(), Arc::downgrade(mount))
            .and_then(|old| old.upgrade())
    }

    fn remove_child_mount(&self, mountpoint: &Dentry) {
        self.mnt_mounts.lock().remove(&mountpoint.key());
    }

    fn detach(&self) {
        self.mnt_location.lock().take();
    }
}

/// A resolved location within a mountpoint.
#[derive(Debug, Clone)]
pub struct Path {
    mnt: Arc<Mount>,
    dentry: Dentry,
}

impl Path {
    /// Create a `struct path` from a mount and dentry.
    pub fn new(mnt: Arc<Mount>, dentry: Dentry) -> Self {
        Self { mnt, dentry }
    }

    pub(crate) fn with_dentry(&self, dentry: Dentry) -> Self {
        Self::new(self.mnt.clone(), dentry)
    }

    /// Returns the mount containing this path.
    pub fn mount(&self) -> &Arc<Mount> {
        &self.mnt
    }

    fn is_mount_root(&self) -> bool {
        if !self.dentry.ptr_eq(&self.mnt.mnt.mnt_root) {
            return false;
        }

        self.mnt.location().is_none_or(|parent_path| {
            parent_path
                .mnt
                .child_mount_at(&parent_path.dentry)
                .is_some_and(|mount| Arc::ptr_eq(&mount, &self.mnt))
        })
    }

    /// Returns the superblock containing this location.
    pub(crate) fn super_block(&self) -> &Arc<SuperBlock> {
        self.mnt.super_block()
    }

    /// Returns filesystem statistics for this path.
    pub fn filesystem_stat(&self) -> VfsResult<StatFs> {
        self.super_block().stat()
    }

    /// Synchronizes the filesystem containing this path.
    pub fn sync_filesystem(&self) -> VfsResult<()> {
        self.super_block().sync_fs()
    }

    /// Returns the maximum regular-file size allowed on this path.
    pub fn max_file_size(&self) -> u64 {
        self.super_block().max_file_size()
    }

    /// Returns this path's inode identity.
    pub fn inode(&self) -> Arc<VfsInode> {
        self.dentry.vfs_inode()
    }

    /// Returns a stable key for this path's inode identity.
    pub fn inode_key(&self) -> usize {
        Arc::as_ptr(&self.inode()) as usize
    }

    /// Returns a weak reference to this path's inode identity.
    pub fn weak_inode(&self) -> Weak<VfsInode> {
        Arc::downgrade(&self.inode())
    }

    /// Returns this path's node type.
    pub fn node_type(&self) -> NodeType {
        self.inode().node_type()
    }

    /// Returns whether this path names a directory.
    pub fn is_dir(&self) -> bool {
        self.node_type() == NodeType::Directory
    }

    /// Returns whether this path names a regular file.
    pub fn is_regular_file(&self) -> bool {
        self.node_type() == NodeType::RegularFile
    }

    /// Returns whether this path names a non-directory file.
    pub fn is_file(&self) -> bool {
        !self.is_dir()
    }

    /// Returns whether this path names a symbolic link.
    pub fn is_symlink(&self) -> bool {
        self.node_type() == NodeType::Symlink
    }

    /// Attempt to downcast this path's inode-private state.
    pub fn downcast_node<T: core::any::Any + Send + Sync>(&self) -> VfsResult<Arc<T>> {
        self.inode().downcast()
    }

    /// Returns the underlying directory entry.
    pub(crate) fn dentry(&self) -> &Dentry {
        &self.dentry
    }

    /// Returns a cached child inode or inserts a filesystem-created child.
    pub fn cached_child_inode_or_insert_with(
        &self,
        name: &str,
        create: impl FnOnce(&Dentry, String) -> Dentry,
    ) -> u64 {
        if let Some(entry) = self
            .dentry
            .lookup_cache(name)
            .filter(Dentry::is_really_positive)
        {
            return entry.inode();
        }

        let name = name.to_string();
        let entry = create(&self.dentry, name.clone());
        self.dentry
            .insert_cache(name, entry.clone())
            .filter(Dentry::is_really_positive)
            .unwrap_or(entry)
            .inode()
    }

    /// Returns metadata for the inode referenced by this location.
    pub fn metadata(&self) -> Metadata {
        self.dentry.vfs_inode().metadata()
    }

    /// Checks access to this location's inode.
    pub fn permission(&self, permission: Permission, cred: &Cred) -> VfsResult<()> {
        self.inode().permission(permission, cred)
    }

    /// Returns a snapshot of this location's name within its parent directory.
    pub fn name(&self) -> String {
        self.dentry.name_snapshot()
    }

    /// Returns the parent location, if any.
    pub fn parent(&self) -> Option<Self> {
        if !self.dentry.ptr_eq(&self.mnt.mnt.mnt_root) {
            return Some(self.with_dentry(self.dentry.parent()?));
        }
        self.mnt.location()?.parent()
    }

    /// Returns `true` if this is the root of a mount tree.
    pub fn is_root(&self) -> bool {
        self.mnt.is_root() && self.dentry.is_root_of_mount()
    }

    /// Build the absolute path for this location.
    pub fn absolute_path(&self) -> VfsResult<PathBuf> {
        if !self.dentry.ptr_eq(&self.mnt.mnt.mnt_root)
            && let Some(dynamic_name) = self.dentry.dynamic_name()?
        {
            return Ok(PathBuf::from(dynamic_name));
        }

        let mut components = vec![];
        let mut cur = self.clone();
        loop {
            cur.dentry.collect_absolute_path(&mut components);
            cur = match cur.mnt.location() {
                Some(loc) => loc,
                None => break,
            }
        }
        Ok(iter::once("/")
            .chain(components.iter().map(String::as_str).rev())
            .collect())
    }

    /// Returns a display pathname for this location.
    pub fn display_path(&self) -> VfsResult<String> {
        Ok(self.absolute_path()?.to_string())
    }

    /// Returns `true` if this location references the same entry.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.mnt, &other.mnt) && self.dentry.ptr_eq(&other.dentry)
    }

    /// Returns `true` if both locations point at the same VFS inode identity.
    pub fn is_same_inode(&self, other: &Self) -> bool {
        self.dentry.is_same_inode(&other.dentry)
    }

    /// Returns whether this path's mount is read-only.
    pub fn is_mount_readonly(&self) -> bool {
        self.mnt.is_readonly()
    }

    /// Returns whether this path is effectively read-only.
    pub fn is_effectively_readonly(&self) -> bool {
        self.is_mount_readonly() || self.super_block().is_readonly()
    }

    /// Ensures this path's mount allows modification.
    pub fn check_writable_mount(&self) -> VfsResult<()> {
        if self.is_effectively_readonly() {
            return Err(VfsError::ReadOnlyFilesystem);
        }
        Ok(())
    }

    pub(crate) fn touch_atime(&self) {
        if self.is_effectively_readonly() {
            return;
        }

        let mount_flags = self.mnt.flags();
        let super_block_flags = self.super_block().flags();
        if mount_flags.contains(MountFlags::NOATIME)
            || super_block_flags.contains(SuperBlockFlags::NOATIME)
            || (self.inode().node_type() == NodeType::Directory
                && (mount_flags.contains(MountFlags::NODIRATIME)
                    || super_block_flags.contains(SuperBlockFlags::NODIRATIME)))
        {
            return;
        }

        let metadata = self.metadata();
        let now = self.inode().current_time();
        if mount_flags.contains(MountFlags::RELATIME)
            && metadata.mtime < metadata.atime
            && metadata.ctime < metadata.atime
            && now.duration_since(metadata.atime).unwrap_or(TimeSpan::ZERO)
                < TimeSpan::from_secs(24 * 60 * 60)
        {
            return;
        }
        if metadata.atime == now {
            return;
        }

        // Linux treats atime persistence as best effort so a read that already
        // completed is never converted into an update-time failure.
        let _ = self
            .inode()
            .update_time(self.dentry(), now, InodeUpdateTime::Access);
    }

    pub(crate) fn update_cmtime(&self) -> VfsResult<()> {
        if self.is_effectively_readonly() {
            return Ok(());
        }

        let metadata = self.metadata();
        let now = self.inode().current_time();
        if metadata.mtime == now && metadata.ctime == now {
            return Ok(());
        }
        self.inode()
            .update_time(self.dentry(), now, InodeUpdateTime::ChangeAndModification)
    }

    /// Changes this inode's owner after applying Linux chown/chgrp authorization.
    pub fn chown(&self, uid: Option<u32>, gid: Option<u32>, cred: &Cred) -> VfsResult<()> {
        if uid.is_none() && gid.is_none() {
            return Ok(());
        }

        self.check_writable_mount()?;
        let metadata = self.metadata();
        let is_owner = cred.fsuid() == metadata.uid;
        let is_privileged = cred.is_privileged();

        if uid.is_some_and(|uid| !is_privileged && (!is_owner || uid != metadata.uid)) {
            return Err(VfsError::OperationNotPermitted);
        }
        if gid.is_some_and(|gid| {
            !is_privileged && (!is_owner || (gid != metadata.gid && !cred.in_group(gid)))
        }) {
            return Err(VfsError::OperationNotPermitted);
        }

        let mut mode = metadata.mode.permission();
        mode.remove(NodePermission::SET_UID);
        if mode.contains(NodePermission::GROUP_EXEC) {
            mode.remove(NodePermission::SET_GID);
        }
        self.dentry.update_metadata(MetadataUpdate {
            owner: Some((uid.unwrap_or(metadata.uid), gid.unwrap_or(metadata.gid))),
            mode: Some(mode),
            ..Default::default()
        })
    }

    /// Changes this inode's permission bits after applying Linux chmod authorization.
    pub fn chmod(&self, mut mode: NodePermission, cred: &Cred) -> VfsResult<()> {
        self.check_writable_mount()?;
        let metadata = self.metadata();
        if cred.fsuid() != metadata.uid && !cred.is_privileged() {
            return Err(VfsError::OperationNotPermitted);
        }
        if mode.contains(NodePermission::SET_GID)
            && !cred.in_group(metadata.gid)
            && !cred.is_privileged()
        {
            mode.remove(NodePermission::SET_GID);
        }
        self.dentry.update_metadata(MetadataUpdate {
            mode: Some(mode),
            ..Default::default()
        })
    }

    /// Changes this inode's timestamps after applying Linux utimens authorization.
    pub fn set_times(
        &self,
        atime: Option<SetattrTime>,
        mtime: Option<SetattrTime>,
        cred: &Cred,
    ) -> VfsResult<()> {
        if atime.is_none() && mtime.is_none() {
            return Ok(());
        }

        self.check_writable_mount()?;
        let metadata = self.metadata();
        let is_owner = cred.fsuid() == metadata.uid;
        let is_touch = matches!(
            (atime, mtime),
            (Some(SetattrTime::Current(_)), Some(SetattrTime::Current(_)))
        );
        if !is_touch {
            if !is_owner && !cred.is_privileged() {
                return Err(VfsError::OperationNotPermitted);
            }
        } else if !is_owner && !cred.is_privileged() {
            self.permission(Permission::MAY_WRITE, cred)?;
        }

        self.dentry.update_metadata(MetadataUpdate {
            atime: atime.map(SetattrTime::value),
            mtime: mtime.map(SetattrTime::value),
            ..Default::default()
        })
    }

    /// Returns metadata without security checks.
    pub fn getattr_nosec(
        &self,
        request_mask: GetattrRequestMask,
        query_flags: GetattrQueryFlags,
    ) -> VfsResult<Metadata> {
        self.inode().getattr(self, request_mask, query_flags)
    }

    /// Returns VFS metadata for this path.
    pub fn getattr(&self) -> VfsResult<Metadata> {
        let mut metadata =
            self.getattr_nosec(GetattrRequestMask::empty(), GetattrQueryFlags::empty())?;
        metadata.device = self.mnt.synthetic_device_id();
        Ok(metadata)
    }

    /// Returns `true` if this path is a mountpoint directory.
    pub fn is_mountpoint(&self) -> bool {
        self.dentry.as_dir().is_ok() && self.mnt.child_mount_at(&self.dentry).is_some()
    }

    fn check_not_mountpoint(&self) -> VfsResult<()> {
        if self.is_mountpoint() {
            return Err(VfsError::ResourceBusy);
        }
        Ok(())
    }

    fn may_modify_directory(&self, cred: &Cred) -> VfsResult<()> {
        self.permission(Permission::MAY_WRITE | Permission::MAY_EXEC, cred)
    }

    pub(crate) fn vfs_create(
        &self,
        candidate: &Dentry,
        mode: Umode,
        umask: NodePermission,
        exclusive: bool,
        cred: &Cred,
    ) -> VfsResult<()> {
        self.check_writable_mount()?;
        self.may_modify_directory(cred)?;
        let dir_inode = self.inode();
        let mode = dir_inode.prepare_create_mode(
            mode,
            umask,
            NodePermission::all(),
            NodeType::RegularFile,
            cred,
        );
        dir_inode.create_with_mode(candidate, mode, exclusive, cred)
    }

    pub(crate) fn vfs_mknod(
        &self,
        candidate: &Dentry,
        mode: Umode,
        device: DeviceId,
        umask: NodePermission,
        cred: &Cred,
    ) -> VfsResult<()> {
        self.check_writable_mount()?;
        self.may_modify_directory(cred)?;
        let node_type = mode.node_type();
        if matches!(node_type, NodeType::CharacterDevice | NodeType::BlockDevice)
            && !cred.is_privileged()
        {
            return Err(VfsError::OperationNotPermitted);
        }
        let dir_inode = self.inode();
        let mode = dir_inode.prepare_create_mode(mode, umask, mode.permission(), node_type, cred);
        dir_inode.mknod_with_mode(candidate, mode, device, cred)
    }

    pub(crate) fn vfs_mkdir(
        &self,
        candidate: &Dentry,
        permission: NodePermission,
        umask: NodePermission,
        cred: &Cred,
    ) -> VfsResult<()> {
        self.check_writable_mount()?;
        self.may_modify_directory(cred)?;
        let dir_inode = self.inode();
        let allowed_permission =
            NodePermission::all() & !(NodePermission::SET_UID | NodePermission::SET_GID);
        let mode = dir_inode.prepare_create_mode(
            Umode::new(NodeType::Directory, permission),
            umask,
            allowed_permission,
            NodeType::Directory,
            cred,
        );
        dir_inode.mkdir(candidate, mode, cred)
    }

    pub(crate) fn vfs_symlink(
        &self,
        candidate: &Dentry,
        target: &str,
        cred: &Cred,
    ) -> VfsResult<()> {
        self.check_writable_mount()?;
        self.may_modify_directory(cred)?;
        let dir_inode = self.inode();
        dir_inode.symlink(candidate, target, cred)
    }

    pub(crate) fn vfs_link(&self, candidate: &Dentry, source: &Self, cred: &Cred) -> VfsResult<()> {
        if !Arc::ptr_eq(&self.mnt, &source.mnt) {
            return Err(VfsError::CrossesDevices);
        }
        self.check_writable_mount()?;
        self.may_modify_directory(cred)?;
        if source.is_dir() {
            return Err(VfsError::OperationNotPermitted);
        }
        let source_inode = source.inode();
        let _source_guard = source_inode.lock_namespace_exclusive();
        self.inode().link(candidate, &source.dentry)
    }

    fn check_sticky(&self, victim: &Self, cred: &Cred) -> VfsResult<()> {
        let dir = self.metadata();
        if !dir.mode.permission().contains(NodePermission::STICKY) {
            return Ok(());
        }

        let fsuid = cred.fsuid();
        if fsuid == 0 || fsuid == dir.uid || fsuid == victim.metadata().uid {
            Ok(())
        } else {
            Err(VfsError::OperationNotPermitted)
        }
    }

    /// Creates a regular file under this directory path.
    ///
    /// The final lookup and authorization are serialized by the parent
    /// directory namespace lock, and the filesystem receives an exclusive
    /// create request.
    pub fn create(&self, name: &str, permission: NodePermission, cred: &Cred) -> VfsResult<Self> {
        let mode = Umode::new(NodeType::RegularFile, permission);
        self.dentry
            .as_dir()?
            .create_exclusive_with(name, |candidate| {
                self.vfs_create(candidate, mode, NodePermission::empty(), true, cred)
            })
            .map(|entry| self.with_dentry(entry))
    }

    /// Creates a directory under this directory path.
    pub fn mkdir(&self, name: &str, permission: NodePermission, cred: &Cred) -> VfsResult<Self> {
        self.dentry
            .as_dir()?
            .create_exclusive_with(name, |candidate| {
                self.vfs_mkdir(candidate, permission, NodePermission::empty(), cred)
            })
            .map(|entry| self.with_dentry(entry))
    }

    /// Creates a non-regular filesystem node under this directory path.
    ///
    /// Character and block device creation requires a privileged credential.
    /// The existence check takes precedence over create-only authorization
    /// failures.
    pub fn mknod(
        &self,
        name: &str,
        node_type: NodeType,
        permission: NodePermission,
        device: DeviceId,
        cred: &Cred,
    ) -> VfsResult<Self> {
        let mode = Umode::new(node_type, permission);
        self.dentry
            .as_dir()?
            .create_exclusive_with(name, |candidate| {
                self.vfs_mknod(candidate, mode, device, NodePermission::empty(), cred)
            })
            .map(|entry| self.with_dentry(entry))
    }

    /// Creates a hard link to an existing path.
    pub fn link(&self, name: &str, source: &Self, cred: &Cred) -> VfsResult<Self> {
        self.dentry
            .as_dir()?
            .create_exclusive_with(name, |candidate| self.vfs_link(candidate, source, cred))
            .map(|entry| self.with_dentry(entry))
    }

    /// Creates a symbolic link under this directory path.
    pub fn symlink(&self, name: &str, target: &str, cred: &Cred) -> VfsResult<Self> {
        self.dentry
            .as_dir()?
            .create_exclusive_with(name, |candidate| self.vfs_symlink(candidate, target, cred))
            .map(|entry| self.with_dentry(entry))
    }

    /// Rename an entry within the same mountpoint.
    pub fn rename(
        &self,
        old_name: &str,
        new_dir: &Self,
        new_name: &str,
        flags: RenameFlags,
        cred: &Cred,
    ) -> VfsResult<()> {
        if !Arc::ptr_eq(&self.mnt, &new_dir.mnt) {
            return Err(VfsError::CrossesDevices);
        }
        self.check_writable_mount()?;

        self.dentry.as_dir()?.rename_with(
            old_name,
            new_dir.dentry.as_dir()?,
            new_name,
            flags,
            |source, target| {
                self.may_modify_directory(cred)?;
                if !self.ptr_eq(new_dir) {
                    new_dir.may_modify_directory(cred)?;
                }

                let old_path = self.with_dentry(source.clone());
                self.check_sticky(&old_path, cred)?;
                old_path.check_not_mountpoint()?;
                if target.is_really_positive() {
                    let new_path = new_dir.with_dentry(target.clone());
                    new_dir.check_sticky(&new_path, cred)?;
                    new_path.check_not_mountpoint()?;
                }
                Ok(())
            },
        )
    }

    /// Remove a non-directory entry.
    pub fn unlink(&self, name: &str, cred: &Cred) -> VfsResult<()> {
        self.unlink_with_pathname(name, false, cred)
    }

    pub(crate) fn unlink_with_pathname(
        &self,
        name: &str,
        has_trailing_slash: bool,
        cred: &Cred,
    ) -> VfsResult<()> {
        self.check_writable_mount()?;
        self.dentry.as_dir()?.unlink_with(name, |victim| {
            if has_trailing_slash {
                return Err(VfsError::NotADirectory);
            }
            self.may_modify_directory(cred)?;
            let victim_path = self.with_dentry(victim.clone());
            self.check_sticky(&victim_path, cred)?;
            victim_path.check_not_mountpoint()
        })
    }

    /// Remove a directory entry.
    pub fn rmdir(&self, name: &str, cred: &Cred) -> VfsResult<()> {
        self.check_writable_mount()?;
        self.dentry.as_dir()?.rmdir_with(name, |victim| {
            self.may_modify_directory(cred)?;
            let victim_path = self.with_dentry(victim.clone());
            self.check_sticky(&victim_path, cred)?;
            victim_path.check_not_mountpoint()
        })
    }

    /// Mount a filesystem at this path.
    fn mount_filesystem_with_requested_superblock_flags(
        &self,
        fs: &Arc<SuperBlock>,
        flags: MountFlags,
        devname: Option<&str>,
        requested_superblock_flags: Option<SuperBlockFlags>,
    ) -> VfsResult<Arc<Mount>> {
        let mount = Mount::try_new_detached(fs, flags, devname, requested_superblock_flags)?
            .ok_or(VfsError::ResourceBusy)?;
        self.graft_tree(mount)
    }

    fn mount_bind(&self, source: &Path) -> VfsResult<Arc<Mount>> {
        self.graft_tree(Mount::clone_mnt(source))
    }

    fn graft_tree(&self, mount: Mount) -> VfsResult<Arc<Mount>> {
        let inode = self.inode();
        let _namespace_guard = inode.lock_namespace_exclusive();
        if mount.mnt.mnt_root.as_dir().is_ok() != self.is_dir() {
            // Linux `graft_tree()` returns `ENOTDIR` for either direction of
            // a directory/non-directory mismatch.
            return Err(VfsError::NotADirectory);
        }
        let result = mount.into_arc_at(Some(self.clone()));
        if let Some(old) = self.mnt.install_child_mount(&self.dentry, &result) {
            *result.covers.lock() = Some(old);
        }
        Ok(result)
    }

    /// Unmount the filesystem rooted at this path.
    fn unmount(&self) -> VfsResult<()> {
        if !self.dentry.ptr_eq(&self.mnt.mnt.mnt_root) {
            return Err(VfsError::InvalidInput);
        }
        let parent_path = self.mnt.location().ok_or(VfsError::InvalidInput)?;
        let inode = parent_path.inode();
        let _namespace_guard = inode.lock_namespace_exclusive();
        if !parent_path
            .mnt
            .child_mount_at(&parent_path.dentry)
            .is_some_and(|mount| Arc::ptr_eq(&mount, &self.mnt))
        {
            return Err(VfsError::InvalidInput);
        }
        if !self.mnt.mnt_mounts.lock().is_empty() {
            return Err(VfsError::ResourceBusy);
        }

        let covered = self.mnt.covers.lock().take();
        match covered {
            Some(ref mount) => {
                parent_path
                    .mnt
                    .install_child_mount(&parent_path.dentry, mount);
            }
            None => {
                parent_path.mnt.remove_child_mount(&parent_path.dentry);
            }
        }
        self.mnt.detach();
        Ok(())
    }

    /// Recursively unmount this filesystem and all children.
    #[cfg(unittest)]
    fn unmount_tree(&self) -> VfsResult<()> {
        let mounts = self.mnt.collect_subtree();
        self.detach_mount_subtree(&mounts)
    }

    fn detach_mount_subtree(&self, mounts: &[Arc<Mount>]) -> VfsResult<()> {
        if !self.dentry.ptr_eq(&self.mnt.mnt.mnt_root) {
            return Err(VfsError::InvalidInput);
        }
        if !mounts
            .first()
            .is_some_and(|root| Arc::ptr_eq(root, &self.mnt))
        {
            return Err(VfsError::InvalidInput);
        }

        let parent_path = self.mnt.location().ok_or(VfsError::InvalidInput)?;
        let inode = parent_path.inode();
        let _namespace_guard = inode.lock_namespace_exclusive();
        if !parent_path
            .mnt
            .child_mount_at(&parent_path.dentry)
            .is_some_and(|mount| Arc::ptr_eq(&mount, &self.mnt))
        {
            return Err(VfsError::InvalidInput);
        }

        let covered = self.mnt.covers.lock().take();
        match covered {
            Some(ref mount) => {
                parent_path
                    .mnt
                    .install_child_mount(&parent_path.dentry, mount);
            }
            None => parent_path.mnt.remove_child_mount(&parent_path.dentry),
        }

        // All fallible checks complete before this commit phase. The namespace
        // entry point also validates registry membership for every layer. The
        // remaining operations are infallible, so an overmount cannot be
        // restored halfway and leave stale registry entries.
        for mount in mounts {
            mount.mnt_mounts.lock().clear();
            mount.covers.lock().take();
            mount.detach();
        }
        Ok(())
    }

    /// Changes a regular file length through the VFS truncate path.
    pub fn truncate(&self, len: u64, cred: &Cred) -> VfsResult<()> {
        self.permission(Permission::MAY_WRITE, cred)?;
        self.truncate_opened(len)
    }

    pub(crate) fn truncate_opened(&self, len: u64) -> VfsResult<()> {
        self.check_writable_mount()?;
        if self.is_dir() {
            return Err(VfsError::IsADirectory);
        }

        let inode = self.inode();
        if len > self.max_file_size() {
            return Err(VfsError::FileTooLarge);
        }
        if self.is_regular_file() && !inode.flags().contains(NodeFlags::NON_CACHEABLE) {
            inode.set_len(len)?;
        } else {
            self.dentry.update_metadata(MetadataUpdate {
                size: Some(len),
                ..Default::default()
            })?;
        }
        Ok(())
    }

    /// Resolves overmounts visible from this path.
    pub(crate) fn resolve_final_mount(self) -> Self {
        let mut path = self;
        while let Some(mountpoint) = path.mnt.child_mount_at(&path.dentry) {
            let entry = mountpoint.mnt.mnt_root.clone();
            path = Self::new(mountpoint, entry);
        }
        path
    }
}

fn clone_mount_tree(
    source: &Arc<Mount>,
    location_in_parent: Option<Path>,
    mount_map: &mut HashMap<usize, Arc<Mount>>,
) -> Arc<Mount> {
    let cloned = Mount::clone_mnt(&source.root_path()).into_arc_at(location_in_parent);
    mount_map.insert(Arc::as_ptr(source) as usize, cloned.clone());

    if let Some(covered) = source.covered_mount() {
        let location = cloned.location();
        let cloned_covered = clone_mount_tree(&covered, location, mount_map);
        *cloned.covers.lock() = Some(cloned_covered);
    }

    for child in source.children() {
        let mountpoint = child
            .location()
            .expect("child mount must have mountpoint")
            .dentry;
        let location = Path::new(cloned.clone(), mountpoint.clone());
        let cloned_child = clone_mount_tree(&child, Some(location), mount_map);
        cloned.install_child_mount(&mountpoint, &cloned_child);
    }

    cloned
}

fn retarget_path(path: &Path, mount_map: &HashMap<usize, Arc<Mount>>) -> VfsResult<Path> {
    let key = Arc::as_ptr(path.mount()) as usize;
    let mount = mount_map.get(&key).ok_or(VfsError::InvalidInput)?;
    Ok(Path::new(mount.clone(), path.dentry.clone()))
}

#[cfg(unittest)]
mod tests {
    extern crate alloc;

    use alloc::{string::String, sync::Arc};

    use ktime_types::SystemTime;
    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::{
        Dentry, DirContext, FileDirOperations, FileOperations, FsContextPurpose,
        InodeDirOperations, InodeOperations, LockedDentry, Metadata, MetadataUpdate,
        NodePermission, NodeType, StatFs, StatFsFlags, SuperBlockFlags, SuperBlockOperations,
        VfsError, VfsFile, VfsInode, VfsInodeInit, VfsResult,
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
            let inode = VfsInode::new_openable_dir(
                Arc::new(MockDirOps::new(self.mount_flags, inode)),
                init,
            );
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
    fn test_unmount_rejects_non_mount_root_path() {
        let root_fs = mock_filesystem(StatFsFlags::empty());
        let root_mount = Mount::new_root(&root_fs);
        let mount_dir = lookup_no_follow(&root_mount.root_path(), "mnt").unwrap();

        assert_eq!(mount_dir.unmount(), Err(VfsError::InvalidInput));
        assert_eq!(mount_dir.unmount_tree(), Err(VfsError::InvalidInput));
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
}

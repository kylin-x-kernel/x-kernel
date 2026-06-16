// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Mountpoints and location resolution for the VFS.
use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::{
    iter, mem,
    sync::atomic::{AtomicU64, Ordering},
    task::Context,
};

use hashbrown::HashMap;
use inherit_methods_macro::inherit_methods;
use kpoll::{IoEvents, Pollable};

use crate::{
    AddressSpace, AddressSpaceOperations, DirEntry, DirEntrySink, Filesystem, FilesystemOps,
    Metadata, MetadataUpdate, Mutex, MutexGuard, NodeFlags, NodePermission, NodeType, OpenOptions,
    ReferenceKey, ST_RDONLY, SuperBlock, TypeMap, VfsError, VfsInode, VfsResult,
    path::{DOT, DOTDOT, PathBuf},
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

/// A mounted filesystem instance and its relationships.
#[derive(Debug)]
pub struct Mountpoint {
    /// Superblock backing this mounted filesystem instance.
    super_block: Arc<SuperBlock>,
    /// Root dir entry in the mountpoint.
    root: DirEntry,
    /// Location in the parent mountpoint.
    location: Option<Location>,
    /// Children of the mountpoint - tracks nested mounts under this mountpoint.
    /// Maps from directory entry keys to weak references to child mountpoints.
    child_mounts: Mutex<HashMap<ReferenceKey, Weak<Self>>>,
    /// Device ID
    device: u64,
    /// Per-mount flags.
    flags: MountFlags,
    /// Mount that this one covers (for overmount).
    ///
    /// When a new mount is created at a location that already has a mount,
    /// the old mount is stored here so it can be restored on unmount.
    covers: Mutex<Option<Arc<Self>>>,
}

impl Mountpoint {
    /// Creates a new mountpoint for a filesystem at an optional parent location.
    pub fn new(fs: &Filesystem, location_in_parent: Option<Location>) -> Arc<Self> {
        Self::new_with_flags(fs, location_in_parent, MountFlags::empty())
    }

    /// Creates a new mountpoint with per-mount flags.
    pub fn new_with_flags(
        fs: &Filesystem,
        location_in_parent: Option<Location>,
        flags: MountFlags,
    ) -> Arc<Self> {
        static DEVICE_COUNTER: AtomicU64 = AtomicU64::new(1);

        let root = fs.root_dir();
        Arc::new(Self {
            super_block: fs.super_block().clone(),
            root,
            location: location_in_parent,
            child_mounts: Mutex::default(),
            device: DEVICE_COUNTER.fetch_add(1, Ordering::Relaxed),
            flags,
            covers: Mutex::default(),
        })
    }

    /// Creates a root mountpoint for a filesystem.
    pub fn new_root(fs: &Filesystem) -> Arc<Self> {
        Self::new(fs, None)
    }

    /// Creates a root mountpoint with per-mount flags.
    pub fn new_root_with_flags(fs: &Filesystem, flags: MountFlags) -> Arc<Self> {
        Self::new_with_flags(fs, None, flags)
    }

    /// Return a `Location` representing the mountpoint root.
    pub fn root_location(self: &Arc<Self>) -> Location {
        Location::new(self.clone(), self.root.clone())
    }

    /// Returns the superblock backing this mountpoint.
    pub fn super_block(&self) -> &Arc<SuperBlock> {
        &self.super_block
    }

    /// Returns the location in the parent mountpoint.
    pub fn location(&self) -> Option<Location> {
        self.location.clone()
    }

    /// Returns whether this mountpoint has no parent mount.
    pub fn is_root(&self) -> bool {
        self.location.is_none()
    }

    /// Returns the effective (visible) mountpoint by traversing the mount chain.
    ///
    /// When multiple filesystems are mounted at the same location, they form a chain
    /// where each new mount hides the previous one. This method traverses the chain
    /// to find the final, visible mountpoint.
    ///
    /// # Example
    ///
    /// ```text
    /// mount /dev/sda1 /mnt  -> creates mountpoint A
    /// mount /dev/sda2 /mnt  -> creates mountpoint B at A's root
    /// A.resolve_final_mount() -> returns B (the visible mount)
    /// ```
    ///
    /// # Implementation
    ///
    /// Follows the chain: root mount -> mnt1 -> mnt2 -> ... -> final mount
    /// by checking if each root directory has a mountpoint attached.
    pub(crate) fn resolve_final_mount(self: &Arc<Self>) -> Arc<Mountpoint> {
        let mut mountpoint = self.clone();
        while let Some(mount) = mountpoint
            .root
            .as_dir()
            .expect("mount root must be a directory")
            .mountpoint()
        {
            mountpoint = mount;
        }
        mountpoint
    }

    /// Returns the mountpoint's synthetic device ID.
    pub fn device(self: &Arc<Self>) -> u64 {
        self.device
    }

    /// Returns this mountpoint's flags.
    pub fn flags(&self) -> MountFlags {
        self.flags
    }

    /// Returns whether this mountpoint is mounted read-only.
    pub fn is_readonly(&self) -> bool {
        self.flags.contains(MountFlags::RDONLY)
    }

    /// Returns a snapshot of direct child mountpoints currently attached here.
    pub fn child_mounts(self: &Arc<Self>) -> Vec<Arc<Self>> {
        self.child_mounts
            .lock()
            .values()
            .filter_map(Weak::upgrade)
            .collect()
    }
}

/// A resolved location within a mountpoint.
#[derive(Debug, Clone)]
pub struct Location {
    mountpoint: Arc<Mountpoint>,
    entry: DirEntry,
}

#[inherit_methods(from = "self.entry")]
impl Location {
    pub fn inode(&self) -> u64;

    pub fn filesystem(&self) -> &dyn FilesystemOps;

    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> VfsResult<u64>;

    pub fn sync(&self, data_only: bool) -> VfsResult<()>;

    pub fn is_file(&self) -> bool;

    pub fn is_dir(&self) -> bool;

    pub fn node_type(&self) -> NodeType;

    pub fn is_root_of_mount(&self) -> bool;

    pub fn read_link(&self) -> VfsResult<String>;

    pub fn ioctl(&self, cmd: u32, arg: usize) -> VfsResult<usize>;

    pub fn flags(&self) -> NodeFlags;

    pub fn dentry_data(&self) -> MutexGuard<'_, TypeMap>;

    pub fn user_data(&self) -> MutexGuard<'_, TypeMap>;

    pub fn inode_data(&self) -> MutexGuard<'_, TypeMap>;

    pub fn vfs_inode(&self) -> &Arc<VfsInode>;

    pub fn address_space(&self) -> Option<Arc<AddressSpace>>;
}

impl Location {
    /// Create a location from a mountpoint and directory entry.
    pub fn new(mountpoint: Arc<Mountpoint>, entry: DirEntry) -> Self {
        Self { mountpoint, entry }
    }

    fn with_entry(&self, entry: DirEntry) -> Self {
        Self::new(self.mountpoint.clone(), entry)
    }

    /// Returns the mountpoint containing this location.
    pub fn mountpoint(&self) -> &Arc<Mountpoint> {
        &self.mountpoint
    }

    /// Returns the superblock containing this location.
    pub fn super_block(&self) -> &Arc<SuperBlock> {
        self.mountpoint.super_block()
    }

    /// Returns the underlying directory entry.
    pub fn entry(&self) -> &DirEntry {
        &self.entry
    }

    /// Return or create this location's inode address-space object.
    pub fn get_or_insert_address_space(
        &self,
        ops: Arc<dyn AddressSpaceOperations>,
    ) -> Arc<AddressSpace> {
        let address_space = self.entry.vfs_inode().get_or_insert_address_space(ops);
        self.super_block().register_address_space(&address_space);
        address_space
    }

    /// Return or create this location's inode address-space object.
    pub fn get_or_insert_address_space_with(
        &self,
        create: impl FnOnce() -> AddressSpace,
    ) -> Arc<AddressSpace> {
        let address_space = self
            .entry
            .vfs_inode()
            .get_or_insert_address_space_with(create);
        self.super_block().register_address_space(&address_space);
        address_space
    }

    /// Returns the name of this location within its parent directory.
    ///
    /// When this location is a mount root, the name is taken from the mountpoint's
    /// parent location chain (the directory where the mount was attached). This
    /// recursion terminates because the topmost mount has no parent location
    /// and returns the empty string.
    pub fn name(&self) -> &str {
        if self.is_root_of_mount() {
            self.mountpoint.location.as_ref().map_or("", Location::name)
        } else {
            self.entry.name()
        }
    }

    /// Returns the parent location, if any.
    pub fn parent(&self) -> Option<Self> {
        if !self.is_root_of_mount() {
            return Some(self.with_entry(self.entry.parent()?));
        }
        self.mountpoint.location()?.parent()
    }

    /// Returns `true` if this is the global root location.
    pub fn is_root(&self) -> bool {
        self.mountpoint.is_root() && self.entry.is_root_of_mount()
    }

    /// Ensure the location refers to a directory.
    pub fn check_is_dir(&self) -> VfsResult<()> {
        self.entry.as_dir().map(|_| ())
    }

    /// Ensure the location refers to a file.
    pub fn check_is_file(&self) -> VfsResult<()> {
        self.entry.as_file().map(|_| ())
    }

    /// Handle mmap for the file at this location.
    ///
    /// Delegates to the underlying file node's mmap implementation.
    pub fn mmap(&self, mapper: &mut dyn crate::MmapMapper) -> VfsResult<()> {
        self.entry.as_file()?.mmap(mapper)
    }

    /// Returns whether this location's mountpoint is read-only.
    pub fn is_mount_readonly(&self) -> bool {
        self.mountpoint.is_readonly()
    }

    /// Returns whether this location is effectively read-only.
    pub fn is_effectively_readonly(&self) -> bool {
        self.is_mount_readonly()
            || self
                .filesystem()
                .stat()
                .is_ok_and(|stat| stat.mount_flags & ST_RDONLY != 0)
    }

    /// Ensures this location can be modified.
    pub fn check_writable_mount(&self) -> VfsResult<()> {
        if self.is_effectively_readonly() {
            return Err(VfsError::ReadOnlyFilesystem);
        }
        Ok(())
    }

    /// Updates metadata after checking mount writability.
    pub fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()> {
        self.check_writable_mount()?;
        self.entry.update_metadata(update)
    }

    /// Returns metadata with the mountpoint device ID applied.
    pub fn metadata(&self) -> VfsResult<Metadata> {
        let mut metadata = self.entry.metadata()?;
        metadata.device = self.mountpoint.device();
        Ok(metadata)
    }

    /// Build the absolute path for this location.
    pub fn absolute_path(&self) -> VfsResult<PathBuf> {
        let mut components = vec![];
        let mut cur = self.clone();
        loop {
            cur.entry.collect_absolute_path(&mut components);
            cur = match cur.mountpoint.location() {
                Some(loc) => loc,
                None => break,
            }
        }
        Ok(iter::once("/")
            .chain(components.iter().map(String::as_str).rev())
            .collect())
    }

    /// Returns `true` if this location references the same entry.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.mountpoint, &other.mountpoint) && self.entry.ptr_eq(&other.entry)
    }

    /// Returns `true` if both locations point at the same VFS inode identity.
    pub fn is_same_inode(&self, other: &Self) -> bool {
        self.entry.is_same_inode(&other.entry)
    }

    /// Returns `true` if this location is a mountpoint directory.
    pub fn is_mountpoint(&self) -> bool {
        self.entry.as_dir().is_ok_and(|it| it.is_mountpoint())
    }

    /// See [`Mountpoint::resolve_final_mount`].
    fn resolve_final_mount(self) -> Self {
        let Some(mountpoint) = self.entry.as_dir().ok().and_then(|it| it.mountpoint()) else {
            return self;
        };
        let mountpoint = mountpoint.resolve_final_mount();
        let entry = mountpoint.root.clone();
        Self::new(mountpoint, entry)
    }

    fn lookup_child_in_mount(&self, name: &str) -> VfsResult<Self> {
        Ok(Self::new(
            self.mountpoint.clone(),
            self.entry.as_dir()?.lookup(name)?,
        ))
    }

    /// Look up a child entry without following a symlink.
    pub fn lookup_no_follow(&self, name: &str) -> VfsResult<Self> {
        Ok(match name {
            DOT => self.clone(),
            DOTDOT => self.parent().unwrap_or_else(|| self.clone()),
            _ => self.lookup_child_in_mount(name)?.resolve_final_mount(),
        })
    }

    /// Create a new entry under this directory.
    pub fn create(
        &self,
        name: &str,
        node_type: NodeType,
        permission: NodePermission,
    ) -> VfsResult<Self> {
        self.check_writable_mount()?;
        self.entry
            .as_dir()?
            .create(name, node_type, permission)
            .map(|entry| self.with_entry(entry))
    }

    /// Create a hard link to an existing node.
    pub fn link(&self, name: &str, node: &Self) -> VfsResult<Self> {
        if !Arc::ptr_eq(&self.mountpoint, &node.mountpoint) {
            return Err(VfsError::CrossesDevices);
        }
        self.check_writable_mount()?;
        self.entry
            .as_dir()?
            .link(name, &node.entry)
            .map(|entry| self.with_entry(entry))
    }

    fn check_not_mountpoint(&self) -> VfsResult<()> {
        if self.is_mountpoint() {
            return Err(VfsError::ResourceBusy);
        }
        Ok(())
    }

    /// Rename an entry within the same mountpoint.
    pub fn rename(&self, src_name: &str, dst_dir: &Self, dst_name: &str) -> VfsResult<()> {
        if !Arc::ptr_eq(&self.mountpoint, &dst_dir.mountpoint) {
            return Err(VfsError::CrossesDevices);
        }
        self.check_writable_mount()?;
        dst_dir.check_writable_mount()?;

        // DirNode::rename revalidates by name; keep these lookups until it can take resolved entries.
        let src_loc = self.lookup_child_in_mount(src_name)?;
        src_loc.check_not_mountpoint()?;
        if let Ok(dst_loc) = dst_dir.lookup_child_in_mount(dst_name) {
            dst_loc.check_not_mountpoint()?;
        }
        if !self.ptr_eq(dst_dir)
            && src_loc.entry.node_type() == NodeType::Directory
            && src_loc.entry.is_ancestor_of(&dst_dir.entry)?
        {
            return Err(VfsError::InvalidInput);
        }
        self.entry
            .as_dir()?
            .rename(src_name, dst_dir.entry.as_dir()?, dst_name)
    }

    /// Remove a file or directory entry.
    pub fn unlink(&self, name: &str, is_dir: bool) -> VfsResult<()> {
        self.check_writable_mount()?;
        // DirNode::unlink revalidates by name; keep this lookup until it can take a resolved entry.
        let loc = self.lookup_child_in_mount(name)?;
        match (loc.is_dir(), is_dir) {
            (true, false) => return Err(VfsError::IsADirectory),
            (false, true) => return Err(VfsError::NotADirectory),
            _ => {}
        }
        loc.check_not_mountpoint()?;
        self.entry.as_dir()?.unlink(name, is_dir)
    }

    /// Open a file entry with options.
    pub fn open_file(&self, name: &str, options: &OpenOptions) -> VfsResult<Location> {
        let dir = self.entry.as_dir()?;
        if options.create || options.create_new {
            self.check_writable_mount()?;
        }
        dir.open_file(name, options)
            .map(|entry| self.with_entry(entry).resolve_final_mount())
    }

    /// Read directory entries starting from the given offset.
    pub fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
        self.entry.as_dir()?.read_dir(offset, sink)
    }

    /// Mounts a filesystem at this location.
    pub fn mount(&self, fs: &Filesystem) -> VfsResult<Arc<Mountpoint>> {
        self.mount_with_flags(fs, MountFlags::empty())
    }

    /// Mounts a filesystem with per-mount flags at this location.
    ///
    /// # Lock ordering
    ///
    /// Acquires `mount_at_this_dir` (target dentry) then `parent.child_mounts`.
    /// The apparent reverse order with `unmount` is safe because `child_mounts`
    /// belongs to a different `Mountpoint`:
    ///
    /// ```text
    ///   mount_on(D in M):   lock D.mount_at_this_dir → lock M.child_mounts
    ///   unmount(C from D):  lock C.child_mounts       → lock D.mount_at_this_dir
    /// ```
    ///
    /// If both race, the mount thread holds `D.mount_at_this_dir` and wants
    /// `M.child_mounts`; the unmount thread holds `C.child_mounts` and wants
    /// `D.mount_at_this_dir`.  The mount thread is never blocked on a lock
    /// the unmount thread holds (`M.child_mounts ≠ C.child_mounts`), so it
    /// finishes first and releases `D.mount_at_this_dir`.
    pub fn mount_with_flags(
        &self,
        fs: &Filesystem,
        flags: MountFlags,
    ) -> VfsResult<Arc<Mountpoint>> {
        let mut mountpoint = self.entry.as_dir()?.mount_at_this_dir.lock();
        let result = Mountpoint::new_with_flags(fs, Some(self.clone()), flags);
        // Overmount: the new mount covers any existing mount at this location.
        // The old mount is stashed so it can be restored on unmount.
        if let Some(old) = mountpoint.take() {
            *result.covers.lock() = Some(old);
        }
        *mountpoint = Some(result.clone());
        self.mountpoint
            .child_mounts
            .lock()
            .insert(self.entry.key(), Arc::downgrade(&result));
        Ok(result)
    }

    /// Unmount the filesystem rooted at this location.
    ///
    /// # Lock ordering
    ///
    /// Acquires `parent.mount_at_this_dir` → `self.child_mounts` →
    /// `parent.child_mounts`.  The `parent.mount_at_this_dir → parent.child_mounts`
    /// tail matches `mount_with_flags`; `self.child_mounts` is a distinct
    /// instance so no deadlock.  The `child_mounts` check is done under
    /// `mount_at_this_dir` to close the TOCTOU window.
    pub fn unmount(&self) -> VfsResult<()> {
        if !self.is_root_of_mount() {
            return Err(VfsError::InvalidInput);
        }
        if !self.entry.ptr_eq(&self.mountpoint.root) {
            return Err(VfsError::InvalidInput);
        }
        if let Some(parent_loc) = &self.mountpoint.location {
            let mut mount_slot = parent_loc.entry.as_dir()?.mount_at_this_dir.lock();
            // Guard against overmount race: self must still be the visible mount
            // at this dentry after we acquired the lock.
            if !mount_slot
                .as_ref()
                .is_some_and(|m| Arc::ptr_eq(m, &self.mountpoint))
            {
                return Err(VfsError::InvalidInput);
            }
            // Re-check child_mounts under mount_slot to close the TOCTOU window
            // between the earlier check and acquiring mount_slot.
            if !self.mountpoint.child_mounts.lock().is_empty() {
                return Err(VfsError::ResourceBusy);
            }
            let mut parent_children = parent_loc.mountpoint.child_mounts.lock();
            let covered = self.mountpoint.covers.lock().take();
            *mount_slot = covered.clone();
            match covered {
                Some(ref m) => {
                    parent_children.insert(parent_loc.entry.key(), Arc::downgrade(m));
                }
                None => {
                    parent_children.remove(&parent_loc.entry.key());
                }
            }
            // Drop parent locks before forget.  Order matches mount_with_flags.
        }
        // forget() after parent state is committed — if the parent update
        // failed we must not have destroyed the dentry cache prematurely.
        self.entry.as_dir()?.forget();
        Ok(())
    }

    /// Recursively unmount this filesystem and all children.
    pub fn unmount_all(&self) -> VfsResult<()> {
        if !self.is_root_of_mount() {
            return Err(VfsError::InvalidInput);
        }
        let mut children = self.mountpoint.child_mounts.lock();
        let remaining = mem::take(&mut *children);
        drop(children);
        let mut failed = false;
        for (key, child) in remaining {
            if let Some(m) = child.upgrade()
                && let Err(_e) = m.root_location().unmount_all()
            {
                failed = true;
                // Re-insert so the child is not orphaned.
                self.mountpoint
                    .child_mounts
                    .lock()
                    .insert(key, Arc::downgrade(&m));
            }
        }
        if failed {
            return Err(VfsError::ResourceBusy);
        }
        self.unmount()
    }
}

#[inherit_methods(from = "self.entry")]
impl Pollable for Location {
    fn poll(&self) -> IoEvents;

    fn register(&self, context: &mut Context<'_>, events: IoEvents);
}

#[cfg(unittest)]
mod tests {
    extern crate alloc;

    use alloc::{string::String, sync::Arc};
    use core::{any::Any, time::Duration};

    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::{
        DirEntry, DirEntrySink, DirNode, DirNodeOps, Filesystem, FilesystemOps, Metadata,
        MetadataUpdate, NodeOps, NodePermission, NodeType, Reference, StatFs, VfsError, VfsResult,
    };

    struct MockFilesystem {
        mount_flags: u32,
    }

    impl FilesystemOps for MockFilesystem {
        fn name(&self) -> &str {
            "mockfs"
        }

        fn root_dir(&self) -> DirEntry {
            DirEntry::new_dir(
                |_| DirNode::new(Arc::new(MockDirNodeOps::new(self.mount_flags, 1))),
                Reference::root(),
            )
        }

        fn stat(&self) -> VfsResult<StatFs> {
            statfs(self.mount_flags)
        }
    }

    struct MockDirNodeOps {
        mount_flags: u32,
        inode: u64,
    }

    impl MockDirNodeOps {
        fn new(mount_flags: u32, inode: u64) -> Self {
            Self { mount_flags, inode }
        }
    }

    impl NodeOps for MockDirNodeOps {
        fn inode(&self) -> u64 {
            self.inode
        }

        fn metadata(&self) -> VfsResult<Metadata> {
            Ok(Metadata {
                device: 0,
                inode: self.inode,
                nlink: 1,
                mode: NodePermission::default(),
                node_type: NodeType::Directory,
                uid: 0,
                gid: 0,
                size: 0,
                block_size: 512,
                blocks: 1,
                rdev: Default::default(),
                atime: Duration::ZERO,
                mtime: Duration::ZERO,
                ctime: Duration::ZERO,
            })
        }

        fn update_metadata(&self, _update: MetadataUpdate) -> VfsResult<()> {
            Ok(())
        }

        fn filesystem(&self) -> &dyn FilesystemOps {
            self
        }

        fn sync(&self, _data_only: bool) -> VfsResult<()> {
            Ok(())
        }

        fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
            self
        }
    }

    impl FilesystemOps for MockDirNodeOps {
        fn name(&self) -> &str {
            "mockfs"
        }

        fn root_dir(&self) -> DirEntry {
            panic!("root_dir is not used through directory nodes")
        }

        fn stat(&self) -> VfsResult<StatFs> {
            statfs(self.mount_flags)
        }
    }

    impl DirNodeOps for MockDirNodeOps {
        fn read_dir(&self, _offset: u64, _sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
            Ok(0)
        }

        fn lookup(&self, name: &str) -> VfsResult<DirEntry> {
            if name != "mnt" {
                return Err(VfsError::NotFound);
            }

            Ok(DirEntry::new_dir(
                |_| {
                    DirNode::new(Arc::new(MockDirNodeOps::new(
                        self.mount_flags,
                        self.inode + 1,
                    )))
                },
                Reference::new(None, String::from(name)),
            ))
        }

        fn create(
            &self,
            _name: &str,
            _node_type: NodeType,
            _permission: NodePermission,
        ) -> VfsResult<DirEntry> {
            Err(VfsError::OperationNotSupported)
        }

        fn link(&self, _name: &str, _node: &DirEntry) -> VfsResult<DirEntry> {
            Err(VfsError::OperationNotSupported)
        }

        fn unlink(&self, _name: &str) -> VfsResult<()> {
            Err(VfsError::OperationNotSupported)
        }

        fn rename(&self, _src_name: &str, _dst_dir: &DirNode, _dst_name: &str) -> VfsResult<()> {
            Err(VfsError::OperationNotSupported)
        }
    }

    fn mock_filesystem(mount_flags: u32) -> Filesystem {
        Filesystem::new(Arc::new(MockFilesystem { mount_flags }))
    }

    fn statfs(mount_flags: u32) -> VfsResult<StatFs> {
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
            mount_flags,
        })
    }

    #[def_test]
    fn test_mountpoint_thread_safety() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Mountpoint>();
    }

    #[def_test]
    fn test_root_mount_defaults_to_writable() {
        let fs = mock_filesystem(0);
        let mount = Mountpoint::new_root(&fs);
        let root = mount.root_location();

        assert_eq!(mount.flags(), MountFlags::empty());
        assert!(!mount.is_readonly());
        assert!(!root.is_mount_readonly());
        assert!(!root.is_effectively_readonly());
        assert_eq!(root.check_writable_mount(), Ok(()));
    }

    #[def_test]
    fn test_root_mount_can_be_readonly() {
        let fs = mock_filesystem(0);
        let mount = Mountpoint::new_root_with_flags(&fs, MountFlags::RDONLY);
        let root = mount.root_location();

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
        let root_fs = mock_filesystem(0);
        let child_fs = mock_filesystem(0);
        let root_mount = Mountpoint::new_root_with_flags(&root_fs, MountFlags::RDONLY);
        let mount_dir = root_mount.root_location().lookup_no_follow("mnt").unwrap();
        let child_mount = mount_dir
            .mount_with_flags(&child_fs, MountFlags::empty())
            .unwrap();
        let child_root = child_mount.root_location();

        assert!(root_mount.is_readonly());
        assert!(!child_mount.is_readonly());
        assert!(!child_root.is_mount_readonly());
        assert!(!child_root.is_effectively_readonly());
    }

    #[def_test]
    fn test_filesystem_stat_readonly_makes_location_effectively_readonly() {
        let fs = mock_filesystem(crate::ST_RDONLY);
        let mount = Mountpoint::new_root(&fs);
        let root = mount.root_location();

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
    fn test_overmount_hides_previous_and_restores_on_unmount() {
        let root_fs = mock_filesystem(0);
        let fs_a = mock_filesystem(0);
        let fs_b = mock_filesystem(0);

        let root_mount = Mountpoint::new_root(&root_fs);
        let mnt_loc = root_mount.root_location().lookup_no_follow("mnt").unwrap();

        let mount_a = mnt_loc.mount(&fs_a).unwrap();
        let loc_a = root_mount.root_location().lookup_no_follow("mnt").unwrap();
        assert!(Arc::ptr_eq(&loc_a.mountpoint().clone(), &mount_a));

        let mount_b = mnt_loc.mount_with_flags(&fs_b, MountFlags::RDONLY).unwrap();
        let loc_b = root_mount.root_location().lookup_no_follow("mnt").unwrap();
        assert!(Arc::ptr_eq(&loc_b.mountpoint().clone(), &mount_b));
        assert!(loc_b.is_mount_readonly());

        mount_b.root_location().unmount().unwrap();
        let loc_a_again = root_mount.root_location().lookup_no_follow("mnt").unwrap();
        assert!(Arc::ptr_eq(&loc_a_again.mountpoint().clone(), &mount_a));
    }

    #[def_test]
    fn test_readonly_mount_blocks_create() {
        let fs = mock_filesystem(0);
        let mount = Mountpoint::new_root_with_flags(&fs, MountFlags::RDONLY);
        let root = mount.root_location();

        let result = root.create("test", NodeType::RegularFile, NodePermission::default());
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), VfsError::ReadOnlyFilesystem);
    }

    #[def_test]
    fn test_readonly_mount_blocks_unlink() {
        let fs = mock_filesystem(0);
        let mount = Mountpoint::new_root_with_flags(&fs, MountFlags::RDONLY);
        let root = mount.root_location();

        let result = root.unlink("test", false);
        assert_eq!(result, Err(VfsError::ReadOnlyFilesystem));
    }

    #[def_test]
    fn test_readonly_mount_blocks_rename() {
        let fs = mock_filesystem(0);
        let mount = Mountpoint::new_root_with_flags(&fs, MountFlags::RDONLY);
        let root = mount.root_location();

        let result = root.rename("a", &root, "b");
        assert_eq!(result, Err(VfsError::ReadOnlyFilesystem));
    }

    #[def_test]
    fn test_writable_mount_allows_operations_to_reach_filesystem() {
        let fs = mock_filesystem(0);
        let mount = Mountpoint::new_root(&fs);
        let root = mount.root_location();

        let result = root.create("test", NodeType::RegularFile, NodePermission::default());
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), VfsError::OperationNotSupported);

        assert_eq!(root.unlink("test", false), Err(VfsError::NotFound));
    }

    #[def_test]
    fn test_stat_rdonly_combined_with_mount_rdonly_is_readonly() {
        let fs = mock_filesystem(crate::ST_RDONLY);
        let mount = Mountpoint::new_root_with_flags(&fs, MountFlags::RDONLY);
        let root = mount.root_location();

        assert!(mount.is_readonly());
        assert!(root.is_effectively_readonly());
        assert_eq!(
            root.check_writable_mount(),
            Err(VfsError::ReadOnlyFilesystem)
        );
    }

    #[def_test]
    fn test_st_constants_values() {
        assert_eq!(crate::ST_RDONLY, 0x0001);
        assert_eq!(crate::ST_NOSUID, 0x0002);
        assert_eq!(crate::ST_NODEV, 0x0004);
        assert_eq!(crate::ST_NOEXEC, 0x0008);
        assert_eq!(crate::ST_NOATIME, 0x0400);
        assert_eq!(crate::ST_NODIRATIME, 0x0800);
        assert_eq!(crate::ST_RELATIME, 0x1000);
        assert_eq!(crate::ST_NOSYMFOLLOW, 0x2000);
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

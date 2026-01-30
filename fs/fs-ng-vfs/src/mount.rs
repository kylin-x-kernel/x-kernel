// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Mountpoints and location resolution for the VFS.
use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec,
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
    DirEntry, DirEntrySink, Filesystem, FilesystemOps, Metadata, MetadataUpdate, Mutex, MutexGuard,
    NodeFlags, NodePermission, NodeType, OpenOptions, ReferenceKey, TypeMap, VfsError, VfsResult,
    path::{DOT, DOTDOT, PathBuf},
};

/// A mounted filesystem instance and its relationships.
#[derive(Debug)]
pub struct Mountpoint {
    /// Root dir entry in the mountpoint.
    root: DirEntry,
    /// Location in the parent mountpoint.
    location: Option<Location>,
    /// Children of the mountpoint - tracks nested mounts under this mountpoint.
    /// Maps from directory entry keys to weak references to child mountpoints.
    child_mounts: Mutex<HashMap<ReferenceKey, Weak<Self>>>,
    /// Device ID
    device: u64,
}

impl Mountpoint {
    /// Create a new mountpoint for a filesystem at an optional parent location.
    pub fn new(fs: &Filesystem, location_in_parent: Option<Location>) -> Arc<Self> {
        static DEVICE_COUNTER: AtomicU64 = AtomicU64::new(1);

        let root = fs.root_dir();
        Arc::new(Self {
            root,
            location: location_in_parent,
            child_mounts: Mutex::default(),
            device: DEVICE_COUNTER.fetch_add(1, Ordering::Relaxed),
        })
    }

    /// Create a root mountpoint for a filesystem.
    pub fn new_root(fs: &Filesystem) -> Arc<Self> {
        Self::new(fs, None)
    }

    /// Return a `Location` representing the mountpoint root.
    pub fn root_location(self: &Arc<Self>) -> Location {
        Location::new(self.clone(), self.root.clone())
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
        while let Some(mount) = mountpoint.root.as_dir().unwrap().mountpoint() {
            mountpoint = mount;
        }
        mountpoint
    }

    /// Returns the mountpoint's synthetic device ID.
    pub fn device(self: &Arc<Self>) -> u64 {
        self.device
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

    pub fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()>;

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

    pub fn user_data(&self) -> MutexGuard<'_, TypeMap>;
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

    /// Returns the underlying directory entry.
    pub fn entry(&self) -> &DirEntry {
        &self.entry
    }

    /// Returns the name of this location within its parent directory.
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
            return Some(self.with_entry(self.entry.parent().unwrap()));
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

    /// Look up a child entry without following a symlink.
    pub fn lookup_no_follow(&self, name: &str) -> VfsResult<Self> {
        Ok(match name {
            DOT => self.clone(),
            DOTDOT => self.parent().unwrap_or_else(|| self.clone()),
            _ => {
                let loc = Self::new(self.mountpoint.clone(), self.entry.as_dir()?.lookup(name)?);
                loc.resolve_final_mount()
            }
        })
    }

    /// Create a new entry under this directory.
    pub fn create(
        &self,
        name: &str,
        node_type: NodeType,
        permission: NodePermission,
    ) -> VfsResult<Self> {
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
        self.entry
            .as_dir()?
            .link(name, &node.entry)
            .map(|entry| self.with_entry(entry))
    }

    /// Rename an entry within the same mountpoint.
    pub fn rename(&self, src_name: &str, dst_dir: &Self, dst_name: &str) -> VfsResult<()> {
        if !Arc::ptr_eq(&self.mountpoint, &dst_dir.mountpoint) {
            return Err(VfsError::CrossesDevices);
        }
        if !self.ptr_eq(dst_dir) && self.entry.is_ancestor_of(&dst_dir.entry)? {
            return Err(VfsError::InvalidInput);
        }
        self.entry
            .as_dir()?
            .rename(src_name, dst_dir.entry.as_dir()?, dst_name)
    }

    /// Remove a file or directory entry.
    pub fn unlink(&self, name: &str, is_dir: bool) -> VfsResult<()> {
        self.entry.as_dir()?.unlink(name, is_dir)
    }

    /// Open a file entry with options.
    pub fn open_file(&self, name: &str, options: &OpenOptions) -> VfsResult<Location> {
        self.entry
            .as_dir()?
            .open_file(name, options)
            .map(|entry| self.with_entry(entry).resolve_final_mount())
    }

    /// Read directory entries starting from the given offset.
    pub fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
        self.entry.as_dir()?.read_dir(offset, sink)
    }

    /// Mount a filesystem at this location.
    pub fn mount(&self, fs: &Filesystem) -> VfsResult<Arc<Mountpoint>> {
        let mut mountpoint = self.entry.as_dir()?.mount_at_this_dir.lock();
        if mountpoint.is_some() {
            return Err(VfsError::ResourceBusy);
        }
        let result = Mountpoint::new(fs, Some(self.clone()));
        *mountpoint = Some(result.clone());
        self.mountpoint
            .child_mounts
            .lock()
            .insert(self.entry.key(), Arc::downgrade(&result));
        Ok(result)
    }

    /// Unmount the filesystem rooted at this location.
    pub fn unmount(&self) -> VfsResult<()> {
        if !self.is_root_of_mount() {
            return Err(VfsError::InvalidInput);
        }
        if !self.mountpoint.child_mounts.lock().is_empty() {
            return Err(VfsError::ResourceBusy);
        }
        assert!(self.entry.ptr_eq(&self.mountpoint.root));
        self.entry.as_dir()?.forget();
        if let Some(parent_loc) = &self.mountpoint.location {
            *parent_loc.entry.as_dir()?.mount_at_this_dir.lock() = None;
        }
        Ok(())
    }

    /// Recursively unmount this filesystem and all children.
    pub fn unmount_all(&self) -> VfsResult<()> {
        if !self.is_root_of_mount() {
            return Err(VfsError::InvalidInput);
        }
        let children = mem::take(&mut *self.mountpoint.child_mounts.lock());
        for (_, child) in children {
            if let Some(child) = child.upgrade() {
                child.root_location().unmount_all()?;
            }
        }
        self.unmount()
    }
}

#[inherit_methods(from = "self.entry")]
impl Pollable for Location {
    fn poll(&self) -> IoEvents;

    fn register(&self, context: &mut Context<'_>, events: IoEvents);
}

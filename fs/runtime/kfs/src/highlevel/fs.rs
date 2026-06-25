// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! High-level filesystem context and directory iteration utilities.
use alloc::{
    borrow::{Cow, ToOwned},
    collections::vec_deque::VecDeque,
    string::String,
    sync::Arc,
    vec::Vec,
};
use core::iter;

use kio::{Read, Write};
use klazy::Once;
use ksync::Mutex;
use kvfs::{
    Location, LookupContext, LookupFlags, LookupIntent, Metadata, Mountpoint, NodePermission,
    NodeType, VfsError, VfsResult, lookup_location, lookup_nonexistent, lookup_parent,
    path::{Path, PathBuf},
};

use super::File;

/// Global root filesystem context initializer.
pub static ROOT_FS_CONTEXT: Once<FsContext> = Once::new();

/// Kernel-default filesystem context shared by boot and kernel-task paths.
pub static KERNEL_FS_CONTEXT: Once<Arc<Mutex<FsContext>>> = Once::new();

/// Returns the kernel-default filesystem context.
pub fn kernel_fs_context() -> &'static Arc<Mutex<FsContext>> {
    KERNEL_FS_CONTEXT
        .get()
        .expect("kernel FS context not initialized")
}

/// Creates a new process-owned filesystem context from the kernel defaults.
pub fn new_process_fs_context() -> Arc<Mutex<FsContext>> {
    Arc::new(Mutex::new(kernel_fs_context().lock().clone()))
}

fn sync_mount_tree(mountpoint: &Arc<Mountpoint>) -> VfsResult<()> {
    mountpoint.super_block().sync_fs()?;
    for child in mountpoint.child_mounts() {
        sync_mount_tree(&child)?;
    }
    Ok(())
}

/// Synchronizes all filesystems visible from the kernel root mount tree.
pub fn sync_filesystems() -> VfsResult<()> {
    let root_mount = kernel_fs_context().lock().root_dir().mountpoint().clone();
    sync_mount_tree(&root_mount)
}

/// Directory entry returned by `ReadDir`.
pub struct ReadDirEntry {
    /// Entry name.
    pub name: String,
    /// Inode number.
    pub ino: u64,
    /// Node type.
    pub node_type: NodeType,
    /// Offset of the next entry.
    pub offset: u64,
}

/// Per-process filesystem context.
#[derive(Debug, Clone)]
pub struct FsContext {
    root_dir: Location,
    current_dir: Location,
}

impl FsContext {
    /// Create a filesystem context rooted at `root_dir`.
    pub fn new(root_dir: Location) -> Self {
        Self {
            root_dir: root_dir.clone(),
            current_dir: root_dir,
        }
    }

    /// Returns the root directory location.
    pub fn root_dir(&self) -> &Location {
        &self.root_dir
    }

    /// Returns the current working directory.
    pub fn current_dir(&self) -> &Location {
        &self.current_dir
    }

    /// Returns the VFS lookup context represented by this process state.
    pub fn lookup_context(&self) -> LookupContext {
        LookupContext::new(self.root_dir.clone(), self.current_dir.clone())
    }

    fn is_under_root(&self, loc: &Location) -> bool {
        let mut current = loc.clone();
        loop {
            if current.ptr_eq(&self.root_dir) {
                return true;
            }
            let Some(parent) = current.parent() else {
                return false;
            };
            current = parent;
        }
    }

    fn path_from_root(&self, loc: &Location) -> VfsResult<PathBuf> {
        let mut components = Vec::new();
        let mut current = loc.clone();
        loop {
            if current.ptr_eq(&self.root_dir) {
                return Ok(iter::once("/")
                    .chain(components.iter().map(String::as_str).rev())
                    .collect());
            }

            let name = current.name();
            if !name.is_empty() {
                components.push(name.to_owned());
            }

            let Some(parent) = current.parent() else {
                return Err(VfsError::InvalidInput);
            };
            current = parent;
        }
    }

    /// Change the current working directory.
    pub fn set_current_dir(&mut self, current_dir: Location) -> VfsResult<()> {
        current_dir.check_is_dir()?;
        if !self.is_under_root(&current_dir) {
            return Err(VfsError::InvalidInput);
        }
        self.current_dir = current_dir;
        Ok(())
    }

    /// Create a new context with a different current directory.
    pub fn with_current_dir(&self, current_dir: Location) -> VfsResult<Self> {
        current_dir.check_is_dir()?;
        if !self.is_under_root(&current_dir) {
            return Err(VfsError::InvalidInput);
        }
        Ok(Self {
            root_dir: self.root_dir.clone(),
            current_dir,
        })
    }

    /// Resolves a path starting from `current_dir`.
    pub(crate) fn resolve(&self, path: impl AsRef<Path>) -> VfsResult<Location> {
        lookup_location(
            &self.lookup_context(),
            path,
            LookupIntent::Open,
            LookupFlags::follow(),
        )
    }

    /// Resolves a path starting from `current_dir`, without following symlinks.
    pub(crate) fn resolve_no_follow(&self, path: impl AsRef<Path>) -> VfsResult<Location> {
        lookup_location(
            &self.lookup_context(),
            path,
            LookupIntent::Open,
            LookupFlags::no_follow(),
        )
    }

    /// Resolve a path to its parent directory and entry name.
    pub(crate) fn resolve_parent<'a>(&self, path: &'a Path) -> VfsResult<(Location, Cow<'a, str>)> {
        let (dir, name) = lookup_parent(&self.lookup_context(), path, LookupIntent::Open)?;
        Ok((dir, Cow::Owned(name)))
    }

    /// Resolve a path that is expected not to exist.
    pub(crate) fn resolve_nonexistent<'a>(&self, path: &'a Path) -> VfsResult<(Location, &'a str)> {
        lookup_nonexistent(&self.lookup_context(), path, LookupIntent::Open)
    }

    /// Retrieves metadata for the file.
    pub(crate) fn metadata(&self, path: impl AsRef<Path>) -> VfsResult<Metadata> {
        self.resolve(path)?.metadata()
    }

    /// Reads the entire contents of a file into a bytes vector.
    pub(crate) fn read(&self, path: impl AsRef<Path>) -> VfsResult<Vec<u8>> {
        let mut buf = Vec::new();
        let file = File::open(self, path.as_ref())?;
        (&file).read_to_end(&mut buf)?;
        Ok(buf)
    }

    /// Reads the entire contents of a file into a string.
    pub(crate) fn read_to_string(&self, path: impl AsRef<Path>) -> VfsResult<String> {
        String::from_utf8(self.read(path)?).map_err(|_| VfsError::InvalidData)
    }

    /// Writes a slice as the entire contents of a file.
    pub(crate) fn write(&self, path: impl AsRef<Path>, buf: impl AsRef<[u8]>) -> VfsResult<()> {
        let file = File::create(self, path.as_ref())?;
        (&file).write_all(buf.as_ref())?;
        Ok(())
    }

    /// Writes a slice as the entire contents of a file and synchronizes it.
    pub(crate) fn write_sync(
        &self,
        path: impl AsRef<Path>,
        buf: impl AsRef<[u8]>,
    ) -> VfsResult<()> {
        let file = File::create(self, path.as_ref())?;
        (&file).write_all(buf.as_ref())?;
        file.sync(false)
    }

    /// Returns an iterator over the entries in a directory.
    pub(crate) fn read_dir(&self, path: impl AsRef<Path>) -> VfsResult<ReadDir> {
        let dir = self.resolve(path)?;
        Ok(ReadDir {
            dir,
            buf: VecDeque::new(),
            offset: 0,
            ended: false,
        })
    }

    /// Removes a file from the filesystem.
    pub(crate) fn remove_file(&self, path: impl AsRef<Path>) -> VfsResult<()> {
        let entry = self.resolve_no_follow(path.as_ref())?;
        entry
            .parent()
            .ok_or(VfsError::IsADirectory)?
            .unlink(entry.name(), false)
    }

    /// Removes a directory from the filesystem.
    pub(crate) fn remove_dir(&self, path: impl AsRef<Path>) -> VfsResult<()> {
        let entry = self.resolve_no_follow(path.as_ref())?;
        entry
            .parent()
            .ok_or(VfsError::ResourceBusy)?
            .unlink(entry.name(), true)
    }

    /// Renames a file or directory to a new name.
    pub(crate) fn rename(&self, from: impl AsRef<Path>, to: impl AsRef<Path>) -> VfsResult<()> {
        let (src_dir, src_name) =
            lookup_parent(&self.lookup_context(), from.as_ref(), LookupIntent::Open)?;
        let (dst_dir, dst_name) =
            lookup_parent(&self.lookup_context(), to.as_ref(), LookupIntent::Open)?;
        src_dir.rename(&src_name, &dst_dir, &dst_name)
    }

    /// Creates a new, empty directory at the provided path.
    pub(crate) fn create_dir(
        &self,
        path: impl AsRef<Path>,
        mode: NodePermission,
    ) -> VfsResult<Location> {
        let (dir, name) =
            lookup_nonexistent(&self.lookup_context(), path.as_ref(), LookupIntent::Open)?;
        dir.create(name, NodeType::Directory, mode)
    }

    /// Creates a new hard link on the filesystem.
    pub(crate) fn link(
        &self,
        old_path: impl AsRef<Path>,
        new_path: impl AsRef<Path>,
    ) -> VfsResult<Location> {
        let old = self.resolve(old_path.as_ref())?;
        let (new_dir, new_name) = lookup_nonexistent(
            &self.lookup_context(),
            new_path.as_ref(),
            LookupIntent::Open,
        )?;
        new_dir.link(new_name, &old)
    }

    /// Creates a new symbolic link on the filesystem.
    pub(crate) fn symlink(
        &self,
        target: impl AsRef<str>,
        link_path: impl AsRef<Path>,
    ) -> VfsResult<Location> {
        let (dir, name) = lookup_nonexistent(
            &self.lookup_context(),
            link_path.as_ref(),
            LookupIntent::Open,
        )?;
        if dir.lookup_no_follow(name).is_ok() {
            return Err(VfsError::AlreadyExists);
        }

        let symlink = dir.create(name, NodeType::Symlink, NodePermission::default())?;
        if let Err(err) = symlink.entry().as_file()?.set_symlink(target.as_ref()) {
            error!(
                "symlink: set_symlink failed target={} link_name={} fs={} err={:?}",
                target.as_ref(),
                name,
                dir.super_block().name(),
                err
            );
            return Err(err);
        }
        Ok(symlink)
    }

    /// Returns the canonical, absolute form of a path.
    pub(crate) fn canonicalize(&self, path: impl AsRef<Path>) -> VfsResult<PathBuf> {
        let loc = self.resolve(path.as_ref())?;
        self.path_from_root(&loc)
    }
}

/// Iterator returned by [`FsContext::read_dir`].
pub struct ReadDir {
    pub(crate) dir: Location,
    pub(crate) buf: VecDeque<ReadDirEntry>,
    pub(crate) offset: u64,
    pub(crate) ended: bool,
}

impl ReadDir {
    // TODO: tune this
    /// Read-ahead buffer size for directory entries.
    pub const BUF_SIZE: usize = 128;
}

impl Iterator for ReadDir {
    type Item = VfsResult<ReadDirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.ended {
            return None;
        }

        if self.buf.is_empty() {
            self.buf.clear();
            let result = self.dir.read_dir(
                self.offset,
                &mut |name: &str, ino: u64, node_type: NodeType, offset: u64| {
                    self.buf.push_back(ReadDirEntry {
                        name: name.to_owned(),
                        ino,
                        node_type,
                        offset,
                    });
                    self.offset = offset;
                    self.buf.len() < Self::BUF_SIZE
                },
            );

            // We dispatch_irq errors only if we didn't get any entries
            if self.buf.is_empty() {
                if let Err(err) = result {
                    return Some(Err(err));
                }
                self.ended = true;
                return None;
            }
        }

        self.buf.pop_front().map(Ok)
    }
}

//! Filesystem operations module
//!
//! Combines PathResolver and WorkingContext to provide high-level filesystem operations.

use alloc::{collections::vec_deque::VecDeque, string::String, vec::Vec};

use fs_ng_vfs::{
    Location, Metadata, NodePermission, NodeType, VfsResult,
    path::{Path, PathBuf},
};
use kio::{Read, Write};

use crate::{File, PathResolver, ReadDir, WorkingContext};

/// Filesystem operations - combines path resolution and working context
///
/// This structure provides a high-level interface for filesystem operations,
/// delegating path resolution to `PathResolver` and state management to `WorkingContext`.
pub struct FsOperations {
    context: WorkingContext,
    resolver: PathResolver,
}

impl FsOperations {
    /// Creates a new filesystem operations context with the given root
    #[inline]
    pub fn new(root: Location) -> Self {
        Self {
            context: WorkingContext::new(root),
            resolver: PathResolver::new(),
        }
    }

    /// Creates filesystem operations with a custom working context
    #[inline]
    pub fn with_context(context: WorkingContext) -> Self {
        Self {
            context,
            resolver: PathResolver::new(),
        }
    }

    // ========== Context Access ==========

    /// Returns a reference to the root directory
    #[inline]
    pub fn root_dir(&self) -> &Location {
        self.context.root()
    }

    /// Returns a reference to the current working directory
    #[inline]
    pub fn current_dir(&self) -> &Location {
        self.context.cwd()
    }

    /// Changes the current working directory
    #[inline]
    pub fn set_current_dir(&mut self, dir: Location) -> VfsResult<()> {
        self.context.chdir(dir)
    }

    /// Creates a new context with a different current working directory
    #[inline]
    pub fn with_current_dir(&self, current_dir: Location) -> VfsResult<Self> {
        Ok(Self {
            context: self.context.with_cwd(current_dir)?,
            resolver: self.resolver.clone(),
        })
    }

    // ========== Path Resolution ==========

    /// Resolves a path starting from current_dir
    #[inline]
    pub fn resolve(&self, path: impl AsRef<Path>) -> VfsResult<Location> {
        self.resolver
            .resolve(self.context.cwd(), path.as_ref(), true)
    }

    /// Resolves a path without following symlinks
    #[inline]
    pub fn resolve_no_follow(&self, path: impl AsRef<Path>) -> VfsResult<Location> {
        self.resolver
            .resolve(self.context.cwd(), path.as_ref(), false)
    }

    // ========== File Operations ==========

    /// Retrieves metadata for the file
    pub fn metadata(&self, path: impl AsRef<Path>) -> VfsResult<Metadata> {
        self.resolve(path)?.metadata()
    }

    /// Reads the entire contents of a file into a bytes vector
    pub fn read(&self, path: impl AsRef<Path>) -> VfsResult<Vec<u8>> {
        let mut buf = Vec::new();
        // Create a temporary FsContext wrapper for File::open
        let ctx = crate::FsContext {
            inner: self.clone(),
        };
        let file = File::open(&ctx, path.as_ref())?;
        (&file).read_to_end(&mut buf)?;
        Ok(buf)
    }

    /// Reads the entire contents of a file into a string
    pub fn read_to_string(&self, path: impl AsRef<Path>) -> VfsResult<String> {
        String::from_utf8(self.read(path)?).map_err(|_| fs_ng_vfs::VfsError::InvalidData)
    }

    /// Writes a slice as the entire contents of a file
    pub fn write(&self, path: impl AsRef<Path>, buf: impl AsRef<[u8]>) -> VfsResult<()> {
        // Create a temporary FsContext wrapper for File::create
        let ctx = crate::FsContext {
            inner: self.clone(),
        };
        let file = File::create(&ctx, path.as_ref())?;
        (&file).write_all(buf.as_ref())?;
        Ok(())
    }

    /// Returns an iterator over the entries in a directory
    pub fn read_dir(&self, path: impl AsRef<Path>) -> VfsResult<ReadDir> {
        let dir = self.resolve(path)?;
        Ok(ReadDir {
            dir,
            buf: VecDeque::new(),
            offset: 0,
            ended: false,
        })
    }

    // ========== Directory Operations ==========

    /// Removes a file from the filesystem
    pub fn remove_file(&self, path: impl AsRef<Path>) -> VfsResult<()> {
        let entry = self.resolve_no_follow(path.as_ref())?;
        entry
            .parent()
            .ok_or(fs_ng_vfs::VfsError::IsADirectory)?
            .unlink(entry.name(), false)
    }

    /// Removes a directory from the filesystem
    pub fn remove_dir(&self, path: impl AsRef<Path>) -> VfsResult<()> {
        let entry = self.resolve_no_follow(path.as_ref())?;
        entry
            .parent()
            .ok_or(fs_ng_vfs::VfsError::ResourceBusy)?
            .unlink(entry.name(), true)
    }

    /// Renames a file or directory to a new name
    pub fn rename(&self, from: impl AsRef<Path>, to: impl AsRef<Path>) -> VfsResult<()> {
        let (src_dir, src_name) = self
            .resolver
            .resolve_parent(self.context.cwd(), from.as_ref())?;
        let (dst_dir, dst_name) = self
            .resolver
            .resolve_parent(self.context.cwd(), to.as_ref())?;
        src_dir.rename(&src_name, &dst_dir, &dst_name)
    }

    /// Creates a new, empty directory at the provided path
    pub fn create_dir(&self, path: impl AsRef<Path>, mode: NodePermission) -> VfsResult<Location> {
        let (dir, name) = self
            .resolver
            .resolve_nonexistent(self.context.cwd(), path.as_ref())?;
        dir.create(name, NodeType::Directory, mode)
    }

    /// Creates a new hard link on the filesystem
    pub fn link(
        &self,
        old_path: impl AsRef<Path>,
        new_path: impl AsRef<Path>,
    ) -> VfsResult<Location> {
        let old = self.resolve(old_path.as_ref())?;
        let (new_dir, new_name) = self
            .resolver
            .resolve_nonexistent(self.context.cwd(), new_path.as_ref())?;
        new_dir.link(new_name, &old)
    }

    /// Creates a new symbolic link on the filesystem
    pub fn symlink(
        &self,
        target: impl AsRef<str>,
        link_path: impl AsRef<Path>,
    ) -> VfsResult<Location> {
        let (dir, name) = self
            .resolver
            .resolve_nonexistent(self.context.cwd(), link_path.as_ref())?;
        if dir.lookup_no_follow(name).is_ok() {
            return Err(fs_ng_vfs::VfsError::AlreadyExists);
        }
        let symlink = dir.create(name, NodeType::Symlink, NodePermission::default())?;
        symlink.entry().as_file()?.set_symlink(target.as_ref())?;
        Ok(symlink)
    }

    /// Returns the canonical, absolute form of a path
    pub fn canonicalize(&self, path: impl AsRef<Path>) -> VfsResult<PathBuf> {
        self.resolve(path.as_ref())?.absolute_path()
    }
}

// Backward compatibility: FsContext is an alias for FsOperations
#[allow(dead_code)]
pub type FsContext = FsOperations;

impl Clone for FsOperations {
    fn clone(&self) -> Self {
        Self {
            context: self.context.clone(),
            resolver: self.resolver.clone(),
        }
    }
}

impl core::fmt::Debug for FsOperations {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FsOperations")
            .field("context", &self.context)
            .field("resolver", &self.resolver)
            .finish()
    }
}

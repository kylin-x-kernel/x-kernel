use alloc::{
    borrow::{Cow, ToOwned},
    collections::vec_deque::VecDeque,
    string::String,
    sync::Arc,
    vec::Vec,
};

use fs_ng_vfs::{
    Location, Metadata, NodePermission, NodeType, VfsError, VfsResult,
    path::{Path, PathBuf},
};
use ksync::Mutex;
use spin::Once;

use crate::PathResolver;

#[allow(dead_code)]
pub const SYMLINKS_MAX: usize = 40;

// Import the new FsOperations as the implementation
use crate::fs_operations::FsOperations as FsContextImpl;

pub static ROOT_FS_CONTEXT: Once<FsContext> = Once::new();

scope_local::scope_local! {
    pub static FS_CONTEXT: Arc<Mutex<FsContext>> =
        Arc::new(Mutex::new(
            ROOT_FS_CONTEXT
                .get()
                .expect("Root FS context not initialized")
                .clone(),
        ));
}

pub struct ReadDirEntry {
    pub name: String,
    pub ino: u64,
    pub node_type: NodeType,
    pub offset: u64,
}

/// Provides `std::fs`-like interface.
///
/// This is now a wrapper around the refactored components for backward compatibility.
#[derive(Debug, Clone)]
pub struct FsContext {
    pub(crate) inner: FsContextImpl,
}

impl FsContext {
    pub fn new(root_dir: Location) -> Self {
        Self {
            inner: FsContextImpl::new(root_dir),
        }
    }

    /// Creates a FsContext from FsOperations (internal use)
    #[doc(hidden)]
    pub fn from_ops(ops: FsContextImpl) -> Self {
        Self { inner: ops }
    }

    pub fn root_dir(&self) -> &Location {
        self.inner.root_dir()
    }

    pub fn current_dir(&self) -> &Location {
        self.inner.current_dir()
    }

    pub fn set_current_dir(&mut self, current_dir: Location) -> VfsResult<()> {
        self.inner.set_current_dir(current_dir)
    }

    pub fn with_current_dir(&self, current_dir: Location) -> VfsResult<Self> {
        Ok(Self {
            inner: self.inner.with_current_dir(current_dir)?,
        })
    }

    /// Resolves a path starting from `current_dir`.
    pub fn resolve(&self, path: impl AsRef<Path>) -> VfsResult<Location> {
        self.inner.resolve(path)
    }

    /// Resolves a path starting from `current_dir`, without following symlinks.
    pub fn resolve_no_follow(&self, path: impl AsRef<Path>) -> VfsResult<Location> {
        self.inner.resolve_no_follow(path)
    }

    /// Resolves a path to its parent directory and entry name
    pub fn resolve_parent<'a>(&self, path: &'a Path) -> VfsResult<(Location, Cow<'a, str>)> {
        // Use inner resolver but convert String to Cow
        let resolver = PathResolver::new();
        let (dir, name) = resolver.resolve_parent(self.inner.current_dir(), path)?;
        Ok((dir, Cow::Owned(name)))
    }

    /// Resolves a path for a nonexistent entry
    pub fn resolve_nonexistent<'a>(&self, path: &'a Path) -> VfsResult<(Location, &'a str)> {
        // This method returns a lifetime-bound &str, so we need to use the path's lifetime
        // We can only return a reference to something in the path itself
        let entry_name = path.file_name().ok_or(VfsError::InvalidInput)?;
        let mut components = path.components();
        components.next_back();

        let resolver = PathResolver::new();
        let dir =
            resolver.resolve_components_internal(self.inner.current_dir(), components, &mut 0)?;
        dir.check_is_dir()?;
        Ok((dir, entry_name))
    }

    /// Retrieves metadata for the file.
    pub fn metadata(&self, path: impl AsRef<Path>) -> VfsResult<Metadata> {
        self.inner.metadata(path)
    }

    /// Reads the entire contents of a file into a bytes vector.
    pub fn read(&self, path: impl AsRef<Path>) -> VfsResult<Vec<u8>> {
        self.inner.read(path)
    }

    /// Reads the entire contents of a file into a string.
    pub fn read_to_string(&self, path: impl AsRef<Path>) -> VfsResult<String> {
        self.inner.read_to_string(path)
    }

    /// Writes a slice as the entire contents of a file.
    pub fn write(&self, path: impl AsRef<Path>, buf: impl AsRef<[u8]>) -> VfsResult<()> {
        self.inner.write(path, buf)
    }

    /// Returns an iterator over the entries in a directory.
    pub fn read_dir(&self, path: impl AsRef<Path>) -> VfsResult<ReadDir> {
        self.inner.read_dir(path)
    }

    /// Removes a file from the filesystem.
    pub fn remove_file(&self, path: impl AsRef<Path>) -> VfsResult<()> {
        self.inner.remove_file(path)
    }

    /// Removes a directory from the filesystem.
    pub fn remove_dir(&self, path: impl AsRef<Path>) -> VfsResult<()> {
        self.inner.remove_dir(path)
    }

    /// Renames a file or directory to a new name.
    pub fn rename(&self, from: impl AsRef<Path>, to: impl AsRef<Path>) -> VfsResult<()> {
        self.inner.rename(from, to)
    }

    /// Creates a new, empty directory at the provided path.
    pub fn create_dir(&self, path: impl AsRef<Path>, mode: NodePermission) -> VfsResult<Location> {
        self.inner.create_dir(path, mode)
    }

    /// Creates a new hard link on the filesystem.
    pub fn link(
        &self,
        old_path: impl AsRef<Path>,
        new_path: impl AsRef<Path>,
    ) -> VfsResult<Location> {
        self.inner.link(old_path, new_path)
    }

    /// Creates a new symbolic link on the filesystem.
    pub fn symlink(
        &self,
        target: impl AsRef<str>,
        link_path: impl AsRef<Path>,
    ) -> VfsResult<Location> {
        self.inner.symlink(target, link_path)
    }

    /// Returns the canonical, absolute form of a path.
    pub fn canonicalize(&self, path: impl AsRef<Path>) -> VfsResult<PathBuf> {
        self.inner.canonicalize(path)
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

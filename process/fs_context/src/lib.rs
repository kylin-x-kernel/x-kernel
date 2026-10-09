// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process filesystem state: root, working directory, umask, and exec flag.
//!
//! [`FsStruct`] is the process-owned counterpart of Linux `fs_struct`;
//! [`init_fs`] returns its initial shared instance. Path resolution and mount
//! transactions belong to KVFS. Attach mounted paths before using root/pwd readers.
//!
//! # Example
//!
//! ```
//! use fs_context::FsStruct;
//! let mut fs = FsStruct::for_init_task();
//! assert_eq!(fs.replace_umask(0o077), 0o022);
//! fs.set_in_exec(true);
//! let child = fs.clone_for_process();
//! assert_eq!(child.umask(), 0o077);
//! assert!(!child.in_exec());
//! ```

#![cfg_attr(any(not(test), doc), no_std)]

extern crate alloc;

use alloc::sync::Arc;

use klazy::Lazy;
use ksync::Mutex;
use kvfs::{NodePermission, Path, VfsError, VfsResult};

const UMASK_BITS: u32 = 0o777;

static INIT_FS: Lazy<Arc<Mutex<FsStruct>>> =
    Lazy::new(|| Arc::new(Mutex::new(FsStruct::for_init_task())));

/// Returns the initial task filesystem context.
pub fn init_fs() -> Arc<Mutex<FsStruct>> {
    Arc::clone(&*INIT_FS)
}

/// Allocates a process-private copy of the initial filesystem context.
pub fn copy_init_fs_struct() -> Arc<Mutex<FsStruct>> {
    Arc::new(Mutex::new(init_fs().lock().clone_for_process()))
}

/// The process filesystem view (`fs_struct`).
///
/// This mirrors Linux `struct fs_struct`. Rust `Option` values represent the
/// zero-initialized `root` and `pwd` paths used by Linux's static `init_fs`
/// before the initial mount tree is installed.
#[derive(Debug)]
pub struct FsStruct {
    umask: u32,
    in_exec: bool,
    root: Option<Path>,
    pwd: Option<Path>,
}

impl FsStruct {
    /// Creates the static init-task filesystem context.
    pub const fn for_init_task() -> Self {
        Self {
            umask: 0o022,
            in_exec: false,
            root: None,
            pwd: None,
        }
    }

    /// Creates a mounted filesystem context with root as both root and pwd.
    ///
    /// # Panics
    ///
    /// Panics if `root` is not a directory. Use [`Self::from_root_and_pwd`]
    /// when the caller needs a recoverable error.
    pub fn new(root: Path) -> Self {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: The infallible constructor requires a \
                      directory root; fallible callers use from_root_and_pwd"
        )]
        Self::from_root_and_pwd(root.clone(), root).expect("initial root must be a directory")
    }

    /// Creates a mounted filesystem context from explicit root and pwd.
    ///
    /// # Errors
    ///
    /// Returns `VfsError::NotADirectory` if any supplied path is not a directory.
    /// No path fields are changed on error.
    pub fn from_root_and_pwd(root: Path, pwd: Path) -> VfsResult<Self> {
        Self::require_directory(&root)?;
        Self::require_directory(&pwd)?;
        Ok(Self {
            umask: 0o022,
            in_exec: false,
            root: Some(root),
            pwd: Some(pwd),
        })
    }

    /// Clones this context for a process that does not share `CLONE_FS`.
    pub fn clone_for_process(&self) -> Self {
        let mut clone = self.snapshot();
        clone.in_exec = false;
        clone
    }

    /// Takes a complete snapshot of this filesystem context.
    pub fn snapshot(&self) -> Self {
        Self {
            umask: self.umask,
            in_exec: self.in_exec,
            root: self.root.clone(),
            pwd: self.pwd.clone(),
        }
    }

    /// Attaches the first mounted root to this context.
    ///
    /// # Errors
    ///
    /// Returns `VfsError::NotADirectory` if any supplied path is not a directory.
    /// No path fields are changed on error.
    ///
    /// # Example
    ///
    /// The caller supplies a resolved, authorized directory from its mounted tree.
    /// After attachment, the initialized path readers are available:
    ///
    /// ```no_run
    /// # fn mount_context(root: kvfs::Path) -> kvfs::VfsResult<()> {
    /// let mut fs = fs_context::FsStruct::for_init_task();
    /// fs.attach_root(root)?;
    /// let (root, pwd) = fs.root_and_pwd();
    /// assert!(root.is_dir() && pwd.is_dir());
    /// # Ok(())
    /// # }
    /// ```
    pub fn attach_root(&mut self, root: Path) -> VfsResult<()> {
        Self::require_directory(&root)?;
        self.root = Some(root.clone());
        self.pwd = Some(root);
        Ok(())
    }

    /// Returns this context's root path.
    ///
    /// # Panics
    ///
    /// Panics if the root has not been initialized.
    /// Install paths with [`Self::attach_root`] before calling this method.
    ///
    /// See [`Self::attach_root`] for a complete initialization sequence.
    pub fn root(&self) -> &Path {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Mounted-context access requires \
                      attach_root to have installed root and pwd"
        )]
        self.root.as_ref().expect("fs root not initialized")
    }

    /// Returns this context's current working directory.
    ///
    /// # Panics
    ///
    /// Panics if the working directory has not been initialized.
    /// Install paths with [`Self::attach_root`] before calling this method.
    ///
    /// See [`Self::attach_root`] for a complete initialization sequence.
    pub fn pwd(&self) -> &Path {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Mounted-context access requires \
                      attach_root to have installed root and pwd"
        )]
        self.pwd.as_ref().expect("fs pwd not initialized")
    }

    /// Returns root and pwd as a stable snapshot.
    ///
    /// # Panics
    ///
    /// Panics if the root or working directory has not been initialized.
    /// Install paths with [`Self::attach_root`] before calling this method.
    ///
    /// See [`Self::attach_root`] for a complete initialization sequence.
    pub fn root_and_pwd(&self) -> (Path, Path) {
        (self.root().clone(), self.pwd().clone())
    }

    /// Returns the file creation mask.
    pub const fn umask(&self) -> u32 {
        self.umask
    }

    /// Returns the file creation mask in the VFS permission representation.
    pub const fn node_umask(&self) -> NodePermission {
        NodePermission::from_bits_truncate(self.umask as u16)
    }

    /// Replaces the file creation mask and returns the previous value.
    pub fn replace_umask(&mut self, umask: u32) -> u32 {
        core::mem::replace(&mut self.umask, umask & UMASK_BITS)
    }

    /// Returns whether this context is in exec transition.
    pub const fn in_exec(&self) -> bool {
        self.in_exec
    }

    /// Sets exec transition state.
    pub fn set_in_exec(&mut self, in_exec: bool) {
        self.in_exec = in_exec;
    }

    /// Changes this context's root.
    ///
    /// # Errors
    ///
    /// Returns `VfsError::NotADirectory` if any supplied path is not a directory.
    /// No path fields are changed on error.
    pub fn set_root(&mut self, root: Path) -> VfsResult<()> {
        Self::require_directory(&root)?;
        if self.pwd.is_none() {
            self.pwd = Some(root.clone());
        }
        self.root = Some(root);
        Ok(())
    }

    /// Changes this context's current working directory.
    ///
    /// # Errors
    ///
    /// Returns `VfsError::NotADirectory` for a non-directory path, or
    /// `VfsError::InvalidInput` if root has not been initialized. State is unchanged.
    ///
    /// See [`Self::attach_root`] for a complete initialization sequence.
    pub fn set_pwd(&mut self, pwd: Path) -> VfsResult<()> {
        Self::require_directory(&pwd)?;
        if self.root.is_none() {
            return Err(VfsError::InvalidInput);
        }
        self.pwd = Some(pwd);
        Ok(())
    }

    /// Replaces root and current working directory in one validated update.
    ///
    /// # Errors
    ///
    /// Returns `VfsError::NotADirectory` if any supplied path is not a directory.
    /// No path fields are changed on error.
    pub fn replace_root_and_pwd(&mut self, root: Path, pwd: Path) -> VfsResult<()> {
        Self::require_directory(&root)?;
        Self::require_directory(&pwd)?;
        self.root = Some(root);
        self.pwd = Some(pwd);
        Ok(())
    }

    /// Clones this context and replaces pwd in the clone.
    ///
    /// # Errors
    ///
    /// Propagates the directory and initialization errors from [`Self::set_pwd`].
    /// The original context is unchanged.
    pub fn clone_with_pwd(&self, pwd: Path) -> VfsResult<Self> {
        let mut fs = self.snapshot();
        fs.set_pwd(pwd)?;
        Ok(fs)
    }

    fn require_directory(path: &Path) -> VfsResult<()> {
        if path.is_dir() {
            Ok(())
        } else {
            Err(VfsError::NotADirectory)
        }
    }
}

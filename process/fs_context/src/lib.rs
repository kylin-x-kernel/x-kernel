// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process filesystem context state.
//!
//! This crate owns the Rust counterpart of Linux `struct fs_struct`: root,
//! current working directory, file creation mask, and exec transition state.

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

#[derive(Clone, Debug)]
enum FsLocation {
    Unmounted,
    Mounted { root: Path, pwd: Path },
}

/// Process filesystem context.
///
/// This mirrors Linux `struct fs_struct`, but keeps the pre-rootfs state
/// explicit instead of representing unmounted root and pwd as unrelated
/// `Option` fields. It stores already resolved root and current-directory
/// paths; lookup and chroot handling enforce path-escape rules.
#[derive(Debug)]
pub struct FsStruct {
    umask: u32,
    in_exec: bool,
    location: FsLocation,
}

impl FsStruct {
    /// Creates the static init-task filesystem context.
    pub const fn for_init_task() -> Self {
        Self {
            umask: 0o022,
            in_exec: false,
            location: FsLocation::Unmounted,
        }
    }

    /// Creates a mounted filesystem context with root as both root and pwd.
    pub fn new(root: Path) -> Self {
        Self::from_root_and_pwd(root.clone(), root).expect("initial root must be a directory")
    }

    /// Creates a mounted filesystem context from explicit root and pwd.
    pub fn from_root_and_pwd(root: Path, pwd: Path) -> VfsResult<Self> {
        Self::require_directory(&root)?;
        Self::require_directory(&pwd)?;
        Ok(Self {
            umask: 0o022,
            in_exec: false,
            location: FsLocation::Mounted { root, pwd },
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
            location: self.location.clone(),
        }
    }

    /// Attaches the first mounted root to this context.
    pub fn attach_root(&mut self, root: Path) -> VfsResult<()> {
        Self::require_directory(&root)?;
        self.location = FsLocation::Mounted {
            root: root.clone(),
            pwd: root,
        };
        Ok(())
    }

    /// Returns this context's root path.
    pub fn root(&self) -> &Path {
        match &self.location {
            FsLocation::Mounted { root, .. } => root,
            FsLocation::Unmounted => panic!("fs root not initialized"),
        }
    }

    /// Returns this context's current working directory.
    pub fn pwd(&self) -> &Path {
        match &self.location {
            FsLocation::Mounted { pwd, .. } => pwd,
            FsLocation::Unmounted => panic!("fs pwd not initialized"),
        }
    }

    /// Returns root and pwd as a stable snapshot.
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
    pub fn set_root(&mut self, root: Path) -> VfsResult<()> {
        Self::require_directory(&root)?;
        match &mut self.location {
            FsLocation::Mounted { root: slot, .. } => *slot = root,
            FsLocation::Unmounted => {
                self.location = FsLocation::Mounted {
                    root: root.clone(),
                    pwd: root,
                };
            }
        }
        Ok(())
    }

    /// Changes this context's current working directory.
    pub fn set_pwd(&mut self, pwd: Path) -> VfsResult<()> {
        Self::require_directory(&pwd)?;
        match &mut self.location {
            FsLocation::Mounted { pwd: slot, .. } => *slot = pwd,
            FsLocation::Unmounted => return Err(VfsError::InvalidInput),
        }
        Ok(())
    }

    /// Replaces root and current working directory in one validated update.
    pub fn replace_root_and_pwd(&mut self, root: Path, pwd: Path) -> VfsResult<()> {
        Self::require_directory(&root)?;
        Self::require_directory(&pwd)?;
        self.location = FsLocation::Mounted { root, pwd };
        Ok(())
    }

    /// Clones this context and replaces pwd in the clone.
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

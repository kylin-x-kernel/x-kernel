// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Open-file objects.
//!
//! Linux keeps per-open state in `struct file`, separate from inode identity
//! and directory-entry name bindings. `VfsFile` is the VFS-owned core of that
//! object: it owns the opened location, access mode, file offset, raw open
//! flags, and file-private attachment storage.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::{Location, Mutex, MutexGuard, NodeFlags, TypeMap, VfsError, VfsResult};

bitflags::bitflags! {
    /// Access mode flags for an opened VFS file.
    #[derive(Debug, Clone, Copy)]
    pub struct VfsFileFlags: u8 {
        /// File may be read.
        const READ = 1;
        /// File may be written.
        const WRITE = 2;
        /// File may be executed.
        const EXECUTE = 4;
        /// Writes append to the current end of file.
        const APPEND = 8;
        /// Path-only file descriptor.
        const PATH = 16;
    }
}

/// VFS-owned open-file state.
pub struct VfsFile {
    location: Location,
    flags: VfsFileFlags,
    position: Option<Mutex<u64>>,
    open_flags: u32,
    nonblock: AtomicBool,
    private_data: Mutex<TypeMap>,
}

impl VfsFile {
    /// Creates a VFS file with no raw open flags.
    pub fn new(location: Location, flags: VfsFileFlags) -> Self {
        Self::with_open_flags(location, flags, 0)
    }

    /// Creates a VFS file with raw user-visible open flags.
    pub fn with_open_flags(location: Location, flags: VfsFileFlags, open_flags: u32) -> Self {
        let position = if location.flags().contains(NodeFlags::STREAM) {
            None
        } else {
            Some(Mutex::new(if flags.contains(VfsFileFlags::APPEND) {
                location.len().unwrap_or_default()
            } else {
                0
            }))
        };

        Self {
            location,
            flags,
            position,
            open_flags,
            nonblock: AtomicBool::new(false),
            private_data: Mutex::default(),
        }
    }

    /// Returns the opened VFS location.
    pub fn location(&self) -> &Location {
        &self.location
    }

    /// Returns this file's access mode flags.
    pub fn flags(&self) -> VfsFileFlags {
        self.flags
    }

    /// Returns whether this file descriptor is path-only.
    pub fn is_path(&self) -> bool {
        self.flags.contains(VfsFileFlags::PATH)
    }

    /// Checks that the file has the requested access mode.
    pub fn access(&self, flags: VfsFileFlags) -> VfsResult<()> {
        if self.flags.contains(flags) && !self.is_path() {
            Ok(())
        } else {
            Err(VfsError::BadFileDescriptor)
        }
    }

    /// Returns whether the underlying node is always blocking.
    ///
    /// This is distinct from the open-file `O_NONBLOCK` state. Linux ignores
    /// `O_NONBLOCK` for regular files because the inode operation itself is
    /// always blocking.
    pub fn is_blocking(&self) -> bool {
        self.location.flags().contains(NodeFlags::BLOCKING)
    }

    /// Locks the current file offset, if this file tracks one.
    pub fn position_lock(&self) -> Option<MutexGuard<'_, u64>> {
        self.position.as_ref().map(Mutex::lock)
    }

    /// Returns the current file offset, if this file tracks one.
    pub fn position(&self) -> Option<u64> {
        self.position.as_ref().map(|position| *position.lock())
    }

    /// Sets the current file offset if this file tracks one.
    pub fn set_position(&self, position: u64) -> VfsResult<()> {
        if let Some(mut current) = self.position_lock() {
            *current = position;
            Ok(())
        } else {
            Err(VfsError::InvalidInput)
        }
    }

    /// Sets the open-file nonblocking flag.
    pub fn set_nonblocking(&self, flag: bool) {
        self.nonblock.store(flag, Ordering::Release);
    }

    /// Returns the open-file nonblocking flag.
    pub fn nonblocking(&self) -> bool {
        self.nonblock.load(Ordering::Acquire)
    }

    /// Returns the raw user-visible open flags.
    pub fn open_flags(&self) -> u32 {
        self.open_flags
    }

    /// Access file-private attachment storage.
    pub fn private_data(&self) -> MutexGuard<'_, TypeMap> {
        self.private_data.lock()
    }
}

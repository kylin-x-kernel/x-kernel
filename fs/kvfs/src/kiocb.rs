// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Synchronous file I/O control blocks.

use iov_iter::{IovIterDest, IovIterSource};

use crate::{OpenFlags, VfsFile, VfsResult};

bitflags::bitflags! {
    /// Per-request I/O flags derived from the open file description.
    ///
    /// Records its `O_DIRECT`/`O_DSYNC`/`O_SYNC` state at control-block
    /// creation, mirroring Linux `iocb_flags()`.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct KiocbFlags: u32 {
        /// The request carries `O_DIRECT` semantics.
        const DIRECT = 1 << 0;
        /// The request requires data-integrity synchronized completion.
        const DSYNC = 1 << 1;
        /// The request requires fully synchronized completion.
        const SYNC = 1 << 2;
    }
}

impl KiocbFlags {
    /// Derives control-block flags from open-file status flags.
    ///
    /// `O_SYNC` also contains `O_DSYNC` in its encoding, so a synchronized
    /// request sets both bits as in Linux `iocb_flags()`.
    ///
    /// - `flags`: the open file's `f_flags`.
    ///
    /// Returns: the matching control-block flags.
    fn from_open_flags(flags: OpenFlags) -> Self {
        let mut kiocb_flags = Self::empty();
        kiocb_flags.set(Self::DIRECT, flags.contains(OpenFlags::DIRECT));
        kiocb_flags.set(Self::DSYNC, flags.contains(OpenFlags::DSYNC));
        kiocb_flags.set(Self::SYNC, flags.contains(OpenFlags::SYNC));
        kiocb_flags
    }
}

/// Synchronous I/O control block.
///
/// Carries the open file description, current operation position, and the
/// open-file completion flags through `read_iter` and `write_iter`.
pub struct Kiocb<'a> {
    file: &'a VfsFile,
    ki_pos: u64,
    flags: KiocbFlags,
}

impl<'a> Kiocb<'a> {
    /// Creates a control block for `file` at `ki_pos`.
    ///
    /// - `file`: the open file description; supplies the per-request flags.
    /// - `ki_pos`: initial operation position in bytes.
    ///
    /// Returns: a control block whose flags are recorded from `file`.
    ///
    /// Locking: reads `f_flags` atomically and acquires no lock.
    pub fn new(file: &'a VfsFile, ki_pos: u64) -> Self {
        let flags = KiocbFlags::from_open_flags(file.flags());
        Self {
            file,
            ki_pos,
            flags,
        }
    }

    /// Returns the control-block flags recorded at creation.
    pub fn flags(&self) -> KiocbFlags {
        self.flags
    }

    /// Returns the open file description.
    pub fn file(&self) -> &'a VfsFile {
        self.file
    }

    /// Returns the current operation position.
    pub fn ki_pos(&self) -> u64 {
        self.ki_pos
    }

    /// Sets the current operation position.
    pub fn set_ki_pos(&mut self, ki_pos: u64) {
        self.ki_pos = ki_pos;
    }

    /// Advances the current operation position.
    pub fn advance(&mut self, len: usize) {
        self.ki_pos = self.ki_pos.saturating_add(len as u64);
    }

    /// Performs the generic buffered read path for this I/O request.
    pub fn generic_file_read_iter(&mut self, iter: &mut IovIterDest<'_>) -> VfsResult<usize> {
        let mapping = self.file().mapping();
        mapping.read_iter(self, iter)
    }

    /// Performs the generic buffered write path for this I/O request.
    pub fn generic_file_write_iter(&mut self, iter: &mut IovIterSource<'_>) -> VfsResult<usize> {
        let mapping = self.file().mapping();
        mapping.write_iter(self, iter)
    }

    /// Performs a generic buffered write with open-file-specific checks.
    ///
    /// `checks` runs after the generic superblock limit check and while the
    /// inode data lock remains held. A filesystem file operation can narrow
    /// `count` before the page-cache write starts without adding policy to
    /// [`crate::AddressSpaceOperations`].
    pub fn generic_file_write_iter_with_checks(
        &mut self,
        iter: &mut IovIterSource<'_>,
        checks: impl FnOnce(u64, &mut usize) -> VfsResult<()>,
    ) -> VfsResult<usize> {
        let mapping = self.file().mapping();
        mapping.write_iter_with_checks(self, iter, checks)
    }
}

unittest::path_mod_test!(tests, "../ut-tests/kiocb.rs");

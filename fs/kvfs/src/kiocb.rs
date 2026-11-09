// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Synchronous file I/O control blocks.

use iov_iter::{IovIterDest, IovIterSource};

use crate::{OpenFlags, VfsError, VfsFile, VfsResult};

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

    /// Applies the generic file-size limit to a buffered write count.
    ///
    /// Call while the inode data lock excludes competing changes to this write.
    /// An empty request is accepted without checking the file-size limit.
    ///
    /// # Errors
    /// Returns a file-size or range error if the request cannot be represented.
    pub fn generic_write_check_limits(&self, count: &mut usize) -> VfsResult<()> {
        if *count == 0 {
            return Ok(());
        }
        let max_file_size = self.file().max_file_size();
        let pos = self.ki_pos();
        if pos >= max_file_size {
            return Err(VfsError::FileTooLarge);
        }

        let count_u64 = u64::try_from(*count).map_err(|_| VfsError::InvalidInput)?;
        let count_u64 = count_u64.min(max_file_size - pos);
        *count = usize::try_from(count_u64).map_err(|_| VfsError::InvalidInput)?;
        Ok(())
    }

    /// Performs a generic buffered write with open-file-specific checks.
    ///
    /// `checks` runs with the inode data lock held and must apply
    /// [`Self::generic_write_check_limits`] along with filesystem policy before
    /// the page-cache write starts. It also runs for an empty request.
    pub fn generic_file_write_iter_with_checks(
        &mut self,
        iter: &mut IovIterSource<'_>,
        checks: impl FnOnce(&Kiocb<'_>, &mut usize) -> VfsResult<()>,
    ) -> VfsResult<usize> {
        let mapping = self.file().mapping();
        mapping.write_iter_with_checks(self, iter, checks)
    }
}

unittest::path_mod_test!(tests, "../ut-tests/kiocb.rs");

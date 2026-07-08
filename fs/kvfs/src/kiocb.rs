// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Synchronous file I/O control blocks.

use iov_iter::{IovIterDest, IovIterSource};

use crate::{VfsFile, VfsResult};

/// Synchronous I/O control block.
///
/// Carries the open file description and current operation position through
/// `read_iter` and `write_iter`.
pub struct Kiocb<'a> {
    file: &'a VfsFile,
    ki_pos: u64,
}

impl<'a> Kiocb<'a> {
    /// Creates a control block for `file` at `ki_pos`.
    pub fn new(file: &'a VfsFile, ki_pos: u64) -> Self {
        Self { file, ki_pos }
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
}

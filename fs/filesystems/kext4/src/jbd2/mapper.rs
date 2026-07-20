// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::fmt;

use crate::{Ext4Result, FilesystemBlock};

/// A logical block in a JBD2 journal.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct JournalBlock(u32);

impl JournalBlock {
    /// Creates a journal block number.
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Returns the numeric journal block number.
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for JournalBlock {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// A filesystem metadata block referenced by a JBD2 descriptor or revoke tag.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct JournalTargetBlock(u64);

impl JournalTargetBlock {
    /// Creates a journal target block number.
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the numeric target block number.
    pub(crate) const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for JournalTargetBlock {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// A JBD2 transaction sequence number.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TransactionId(u32);

impl TransactionId {
    /// Creates a transaction identifier.
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Returns the numeric transaction identifier.
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Maps JBD2 logical blocks onto filesystem physical blocks.
pub(crate) trait JournalBlockMapper {
    /// Resolves one journal block.
    fn map_journal_block(&self, block: JournalBlock) -> Ext4Result<FilesystemBlock>;
}

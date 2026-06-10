// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::fmt;

/// A block number in the ext4 filesystem block address space.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FilesystemBlock(u64);

impl FilesystemBlock {
    /// Creates a filesystem block number.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the numeric block number.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for FilesystemBlock {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// A zero-based ext4 block group number.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct BlockGroupNumber(u32);

impl BlockGroupNumber {
    /// Creates a block group number.
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Returns the numeric group number.
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for BlockGroupNumber {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

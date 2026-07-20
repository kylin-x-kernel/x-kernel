// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::fmt;

/// A block number in the ext4 filesystem block address space.
#[repr(transparent)]
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
#[repr(transparent)]
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

/// An ext4 inode number.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct InodeNumber(u32);

impl InodeNumber {
    /// Creates an inode number.
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Returns the numeric inode number.
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for InodeNumber {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// A logical block number within one inode.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct LogicalBlock(u64);

impl LogicalBlock {
    /// Creates a logical block number.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the numeric logical block number.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for LogicalBlock {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// A physical block number in the ext4 filesystem block address space.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PhysicalBlock(u64);

impl PhysicalBlock {
    /// Creates a physical block number.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the numeric physical block number.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for PhysicalBlock {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// A count of contiguous filesystem blocks.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct BlockCount(u32);

impl BlockCount {
    /// Creates a block count.
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Returns the numeric block count.
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for BlockCount {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::fmt;

use block::DriverError;

/// Result type used by KExt4 storage operations.
pub type Ext4Result<T> = core::result::Result<T, Ext4Error>;

/// Feature namespace containing an unsupported bit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FeatureClass {
    /// The incompatible feature bitmap.
    Incompatible,
    /// The read-only-compatible feature bitmap.
    ReadOnlyCompatible,
}

/// Metadata structure protected by a checksum.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChecksumTarget {
    /// The primary ext4 superblock.
    Superblock,
    /// A block group descriptor.
    BlockGroup(u32),
}

/// Classifies malformed ext4 metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CorruptKind {
    /// The input does not contain the complete structure.
    Truncated,
    /// A required count or size is zero.
    ZeroGeometry,
    /// The block size is outside the supported ext4 range.
    InvalidBlockSize,
    /// The inode size is invalid for the selected block size.
    InvalidInodeSize,
    /// The inode count or per-group inode geometry is invalid.
    InvalidInodeGeometry,
    /// The group descriptor size is invalid.
    InvalidDescriptorSize,
    /// Cluster geometry is inconsistent without `bigalloc`.
    InvalidClusterGeometry,
    /// Block group geometry is inconsistent with the filesystem size.
    InvalidBlockGroupGeometry,
    /// The flex block group geometry cannot be represented.
    InvalidFlexGeometry,
    /// A metadata block lies outside its permitted block group.
    MetadataOutsideGroup,
}

/// Errors returned while decoding or mounting an ext4 filesystem.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ext4Error {
    /// The block driver rejected an I/O operation.
    Device(DriverError),
    /// An offset or length calculation overflowed.
    Overflow,
    /// A caller-provided buffer has the wrong length.
    InvalidBufferLength { expected: usize, actual: usize },
    /// The underlying device block geometry is unusable.
    InvalidDeviceBlockSize(usize),
    /// An I/O range exceeds the device or filesystem.
    OutOfBounds,
    /// The superblock magic is not `0xEF53`.
    InvalidMagic(u16),
    /// The filesystem revision is not supported.
    UnsupportedRevision(u32),
    /// A feature changes a layout that this stage cannot interpret.
    UnsupportedFeature { class: FeatureClass, bits: u32 },
    /// The filesystem requires journal recovery before it can be exposed.
    NeedsRecovery,
    /// A protected metadata structure failed checksum verification.
    ChecksumMismatch {
        target: ChecksumTarget,
        expected: u32,
        actual: u32,
    },
    /// On-disk metadata violates a structural invariant.
    Corrupt(CorruptKind),
}

impl From<DriverError> for Ext4Error {
    fn from(error: DriverError) -> Self {
        Self::Device(error)
    }
}

impl fmt::Display for Ext4Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Device(error) => write!(formatter, "block device error: {error}"),
            Self::Overflow => formatter.write_str("ext4 arithmetic overflow"),
            Self::InvalidBufferLength { expected, actual } => {
                write!(
                    formatter,
                    "invalid buffer length: expected {expected}, got {actual}"
                )
            }
            Self::InvalidDeviceBlockSize(size) => {
                write!(formatter, "invalid device block size: {size}")
            }
            Self::OutOfBounds => formatter.write_str("ext4 I/O range is out of bounds"),
            Self::InvalidMagic(magic) => write!(formatter, "invalid ext4 magic: {magic:#06x}"),
            Self::UnsupportedRevision(revision) => {
                write!(formatter, "unsupported ext4 revision: {revision}")
            }
            Self::UnsupportedFeature { class, bits } => {
                write!(
                    formatter,
                    "unsupported {class:?} feature bits: {bits:#010x}"
                )
            }
            Self::NeedsRecovery => formatter.write_str("ext4 journal recovery is required"),
            Self::ChecksumMismatch {
                target,
                expected,
                actual,
            } => write!(
                formatter,
                "{target:?} checksum mismatch: expected {expected:#010x}, got {actual:#010x}"
            ),
            Self::Corrupt(kind) => write!(formatter, "corrupt ext4 metadata: {kind:?}"),
        }
    }
}

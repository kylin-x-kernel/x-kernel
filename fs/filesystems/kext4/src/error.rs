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
    /// An inode table entry.
    Inode(u32),
    /// An extent tree block.
    ExtentBlock { inode: u32, block: u64 },
    /// A directory data block.
    DirectoryBlock { inode: u32, block: u64 },
    /// An extended-attribute block.
    XattrBlock { inode: u32, block: u64 },
    /// A JBD2 journal superblock.
    JournalSuperblock,
    /// A block in the JBD2 circular log.
    JournalBlock(u32),
}

/// Classifies unsupported ext4 semantics beyond feature-bit negotiation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnsupportedKind {
    /// The inode's block map uses a shape outside the supported read path.
    NonExtentInode,
    /// The inode stores data inline in metadata.
    InlineData,
    /// The inode uses encrypted-name semantics.
    EncryptedName,
    /// The inode uses casefolded-name semantics.
    CasefoldName,
    /// The inode is reserved for ext4 internal metadata.
    ReservedInode,
    /// The filesystem stores its journal on another block device.
    ExternalJournal,
    /// The filesystem uses ext4 orphan-file semantics, not the legacy orphan list.
    OrphanFile,
    /// The directory uses an HTree shape this stage does not scan.
    IndexedDirectory,
    /// The directory needs the ext4 large_dir HTree depth extension.
    LargeDir,
    /// The xattr value is stored in a dedicated EA inode.
    ExternalXattrInode,
    /// The extent tree is deeper than this stage supports.
    ExtentDepth,
    /// The requested extent change needs split, conversion, or tree growth.
    ExtentMutation,
    /// The inode uses huge-file block accounting semantics.
    HugeFile,
    /// The inode owns an external xattr block that this mutation path cannot free yet.
    ExternalXattrBlock,
    /// The inode link count has reached the supported ext4 hard-link limit.
    LinkCountLimit,
    /// The symbolic link target does not fit in supported symlink storage yet.
    BlockMappedSymlink,
    /// The device number cannot be represented in ext4's on-disk `i_block`.
    DeviceId,
    /// A linked-inode truncate cleanup helper received a zero-link orphan.
    UnlinkedOrphanCleanup,
    /// The write path would need block allocation or unwritten extent conversion.
    UnallocatedWrite,
    /// The current writeback foundation does not support shrinking file size.
    FileSizeShrink,
    /// The requested write path requires an internal JBD2 journal.
    JournaledWrite,
    /// The journal is larger than this recovery implementation will materialize.
    JournalTooLarge,
    /// A JBD2 descriptor uses the deleted-update tag flag, which is not replayed yet.
    DeletedJournalUpdate,
    /// A metadata buffer is already owned by another running transaction.
    ConcurrentMetadataTransaction,
    /// A metadata buffer state transition is not valid for this operation.
    MetadataBufferState,
    /// A timestamp cannot be represented by the available inode fields.
    TimestampRange,
    /// The inode kind is outside this stage's read-only data path.
    InodeKind,
    /// A byte string cannot be represented by the caller's name encoding.
    NameEncoding,
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
    /// A block bitmap does not match allocation or release invariants.
    InvalidBlockBitmap,
    /// An inode bitmap does not match allocation or release invariants.
    InvalidInodeBitmap,
    /// An inode number is outside the filesystem inode table.
    InvalidInodeNumber,
    /// An inode table entry is structurally invalid.
    InvalidInode,
    /// An extent header or entry is structurally invalid.
    InvalidExtent,
    /// A directory entry is structurally invalid.
    InvalidDirectoryEntry,
    /// An extended-attribute entry or block is structurally invalid.
    InvalidXattr,
    /// The Ext4 journal location or JBD2 geometry is invalid.
    InvalidJournal,
    /// A file type or mode cannot be interpreted by this stage.
    InvalidFileType,
}

/// Errors returned while decoding or mounting an ext4 filesystem.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ext4Error {
    /// The block driver rejected an I/O operation.
    Device(DriverError),
    /// The journal has aborted and no further metadata mutation is allowed.
    JournalAborted,
    /// A transaction handle ran out of metadata credits.
    InsufficientJournalCredits,
    /// No free space is available for the requested allocation.
    NoSpace,
    /// The requested directory entry already exists.
    AlreadyExists,
    /// The requested directory entry does not exist.
    NotFound,
    /// The requested directory removal target still contains children.
    DirectoryNotEmpty,
    /// A caller-provided ext4 name is not valid for this operation.
    InvalidName,
    /// A journal operation cannot complete while a transaction is still active.
    JournalBusy,
    /// A journal operation referenced a transaction that is not known here.
    InvalidJournalTransaction,
    /// An offset or length calculation overflowed.
    Overflow,
    /// A caller-provided buffer has the wrong length.
    InvalidBufferLength { expected: usize, actual: usize },
    /// The underlying device block geometry is unusable.
    InvalidDeviceBlockSize(usize),
    /// An I/O range exceeds the device or filesystem.
    OutOfBounds,
    /// A directory stream position is not a valid ext4 record boundary.
    InvalidDirectoryPosition,
    /// The superblock magic is not `0xEF53`.
    InvalidMagic(u16),
    /// The filesystem revision is not supported.
    UnsupportedRevision(u32),
    /// A feature changes a layout that this stage cannot interpret.
    UnsupportedFeature { class: FeatureClass, bits: u32 },
    /// The JBD2 journal uses unsupported feature bits.
    UnsupportedJournalFeature {
        compat: u32,
        incompat: u32,
        read_only_compat: u32,
    },
    /// The filesystem uses semantics outside this stage's scope.
    Unsupported(UnsupportedKind),
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
            Self::JournalAborted => formatter.write_str("ext4 journal has aborted"),
            Self::InsufficientJournalCredits => {
                formatter.write_str("insufficient ext4 journal credits")
            }
            Self::NoSpace => formatter.write_str("no free ext4 space is available"),
            Self::AlreadyExists => formatter.write_str("ext4 directory entry already exists"),
            Self::NotFound => formatter.write_str("ext4 directory entry was not found"),
            Self::DirectoryNotEmpty => formatter.write_str("ext4 directory is not empty"),
            Self::InvalidName => formatter.write_str("invalid ext4 directory entry name"),
            Self::JournalBusy => formatter.write_str("ext4 journal transaction is busy"),
            Self::InvalidJournalTransaction => {
                formatter.write_str("invalid ext4 journal transaction")
            }
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
            Self::InvalidDirectoryPosition => {
                formatter.write_str("invalid ext4 directory position")
            }
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
            Self::UnsupportedJournalFeature {
                compat,
                incompat,
                read_only_compat,
            } => write!(
                formatter,
                "unsupported JBD2 features: compat={compat:#010x}, incompat={incompat:#010x}, \
                 ro_compat={read_only_compat:#010x}"
            ),
            Self::Unsupported(kind) => write!(formatter, "unsupported ext4 semantics: {kind:?}"),
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

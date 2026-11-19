// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::{
    fmt,
    sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering},
};

use ktime_types::TimestampLimits;
#[cfg(unittest)]
pub(crate) use unittests::allocation_test_inode;

use crate::{
    BlockMapping, ChecksumTarget, CorruptKind, Ext4Error, Ext4Result, Ext4SbInfo, FilesystemBlock,
    InodeNumber, LogicalBlock, UnsupportedKind,
    buffer::MetadataWriteAccess,
    disk::{checksum, extent as disk_extent, inode as disk_inode},
    extent::InodeMappingRoot,
    file::RegularWriteMetadata,
    jbd2::{JournalCredits, JournalHandle, RuntimeTransaction},
    prealloc_space::InodePreallocMap,
    sync::{self, Mutex, RwLock},
};

const EXT4_ENCODED_DEV_MAJOR_MAX: u32 = 0x0fff;
const EXT4_ENCODED_DEV_MINOR_MAX: u32 = 0x000f_ffff;

fn node_flags_from_disk(flags: u32) -> kvfs::NodeFlags {
    let mut node_flags = kvfs::NodeFlags::empty();
    node_flags.set(
        kvfs::NodeFlags::IMMUTABLE,
        flags & disk_inode::EXT4_IMMUTABLE_FL != 0,
    );
    node_flags.set(
        kvfs::NodeFlags::APPEND_ONLY,
        flags & disk_inode::EXT4_APPEND_FL != 0,
    );
    node_flags.set(
        kvfs::NodeFlags::NO_ATIME,
        flags & disk_inode::EXT4_NOATIME_FL != 0,
    );
    node_flags
}

/// Selects the mapping-format limit without fetching the legacy limit for extents.
/// Callers obtaining flags from a resident inode release its lock before the
/// fallback callback, which may need mount protection.
pub(crate) fn inode_max_file_size(
    flags: u32,
    extent_max_size: u64,
    bitmap_max_size: impl FnOnce() -> u64,
) -> u64 {
    if flags & disk_inode::EXT4_EXTENTS_FL != 0 {
        extent_max_size
    } else {
        bitmap_max_size()
    }
}

/// Inode file type derived from `i_mode`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InodeKind {
    /// FIFO special inode.
    Fifo,
    /// Character device inode.
    CharacterDevice,
    /// Directory inode.
    Directory,
    /// Block device inode.
    BlockDevice,
    /// Regular file inode.
    RegularFile,
    /// Symbolic link inode.
    Symlink,
    /// Socket inode.
    Socket,
}

impl InodeKind {
    pub(crate) fn from_mode(mode: u16) -> Ext4Result<Self> {
        match mode & disk_inode::S_IFMT {
            disk_inode::S_IFIFO => Ok(Self::Fifo),
            disk_inode::S_IFCHR => Ok(Self::CharacterDevice),
            disk_inode::S_IFDIR => Ok(Self::Directory),
            disk_inode::S_IFBLK => Ok(Self::BlockDevice),
            disk_inode::S_IFREG => Ok(Self::RegularFile),
            disk_inode::S_IFLNK => Ok(Self::Symlink),
            disk_inode::S_IFSOCK => Ok(Self::Socket),
            _ => Err(Ext4Error::Corrupt(CorruptKind::InvalidFileType)),
        }
    }

    pub(crate) const fn is_directory(self) -> bool {
        matches!(self, Self::Directory)
    }

    pub(crate) const fn mask_flags(self, flags: u32) -> u32 {
        const REGULAR_EXCLUDED: u32 =
            0x0003_0000 | disk_inode::EXT4_CASEFOLD_FL | disk_inode::EXT4_PROJINHERIT_FL;
        const OTHER_ALLOWED: u32 = 0x0000_0040 | disk_inode::EXT4_NOATIME_FL;
        match self {
            Self::Directory => flags,
            Self::RegularFile => flags & !REGULAR_EXCLUDED,
            _ => flags & OTHER_ALLOWED,
        }
    }

    const fn mode_bits(self) -> u16 {
        match self {
            Self::Fifo => disk_inode::S_IFIFO,
            Self::CharacterDevice => disk_inode::S_IFCHR,
            Self::Directory => disk_inode::S_IFDIR,
            Self::BlockDevice => disk_inode::S_IFBLK,
            Self::RegularFile => disk_inode::S_IFREG,
            Self::Symlink => disk_inode::S_IFLNK,
            Self::Socket => disk_inode::S_IFSOCK,
        }
    }
}

/// Metadata used to initialize a newly allocated inode table entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct InodeInitialization {
    kind: InodeKind,
    permissions: u16,
    uid: u32,
    gid: u32,
    project_id: u32,
    inherited_flags: u32,
    size: u64,
    block: [u8; disk_inode::INODE_BLOCK_BYTES],
    uses_extent_tree: bool,
    links_count: u16,
    timestamp: Ext4Timestamp,
    generation: u32,
}

#[allow(dead_code)]
impl InodeInitialization {
    pub(crate) const fn uid(self) -> u32 {
        self.uid
    }

    pub(crate) const fn gid(self) -> u32 {
        self.gid
    }

    pub(crate) const fn regular_file(permissions: u16, uid: u32, gid: u32) -> Self {
        Self {
            kind: InodeKind::RegularFile,
            permissions,
            uid,
            gid,
            project_id: 0,
            inherited_flags: 0,
            size: 0,
            block: [0; disk_inode::INODE_BLOCK_BYTES],
            uses_extent_tree: false,
            links_count: 1,
            timestamp: Ext4Timestamp::new(0, 0),
            generation: 0,
        }
    }

    pub(crate) const fn directory(permissions: u16, uid: u32, gid: u32) -> Self {
        Self {
            kind: InodeKind::Directory,
            permissions,
            uid,
            gid,
            project_id: 0,
            inherited_flags: 0,
            size: 0,
            block: [0; disk_inode::INODE_BLOCK_BYTES],
            uses_extent_tree: false,
            links_count: 2,
            timestamp: Ext4Timestamp::new(0, 0),
            generation: 0,
        }
    }

    pub(crate) fn fast_symlink(target: &[u8], uid: u32, gid: u32) -> Ext4Result<Self> {
        if target.is_empty() || target.len() >= disk_inode::INODE_BLOCK_BYTES {
            return Err(Ext4Error::Unsupported(UnsupportedKind::BlockMappedSymlink));
        }
        if target.contains(&0) {
            return Err(Ext4Error::InvalidName);
        }

        let mut block = [0; disk_inode::INODE_BLOCK_BYTES];
        block[..target.len()].copy_from_slice(target);
        Ok(Self {
            kind: InodeKind::Symlink,
            permissions: 0o777,
            uid,
            gid,
            project_id: 0,
            inherited_flags: 0,
            size: u64::try_from(target.len()).map_err(|_| Ext4Error::Overflow)?,
            block,
            uses_extent_tree: false,
            links_count: 1,
            timestamp: Ext4Timestamp::new(0, 0),
            generation: 0,
        })
    }

    pub(crate) fn block_mapped_symlink(target_len: usize, uid: u32, gid: u32) -> Ext4Result<Self> {
        if target_len == 0 {
            return Err(Ext4Error::InvalidName);
        }
        Ok(Self {
            kind: InodeKind::Symlink,
            permissions: 0o777,
            uid,
            gid,
            project_id: 0,
            inherited_flags: 0,
            size: u64::try_from(target_len).map_err(|_| Ext4Error::Overflow)?,
            block: [0; disk_inode::INODE_BLOCK_BYTES],
            uses_extent_tree: true,
            links_count: 1,
            timestamp: Ext4Timestamp::new(0, 0),
            generation: 0,
        })
    }

    pub(crate) fn special(
        kind: InodeKind,
        permissions: u16,
        device: Option<Ext4DeviceId>,
        uid: u32,
        gid: u32,
    ) -> Ext4Result<Self> {
        let mut block = [0; disk_inode::INODE_BLOCK_BYTES];
        match kind {
            InodeKind::CharacterDevice | InodeKind::BlockDevice => {
                let device = device.ok_or(Ext4Error::InvalidName)?;
                put_u32(&mut block, 4, encode_new_device_id(device)?)?;
            }
            InodeKind::Fifo | InodeKind::Socket => {}
            _ => return Err(Ext4Error::Unsupported(UnsupportedKind::InodeKind)),
        }

        Ok(Self {
            kind,
            permissions,
            uid,
            gid,
            project_id: 0,
            inherited_flags: 0,
            size: 0,
            block,
            uses_extent_tree: false,
            links_count: 1,
            timestamp: Ext4Timestamp::new(0, 0),
            generation: 0,
        })
    }

    pub(crate) const fn with_owner(mut self, uid: u32, gid: u32) -> Self {
        self.uid = uid;
        self.gid = gid;
        self
    }

    pub(crate) fn with_parent(mut self, parent: &Ext4Inode, has_project: bool) -> Self {
        const INHERITED: u32 = 0x6201_c4cf;
        let flags = parent.flags() & INHERITED;
        self.project_id = parent.inherited_project_id(has_project);
        self.inherited_flags = self.kind.mask_flags(flags);
        self
    }

    pub(crate) const fn project_id(self) -> u32 {
        self.project_id
    }

    pub(crate) const fn with_timestamp(mut self, timestamp: Ext4Timestamp) -> Self {
        self.timestamp = timestamp;
        self
    }

    pub(crate) const fn with_generation(mut self, generation: u32) -> Self {
        self.generation = generation;
        self
    }

    pub(crate) const fn kind(self) -> InodeKind {
        self.kind
    }

    const fn mode(self) -> u16 {
        self.kind.mode_bits() | (self.permissions & 0o7777)
    }
}

/// Linux device identifier stored in a character or block device inode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ext4DeviceId {
    major: u32,
    minor: u32,
}

impl Ext4DeviceId {
    /// Creates a Linux device identifier from major and minor numbers.
    pub const fn new(major: u32, minor: u32) -> Self {
        Self { major, minor }
    }

    /// Creates a device identifier that ext4 can encode on disk.
    pub fn new_checked(major: u32, minor: u32) -> Ext4Result<Self> {
        let device = Self { major, minor };
        ensure_ext4_device_id_representable(device)?;
        Ok(device)
    }

    /// Returns the decoded major number.
    pub const fn major(self) -> u32 {
        self.major
    }

    /// Returns the decoded minor number.
    pub const fn minor(self) -> u32 {
        self.minor
    }
}

/// Journaled inode metadata update for chmod, chown, and explicit timestamps.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ext4InodeMetadataUpdate {
    ctime: Ext4Timestamp,
    mode: Option<u16>,
    owner: Option<(u32, u32)>,
    atime: Option<Ext4Timestamp>,
    mtime: Option<Ext4Timestamp>,
}

impl Ext4InodeMetadataUpdate {
    /// Creates an inode metadata update with the required ctime value.
    pub const fn new(ctime: Ext4Timestamp) -> Self {
        Self {
            ctime,
            mode: None,
            owner: None,
            atime: None,
            mtime: None,
        }
    }

    /// Updates permission bits while preserving the inode file type.
    pub const fn with_mode(mut self, mode: u16) -> Self {
        self.mode = Some(mode & 0o7777);
        self
    }

    /// Updates the inode owner.
    pub const fn with_owner(mut self, uid: u32, gid: u32) -> Self {
        self.owner = Some((uid, gid));
        self
    }

    /// Updates the inode access time.
    pub const fn with_atime(mut self, atime: Ext4Timestamp) -> Self {
        self.atime = Some(atime);
        self
    }

    /// Updates the inode modification time.
    pub const fn with_mtime(mut self, mtime: Ext4Timestamp) -> Self {
        self.mtime = Some(mtime);
        self
    }

    const fn is_empty(self) -> bool {
        self.mode.is_none() && self.owner.is_none() && self.atime.is_none() && self.mtime.is_none()
    }
}

/// Ext4 inode timestamp decoded from the base and extra timestamp fields.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ext4Timestamp {
    seconds: i64,
    nanos: u32,
}

impl Ext4Timestamp {
    /// Creates a timestamp relative to the Unix epoch.
    ///
    /// Sub-second nanoseconds are normalized so any constructed timestamp
    /// satisfies `nanos < NANOS_PER_SEC`.
    pub const fn new(seconds: i64, nanos: u32) -> Self {
        let seconds = seconds + (nanos / ktime_types::NANOS_PER_SEC as u32) as i64;
        let nanos = nanos % ktime_types::NANOS_PER_SEC as u32;
        Self { seconds, nanos }
    }

    /// Returns seconds relative to the Unix epoch.
    pub const fn seconds(self) -> i64 {
        self.seconds
    }

    /// Returns the nanosecond component.
    pub const fn nanos(self) -> u32 {
        self.nanos
    }

    pub(crate) fn encode(self, has_extra: bool) -> Ext4Result<EncodedTimestamp> {
        const EXT4_EPOCH_BITS: u32 = 2;
        const EXT4_EPOCH_COUNT: i64 = 1 << EXT4_EPOCH_BITS;

        if self.nanos >= ktime_types::NANOS_PER_SEC as u32 {
            return Err(Ext4Error::Unsupported(UnsupportedKind::TimestampRange));
        }

        if has_extra {
            for epoch in 0..EXT4_EPOCH_COUNT {
                let epoch_seconds = epoch.checked_shl(32).ok_or(Ext4Error::Overflow)?;
                let base_seconds = self
                    .seconds
                    .checked_sub(epoch_seconds)
                    .ok_or(Ext4Error::Overflow)?;
                if let Ok(base_seconds) = i32::try_from(base_seconds) {
                    let base_seconds = u32::from_le_bytes(base_seconds.to_le_bytes());
                    let epoch = u32::try_from(epoch).map_err(|_| Ext4Error::Overflow)?;
                    let extra = (self.nanos << EXT4_EPOCH_BITS) | epoch;
                    return Ok(EncodedTimestamp {
                        base_seconds,
                        extra: Some(extra),
                    });
                }
            }
            return Err(Ext4Error::Unsupported(UnsupportedKind::TimestampRange));
        }

        // Legacy inodes have no storage for nanoseconds or epoch bits. Linux ext4
        // preserves their writability by discarding nanoseconds and clamping the
        // seconds value to the signed 32-bit on-disk range.
        let seconds = self.seconds.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
        Ok(EncodedTimestamp {
            base_seconds: u32::from_le_bytes(seconds.to_le_bytes()),
            extra: None,
        })
    }
}

pub(crate) const fn timestamp_limits_for_inode_size(inode_size: u16) -> TimestampLimits {
    const EXT4_EXTRA_TIMESTAMP_MAX_SECONDS: i64 = (1_i64 << 34) - 1 + i32::MIN as i64;
    const EXTRA_TIMESTAMP_END: usize = disk_inode::ATIME_EXTRA_OFFSET + 4;

    let has_extra_timestamps = inode_size as usize >= EXTRA_TIMESTAMP_END;
    TimestampLimits::new(
        if has_extra_timestamps {
            1
        } else {
            ktime_types::NANOS_PER_SEC as u32
        },
        i32::MIN as i64,
        if has_extra_timestamps {
            EXT4_EXTRA_TIMESTAMP_MAX_SECONDS
        } else {
            i32::MAX as i64
        },
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RawTimestampFields {
    base_seconds: u32,
    extra: Option<u32>,
}

impl RawTimestampFields {
    const fn new(base_seconds: u32, extra: Option<u32>) -> Self {
        Self {
            base_seconds,
            extra,
        }
    }
}

impl TryFrom<RawTimestampFields> for Ext4Timestamp {
    type Error = Ext4Error;

    fn try_from(raw: RawTimestampFields) -> Ext4Result<Self> {
        const EXT4_EPOCH_BITS: u32 = 2;
        const EXT4_EPOCH_MASK: u32 = (1 << EXT4_EPOCH_BITS) - 1;

        let mut seconds = i64::from(i32::from_le_bytes(raw.base_seconds.to_le_bytes()));
        let nanos = if let Some(extra) = raw.extra {
            let epoch = extra & EXT4_EPOCH_MASK;
            if epoch != 0 {
                seconds = seconds
                    .checked_add(i64::from(epoch) << 32)
                    .ok_or(Ext4Error::Overflow)?;
            }
            let nanos = extra >> EXT4_EPOCH_BITS;
            if nanos >= ktime_types::NANOS_PER_SEC as u32 {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidInode));
            }
            nanos
        } else {
            0
        };

        Ok(Self { seconds, nanos })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EncodedTimestamp {
    base_seconds: u32,
    extra: Option<u32>,
}

/// Decoded fields shared by the resident inode and its journaled table entry.
///
/// This is disk-algorithm state, not a second resident VFS identity. In
/// particular, `disk_size` is Linux ext4's `i_disksize`; the visible `i_size`
/// is stored separately in the same component and the generic inode lifecycle
/// remains owned by KVFS.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Ext4InodeMetadata {
    uid: u32,
    gid: u32,
    project_id: u32,
    disk_size: u64,
    blocks: u64,
    block: [u8; disk_inode::INODE_BLOCK_BYTES],
    file_acl: u64,
    extra_isize: u16,
    inline_xattr: Vec<u8>,
    generation: u32,
    links_count: u16,
    atime: Ext4Timestamp,
    ctime: Ext4Timestamp,
    mtime: Ext4Timestamp,
}

impl Ext4InodeMetadata {
    /// Returns whether a symlink keeps its target bytes in `i_block`.
    ///
    /// - `kind`: resident inode kind; only symlinks can be fast symlinks.
    /// - `flags`: raw `i_flags`; an inline-data inode is never a fast symlink.
    /// - `filesystem_block_size`: filesystem block size in bytes.
    /// - `has_ea_inode_feature`: whether the filesystem accounts EA blocks.
    ///
    /// Returns: `true` when the target is stored inline in `i_block`.
    ///
    /// Errors: `Overflow` when the EA block count exceeds `blocks`.
    fn is_fast_symlink(
        &self,
        kind: InodeKind,
        flags: u32,
        filesystem_block_size: u32,
        has_ea_inode_feature: bool,
    ) -> Ext4Result<bool> {
        if kind != InodeKind::Symlink || flags & disk_inode::EXT4_INLINE_DATA_FL != 0 {
            return Ok(false);
        }
        let ea_blocks = if self.file_acl == 0 {
            0
        } else {
            u64::from(filesystem_block_size) / 512
        };
        let data_blocks = self
            .blocks
            .checked_sub(ea_blocks)
            .ok_or(Ext4Error::Overflow)?;
        Ok(if has_ea_inode_feature {
            self.disk_size != 0 && self.disk_size < disk_inode::INODE_BLOCK_BYTES as u64
        } else {
            data_blocks == 0
        })
    }

    fn from_raw(
        raw: disk_inode::RawInode,
        allow_zero_links: bool,
        has_project: bool,
    ) -> Ext4Result<DecodedInodeFields> {
        if raw.links_count() == 0 && !allow_zero_links {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInode));
        }
        let kind = InodeKind::from_mode(raw.mode())?;
        let permissions = raw.mode() & 0o7777;
        let flags = raw.flags();
        if flags & disk_inode::EXT4_INLINE_DATA_FL != 0 {
            return Err(Ext4Error::Unsupported(UnsupportedKind::InlineData));
        }
        if flags & disk_inode::EXT4_ENCRYPT_FL != 0 {
            return Err(Ext4Error::Unsupported(UnsupportedKind::EncryptedName));
        }
        if flags & disk_inode::EXT4_CASEFOLD_FL != 0 {
            return Err(Ext4Error::Unsupported(UnsupportedKind::CasefoldName));
        }

        let metadata = Self {
            uid: raw.uid(),
            gid: raw.gid(),
            project_id: if has_project {
                raw.project_id().unwrap_or(0)
            } else {
                0
            },
            disk_size: raw.size(),
            blocks: raw.blocks(),
            block: *raw.block(),
            file_acl: raw.file_acl(),
            extra_isize: raw.extra_isize(),
            inline_xattr: Vec::from(raw.inline_xattr()),
            generation: raw.generation(),
            links_count: raw.links_count(),
            atime: Ext4Timestamp::try_from(RawTimestampFields::new(
                raw.atime(),
                raw.atime_extra(),
            ))?,
            ctime: Ext4Timestamp::try_from(RawTimestampFields::new(
                raw.ctime(),
                raw.ctime_extra(),
            ))?,
            mtime: Ext4Timestamp::try_from(RawTimestampFields::new(
                raw.mtime(),
                raw.mtime_extra(),
            ))?,
        };
        Ok(DecodedInodeFields {
            kind,
            flags,
            permissions,
            metadata,
        })
    }
}

/// The fields produced by one `from_raw` decode of an on-disk inode.
///
/// This is transient and never resident: `from_raw` is its only producer, and
/// the caller immediately routes the four parts to their resident owners —
/// `kind` to the immutable `Ext4Inode.kind`, `flags` to the sole-resident
/// `Ext4Inode.flags` atomic, `permissions` to the `Ext4Inode.permissions`
/// atomic, and `metadata` to `Ext4InodeState.metadata` — so no copy of these
/// fields survives past the decode boundary.
pub(crate) struct DecodedInodeFields {
    /// File type from `i_mode`; routed to the immutable `Ext4Inode.kind`.
    pub(crate) kind: InodeKind,
    /// Raw `i_flags`; routed to the sole-resident `Ext4Inode.flags` atomic.
    pub(crate) flags: u32,
    /// POSIX permission and special bits (`i_mode & 0o7777`), type bits
    /// excluded; routed to the `Ext4Inode.permissions` atomic.
    pub(crate) permissions: u16,
    /// Remaining on-disk fields without the type, flags, and permissions;
    /// routed to `Ext4InodeState.metadata`.
    pub(crate) metadata: Ext4InodeMetadata,
}

/// Bounded, disposable extent-status proofs for reservation preparation.
/// Only initialized mappings are cached; physical addresses and delayed
/// reservation accounting remain outside this tree.
#[derive(Default)]
struct ExtentStatusTree {
    ranges: BTreeMap<u64, ExtentStatus>,
    /// End of the contiguous written prefix `[0, covered_prefix_end)`.
    covered_prefix_end: u64,
}

#[derive(Clone, Copy)]
struct ExtentStatus {
    /// Length in logical blocks; entries are nonempty and never overflow.
    len: u64,
    kind: ExtentStatusKind,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ExtentStatusKind {
    /// Initialized storage, not a guarantee that dirty data is durable.
    Written,
    // Future states require explicit transition and eviction rules.
    // Unwritten: allocated physical storage that is not yet initialized.
    // Unwritten,
    // Delayed: delayed-allocation reservation, not disposable cache data.
    // Delayed,
    // Hole: a range without allocated storage.
    // Hole,
}

impl ExtentStatusTree {
    const MAX_RANGES: usize = 64;

    fn contains_written(&self, start: u64, end: u64) -> bool {
        start < end
            && self
                .ranges
                .range(..=start)
                .next_back()
                .is_some_and(|(&range_start, status)| {
                    status.kind == ExtentStatusKind::Written && end <= range_start + status.len
                })
    }

    fn cache_written(&mut self, mut start: u64, mut end: u64) {
        if start >= end || self.contains_written(start, end) {
            return;
        }
        // True when a merge or eviction removed range evidence inside the
        // published prefix; the prefix is then recomputed instead of extended.
        let mut prefix_lost = false;
        if let Some((&previous_start, &status)) = self.ranges.range(..=start).next_back()
            && status.kind == ExtentStatusKind::Written
            && previous_start + status.len >= start
        {
            start = previous_start;
            end = end.max(previous_start + status.len);
            prefix_lost |= previous_start < self.covered_prefix_end;
            self.ranges.remove(&previous_start);
        }
        while let Some((&next_start, &status)) = self.ranges.range(start..=end).next() {
            end = end.max(next_start + status.len);
            prefix_lost |= next_start < self.covered_prefix_end;
            self.ranges.remove(&next_start);
        }
        // Evict the shortest range when full, shedding the least coverage; ties
        // drop the lowest offset (ascending iteration + min_by_key keeps the first
        // minimum). Recency-free by design: eviction only drops a proof, never
        // invents one, so a wrong victim costs one slow-path re-walk that
        // re-caches the range and `has_initialized_overwrite` stays sound (it can
        // under-report, never over-report). Clearing the whole tree instead made
        // the randwrite benchmark re-enter the slow walk on ~half of its overwrites.
        if self.ranges.len() >= Self::MAX_RANGES
            && let Some((&shortest, _)) = self.ranges.iter().min_by_key(|(_, status)| status.len)
        {
            prefix_lost |= shortest < self.covered_prefix_end;
            self.ranges.remove(&shortest);
        }
        self.ranges.insert(
            start,
            ExtentStatus {
                len: end - start,
                kind: ExtentStatusKind::Written,
            },
        );
        if prefix_lost {
            self.refresh_covered_prefix();
        } else if start <= self.covered_prefix_end {
            self.covered_prefix_end = self.covered_prefix_end.max(end);
        }
    }

    /// Recomputes the contiguous written prefix after a proof was dropped.
    ///
    /// Returns: nothing; the stored prefix shrinks to the surviving coverage.
    fn refresh_covered_prefix(&mut self) {
        let mut prefix_end = 0;
        for (&range_start, status) in &self.ranges {
            if status.kind != ExtentStatusKind::Written || range_start > prefix_end {
                break;
            }
            prefix_end = prefix_end.max(range_start.saturating_add(status.len));
        }
        self.covered_prefix_end = prefix_end;
    }

    /// Returns the exclusive end of the contiguous written prefix.
    fn covered_prefix_end(&self) -> u64 {
        self.covered_prefix_end
    }

    /// Drops reconstructible proofs, never delayed-allocation reservations.
    fn clear(&mut self) {
        self.ranges.clear();
        self.covered_prefix_end = 0;
    }
}

/// Timestamp values selected for one metadata publication. A concurrent update
/// remains dirty unless its current value is the one this publication captured.
#[derive(Clone, Copy, Default)]
pub(crate) struct PendingInodeTimestamps {
    atime: Option<Ext4Timestamp>,
    mtime: Option<Ext4Timestamp>,
    ctime: Option<Ext4Timestamp>,
}

impl PendingInodeTimestamps {
    /// Merges the captured dirty fields before the caller's explicit mutation.
    pub(crate) fn encode(self, inode_bytes: &mut [u8]) -> Ext4Result<()> {
        let raw = disk_inode::RawInode::decode(inode_bytes)?;
        let fields = [
            (
                self.atime,
                0x08,
                disk_inode::ATIME_EXTRA_OFFSET,
                raw.atime_extra().is_some(),
            ),
            (
                self.mtime,
                0x10,
                disk_inode::MTIME_EXTRA_OFFSET,
                raw.mtime_extra().is_some(),
            ),
            (
                self.ctime,
                0x0c,
                disk_inode::CTIME_EXTRA_OFFSET,
                raw.ctime_extra().is_some(),
            ),
        ];
        for (timestamp, base, extra, has_extra) in fields {
            if let Some(timestamp) = timestamp {
                update_inode_timestamp_bytes(inode_bytes, base, extra, has_extra, timestamp)?;
            }
        }
        Ok(())
    }
}

struct Ext4InodeState {
    metadata: Ext4InodeMetadata,
    // None requires conservative mount-wide sync. A transaction identity stays
    // meaningful even when a cold resident inode outlives journal sequence wrap.
    sync_transaction: Option<Arc<RuntimeTransaction>>,
    has_dirty_atime: bool,
    has_dirty_mtime: bool,
    has_dirty_ctime: bool,
    delayed_extents: BTreeMap<u64, u64>,
    reserved_data_blocks: u64,
    dquots: [Option<kquota::DquotRef>; 3],
    extent_status: ExtentStatusTree,
    is_extra_isize_expansion_disabled: bool,
    orphan: Option<OrphanLocation>,
}

/// Orphan membership owned by the resident ext4 inode.
///
/// This safely combines Linux ext4's `i_orphan` / `i_orphan_idx` union with
/// `EXT4_STATE_ORPHAN_FILE`. It is runtime state and is therefore kept out of
/// the decoded on-disk inode metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OrphanLocation {
    LegacyList,
    File { index: u32 },
}

impl Ext4InodeState {
    /// Returns whether publishing `timestamp` would change a resident write timestamp.
    ///
    /// - `timestamp`: coarse wall-clock sample the current operation would publish.
    ///
    /// Returns: `true` when `mtime` or `ctime` differs from `timestamp`; `false`
    /// means [`publish_write_timestamps`](Self::publish_write_timestamps) would
    /// leave both values and dirty flags untouched.
    fn needs_write_timestamp(&self, timestamp: Ext4Timestamp) -> bool {
        self.metadata.mtime != timestamp || self.metadata.ctime != timestamp
    }

    fn publish_write_timestamps(&mut self, timestamp: Ext4Timestamp) {
        self.has_dirty_mtime |= self.metadata.mtime != timestamp;
        self.has_dirty_ctime |= self.metadata.ctime != timestamp;
        self.metadata.mtime = timestamp;
        self.metadata.ctime = timestamp;
    }

    fn allocated_blocks_after(&self, sectors: i128, flags: u32) -> Ext4Result<u64> {
        if flags & disk_inode::EXT4_HUGE_FILE_FL != 0 {
            return Err(Ext4Error::Unsupported(UnsupportedKind::HugeFile));
        }
        i128::from(self.metadata.blocks)
            .checked_add(sectors)
            .and_then(|blocks| u64::try_from(blocks).ok())
            .filter(|blocks| blocks >> 48 == 0)
            .ok_or(Ext4Error::Overflow)
    }

    // The caller samples visible_size while holding the inode-state lock,
    // which excludes both size publication and mapping/proof changes.
    fn has_initialized_overwrite(
        &self,
        start: u64,
        count: u64,
        block_size: u32,
        visible_size: u64,
    ) -> bool {
        let Some(end) = start.checked_add(count) else {
            return false;
        };
        // Extensions, partial EOF blocks and pending reservations still need
        // the slow path's mapping and accounting validation.
        self.reserved_data_blocks == 0
            && self.delayed_extents.is_empty()
            && end <= visible_size.min(self.metadata.disk_size) / u64::from(block_size)
            && self.extent_status.contains_written(start, end)
    }

    /// Returns the inode owner's user ID.
    ///
    /// No locking here: only valid while the caller already holds the inode-state
    /// lock, e.g. inside [`Ext4Inode::with_state`] or [`Ext4Inode::update_state`].
    pub(crate) fn uid(&self) -> u32 {
        self.metadata.uid
    }

    /// Returns the inode owner's group ID.
    ///
    /// No locking here: only valid while the caller already holds the inode-state
    /// lock, e.g. inside [`Ext4Inode::with_state`] or [`Ext4Inode::update_state`].
    pub(crate) fn gid(&self) -> u32 {
        self.metadata.gid
    }

    /// Returns the ext4 on-disk inode size in bytes.
    ///
    /// This is the kext4 equivalent of Linux ext4's `i_disksize`. No locking
    /// here: only valid while the caller already holds the inode-state lock, e.g.
    /// inside [`Ext4Inode::with_state`] or [`Ext4Inode::update_state`].
    pub(crate) fn disk_size(&self) -> u64 {
        self.metadata.disk_size
    }
}

/// A transient, lightweight inode attribute snapshot.
///
/// This is the kext4-side equivalent of Linux `struct kstat`: it is produced
/// under one inode-state lock for VFS `stat/getattr`, and is neither resident
/// storage nor a second inode identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ext4InodeStat {
    /// Raw Linux mode bits.
    pub mode: u16,
    /// Inode owner's user ID.
    pub uid: u32,
    /// Inode owner's group ID.
    pub gid: u32,
    /// VFS-visible size in bytes.
    pub size: u64,
    /// Allocated 512-byte block count.
    pub blocks: u64,
    /// Decoded character or block device ID.
    pub rdev: Option<Ext4DeviceId>,
    /// Inode link count.
    pub links_count: u16,
    /// Last-access timestamp.
    pub atime: Ext4Timestamp,
    /// Last-status-change timestamp.
    pub ctime: Ext4Timestamp,
    /// Last-modification timestamp.
    pub mtime: Ext4Timestamp,
}

/// The composed inode component of one resident KVFS inode.
///
/// This is the object-oriented equivalent of Linux `ext4_inode_info` with its
/// embedded `vfs_inode`: KVFS generic attribute operations and ext4 algorithms
/// access this same object. It is the private component of the sole resident
/// KVFS inode and has neither an independent resident identity nor an
/// independent reference count.
pub struct Ext4Inode {
    number: InodeNumber,
    /// File type decoded once at inode load, like Linux `inode->i_mode`.
    /// The on-disk type bits never change for a resident inode, so readers
    /// use this plain field without the inode-state lock.
    kind: InodeKind,
    /// POSIX permission and special (setuid/setgid/sticky) bits, i.e.
    /// `i_mode & 0o7777`; the type bits live only in `kind`. `mode()` composes
    /// the two into the full `i_mode` at the stat/encode boundary. The single
    /// writer (`set_permission`) publishes with a release store and readers
    /// load lock-free.
    permissions: AtomicU16,
    /// Raw ext4 `i_flags`, the sole copy (`EXT4_EXTENTS_FL`, `EXT4_INDEX_FL`, ...).
    /// It stays in the on-disk flag domain; the VFS `NodeFlags` are projected
    /// from it on read in `node_flags`. `EXT4_EXTENTS_FL` is immutable after
    /// load; the single writer publishes under the inode-state write lock and
    /// readers load lock-free.
    flags: AtomicU32,
    /// Runtime-only quota switch, kept out of the disk flag domain (Linux
    /// `S_NOQUOTA`). Written by `set_noquota`, read lock-free.
    no_quota: AtomicBool,
    // The sole generic i_size, shared with KVFS through InodeAttributeOperations.
    // Readers need no mapping lock. Every store after construction must hold
    // the state write lock: sizes(), stat(), and extent-proof checks rely on
    // that exclusion to sample i_size with the corresponding mapping state.
    visible_size: AtomicU64,
    state: RwLock<Ext4InodeState>,
    // Exclusive end in bytes of the file prefix whose blocks are proven
    // allocated and initialized. Zero means no proof is published.
    initialized_extent_end: AtomicU64,
    writeback_lock: Mutex<()>,
    writeback_io: Arc<crate::writeback_io::InodeWritebackIo>,
    /// Speculative prealloc spaces owned by this inode (Linux:
    /// `i_prealloc_node`). They die with this inode object; see the
    /// prealloc-space module for the lifecycle and locking.
    prealloc_map: InodePreallocMap,
}

impl Ext4Inode {
    fn new(
        number: InodeNumber,
        kind: InodeKind,
        flags: u32,
        permissions: u16,
        metadata: Ext4InodeMetadata,
    ) -> Self {
        let visible_size = metadata.disk_size;
        Self {
            number,
            kind,
            permissions: AtomicU16::new(permissions),
            flags: AtomicU32::new(flags),
            no_quota: AtomicBool::new(false),
            visible_size: AtomicU64::new(visible_size),
            state: RwLock::new(Ext4InodeState {
                metadata,
                sync_transaction: None,
                has_dirty_atime: false,
                has_dirty_mtime: false,
                has_dirty_ctime: false,
                delayed_extents: BTreeMap::new(),
                reserved_data_blocks: 0,
                dquots: [None, None, None],
                extent_status: ExtentStatusTree::default(),
                is_extra_isize_expansion_disabled: false,
                orphan: None,
            }),
            initialized_extent_end: AtomicU64::new(0),
            writeback_lock: Mutex::new(()),
            writeback_io: Arc::new(crate::writeback_io::InodeWritebackIo::new()),
            prealloc_map: InodePreallocMap::new(),
        }
    }

    /// Excludes new submissions and drains existing I/O before mapping mutation.
    /// KVFS invalidate/data protection precedes this guard. Never acquire it
    /// under mount protection; completion needs mount access but not this guard.
    pub(crate) fn lock_writeback(&self) -> sync::MutexGuard<'_, ()> {
        let guard = self.lock_writeback_submission();
        self.writeback_io.drain();
        guard
    }

    /// Serializes mapping preparation and publication, without waiting for I/O.
    pub(crate) fn lock_writeback_submission(&self) -> sync::MutexGuard<'_, ()> {
        sync::mutex_lock(&self.writeback_lock)
    }

    pub(crate) fn writeback_io(&self) -> &Arc<crate::writeback_io::InodeWritebackIo> {
        &self.writeback_io
    }

    /// Holds the inode-state read lock while running `f`, with zero cloning.
    ///
    /// This is the kext4 equivalent of Linux `down_read(i_data_sem)`: the
    /// caller observes a consistent inode state without copying it.
    ///
    /// # Locking
    ///
    /// The underlying `RwLock` is **not** reentrant. The closure `f` must not
    /// call any method on the same [`Ext4Inode`] that acquires the inode-state
    /// lock (e.g. [`with_state`](Self::with_state),
    /// [`update_state`](Self::update_state), [`sizes`](Self::sizes),
    /// [`disk_size`](Self::disk_size), [`set_size`](Self::set_size), etc.),
    /// otherwise the current thread will self-deadlock.
    ///
    /// # Writer fairness
    ///
    /// Unlike Linux `i_data_sem` (which is backed by `rwsem` and throttles new
    /// readers while a writer is pending, so truncate/writeback writers cannot
    /// be starved), the underlying [`ksync::RwLock`] is **reader-preference**:
    /// a new reader is admitted as long as no writer currently *holds* the
    /// lock, regardless of how many writers are *waiting*. A waiting writer is
    /// only woken by the last reader's release or by another writer's release.
    ///
    /// Practical consequence: under sustained concurrent read load (e.g. many
    /// page-fault readers), a writer such as truncate ([`update_state`]) may be
    /// deferred indefinitely, whereas Linux would let it through. Code paths
    /// that rely on writer progress for forward advancement must therefore not
    /// assume Linux-grade fairness on this lock.
    fn with_state<R>(&self, f: impl FnOnce(&Ext4InodeState) -> R) -> R {
        let state = sync::read_lock(&self.state);
        f(&state)
    }

    /// Holds the inode-state write lock while running `f`.
    ///
    /// This is the kext4 equivalent of Linux `down_write(i_data_sem)`: the
    /// caller may mutate the inode state under exclusive access. The closure
    /// return value is passed through to the caller.
    ///
    /// # Locking
    ///
    /// The underlying `RwLock` is **not** reentrant. The closure `f` must not
    /// call any method on the same [`Ext4Inode`] that acquires the inode-state
    /// lock (e.g. [`with_state`](Self::with_state),
    /// [`update_state`](Self::update_state), [`sizes`](Self::sizes),
    /// [`disk_size`](Self::disk_size), [`set_size`](Self::set_size), etc.),
    /// otherwise the current thread will self-deadlock.
    ///
    /// # Writer fairness
    ///
    /// See [`with_state`](Self::with_state#writer-fairness): unlike Linux
    /// `i_data_sem`, the underlying lock is reader-preference, so this writer
    /// can be deferred indefinitely under sustained read load and must not be
    /// used on a path that assumes Linux-grade writer fairness.
    fn update_state<R>(&self, f: impl FnOnce(&mut Ext4InodeState) -> R) -> R {
        let mut state = sync::write_lock(&self.state);
        f(&mut state)
    }

    pub(crate) fn with_quota_records<R>(
        &self,
        f: impl FnOnce(&[Option<kquota::DquotRef>; 3]) -> R,
    ) -> R {
        self.with_state(|state| f(&state.dquots))
    }

    pub(crate) fn update_quota_records<R>(
        &self,
        f: impl FnOnce(u32, u32, u32, u64, &mut [Option<kquota::DquotRef>; 3]) -> R,
    ) -> R {
        self.update_state(|state| {
            f(
                state.metadata.uid,
                state.metadata.gid,
                state.metadata.project_id,
                state.reserved_data_blocks,
                &mut state.dquots,
            )
        })
    }

    pub(crate) fn update_allocated_blocks(
        &self,
        sectors: i128,
        update_quota: impl FnOnce(&[Option<kquota::DquotRef>; 3]) -> Ext4Result<()>,
    ) -> Ext4Result<()> {
        self.update_state(|state| {
            let blocks = state.allocated_blocks_after(sectors, self.flags())?;
            update_quota(&state.dquots)?;
            state.metadata.blocks = blocks;
            Ok(())
        })
    }

    pub(crate) fn validate_allocated_blocks(
        &self,
        sectors: i128,
        validate_quota: impl FnOnce(&[Option<kquota::DquotRef>; 3]) -> Ext4Result<()>,
    ) -> Ext4Result<()> {
        self.with_state(|state| {
            state.allocated_blocks_after(sectors, self.flags())?;
            validate_quota(&state.dquots)
        })
    }

    pub(crate) fn drop_quota_records(&self, kind: Option<kquota::QuotaType>) {
        self.update_state(|state| {
            for quota_type in kquota::QuotaType::ALL {
                if kind.is_none_or(|kind| kind == quota_type) {
                    state.dquots[quota_type as usize] = None;
                }
            }
        });
    }

    /// This inode's speculative prealloc map. The lock inside is a leaf owned
    /// by the prealloc-space module, which is also the only code that locks
    /// it; see [`crate::prealloc_space`] for the lock order and the mutation
    /// entries.
    pub(crate) fn prealloc_map(&self) -> &InodePreallocMap {
        &self.prealloc_map
    }

    fn metadata_snapshot(&self) -> Ext4InodeMetadata {
        self.with_state(|state| state.metadata.clone())
    }

    /// Metadata durability target sampled under mount mutation exclusion.
    /// Directory contents retain mount-wide sync until their journal ownership
    /// is tracked together with the inode-table entry.
    pub(crate) fn sync_transaction(&self) -> Option<Arc<RuntimeTransaction>> {
        self.with_state(|state| {
            if self.kind == InodeKind::RegularFile {
                state.sync_transaction.clone()
            } else {
                None
            }
        })
    }

    fn with_metadata<T>(&self, read: impl FnOnce(&Ext4InodeMetadata) -> T) -> T {
        self.with_state(|state| read(&state.metadata))
    }

    fn publish_metadata(&self, metadata: Ext4InodeMetadata) -> Ext4Result<()> {
        self.publish_metadata_with_timestamps(metadata, PendingInodeTimestamps::default(), None)
    }

    fn publish_metadata_with_timestamps(
        &self,
        metadata: Ext4InodeMetadata,
        persisted: PendingInodeTimestamps,
        transaction: Option<Arc<RuntimeTransaction>>,
    ) -> Ext4Result<()> {
        self.publish_metadata_and_quotas(metadata, persisted, transaction, None)
    }

    fn publish_metadata_and_quotas(
        &self,
        mut metadata: Ext4InodeMetadata,
        persisted: PendingInodeTimestamps,
        transaction: Option<Arc<RuntimeTransaction>>,
        records: Option<[Option<kquota::DquotRef>; 3]>,
    ) -> Ext4Result<()> {
        self.update_state(|state| {
            // Publication without an owner invalidates older sync evidence.
            // Xattr/recovery paths can therefore retain conservative sync until
            // they explicitly provide the transaction covering their mutation.
            state.sync_transaction = transaction;
            if metadata.block != state.metadata.block
                || metadata.disk_size < state.metadata.disk_size
            {
                self.clear_extent_proofs(state);
            }
            // Mapping/xattr publications must not restore older on-disk times
            // over an automatic update made while metadata I/O was in flight.
            for (current, incoming, dirty, captured) in [
                (
                    &state.metadata.atime,
                    &mut metadata.atime,
                    &mut state.has_dirty_atime,
                    persisted.atime,
                ),
                (
                    &state.metadata.mtime,
                    &mut metadata.mtime,
                    &mut state.has_dirty_mtime,
                    persisted.mtime,
                ),
                (
                    &state.metadata.ctime,
                    &mut metadata.ctime,
                    &mut state.has_dirty_ctime,
                    persisted.ctime,
                ),
            ] {
                if *dirty && captured != Some(*current) {
                    *incoming = *current;
                } else {
                    *dirty = false;
                }
            }
            state.metadata = metadata;
            if let Some(records) = records {
                state.dquots = records;
            }
        });
        Ok(())
    }

    /// Captures dirty values without retaining inode-state protection over I/O.
    pub(crate) fn pending_timestamps(&self) -> PendingInodeTimestamps {
        self.with_state(|state| PendingInodeTimestamps {
            atime: state.has_dirty_atime.then_some(state.metadata.atime),
            mtime: state.has_dirty_mtime.then_some(state.metadata.mtime),
            ctime: state.has_dirty_ctime.then_some(state.metadata.ctime),
        })
    }

    /// Returns whether an automatic attribute update still needs staging.
    pub(crate) fn has_dirty_timestamps(&self) -> bool {
        self.with_state(|state| {
            state.has_dirty_atime || state.has_dirty_mtime || state.has_dirty_ctime
        })
    }

    /// Forgets initialized-range and sync-target proofs before extent mutation,
    /// including mutations of external leaves that leave the inode root intact.
    /// The caller holds exclusive mount access through `&mut Ext4SbInfo`.
    pub(crate) fn invalidate_extent_status(&self) {
        self.update_state(|state| {
            self.clear_extent_proofs(state);
            // An external leaf rewrite need not publish an inode-table entry.
            // Until that mutation reports an owner, fsync must use the mount's
            // running transaction rather than an older inode-table target.
            state.sync_transaction = None;
        });
    }

    /// Drops the inode's reconstructible extent proofs and the published
    /// initialized-prefix summary in one step.
    ///
    /// - `state`: the inode state; the caller holds the inode-state write lock.
    ///
    /// Returns: nothing.
    ///
    /// Locking: caller must hold the inode-state write lock.
    fn clear_extent_proofs(&self, state: &mut Ext4InodeState) {
        state.extent_status.clear();
        self.initialized_extent_end.store(0, Ordering::Release);
    }

    /// Publishes the lock-free initialized-prefix proof when the extent-status
    /// tree currently proves a contiguous written prefix.
    ///
    /// - `state`: the inode state; the caller holds the inode-state write lock.
    /// - `block_size`: filesystem block size in bytes.
    ///
    /// Returns: nothing; the published end only grows.
    ///
    /// Locking: caller must hold the inode-state write lock. Evicting a proof
    /// never revokes an already published prefix because eviction does not
    /// change the mapping; only [`clear_extent_proofs`](Self::clear_extent_proofs)
    /// resets it.
    fn publish_initialized_extent_end(&self, state: &Ext4InodeState, block_size: u32) {
        let block_size = u64::from(block_size);
        if block_size == 0 {
            return;
        }
        let Some(proven_end) = state
            .extent_status
            .covered_prefix_end()
            .checked_mul(block_size)
        else {
            // Overflow would over-report the proof; withhold it instead.
            return;
        };
        if proven_end != 0 {
            self.initialized_extent_end
                .fetch_max(proven_end, Ordering::AcqRel);
        }
    }

    #[cfg(unittest)]
    pub(crate) fn cache_initialized_blocks(&self, start: u64, end: u64) {
        // Unit-test inode fixtures use 4 KiB blocks.
        const BLOCK_SIZE: u32 = 4096;
        self.update_state(|state| {
            state.extent_status.cache_written(start, end);
            self.publish_initialized_extent_end(state, BLOCK_SIZE);
        });
    }

    #[cfg(unittest)]
    fn has_initialized_overwrite(&self, start: u64, count: u64, block_size: u32) -> bool {
        self.with_state(|state| {
            state.has_initialized_overwrite(start, count, block_size, self.size())
        })
    }

    /// Checks whether a buffered write needs no mapping lookup or reservation.
    ///
    /// The caller holds KVFS data or invalidate protection against truncate and fallocate,
    /// and checks journal/mount writability separately. The inode-state guard
    /// observes mapping invalidation and size changes together. Misses retain
    /// the mount-protected reservation path, including partial EOF blocks.
    pub(crate) fn check_initialized_overwrite(
        &self,
        start: LogicalBlock,
        count: u64,
        block_size: u32,
        has_huge_file: bool,
    ) -> Ext4Result<bool> {
        self.with_state(|state| {
            regular_file_extent_mutation_supported(self.kind, self.flags(), has_huge_file)?;
            Ok(state.has_initialized_overwrite(start.get(), count, block_size, self.size()))
        })
    }

    pub(crate) fn orphan_location(&self) -> Option<OrphanLocation> {
        self.with_state(|state| state.orphan)
    }

    pub(crate) fn bind_orphan(&self, location: OrphanLocation) -> Ext4Result<()> {
        self.update_state(|state| match state.orphan {
            None => {
                state.orphan = Some(location);
                Ok(())
            }
            Some(current) if current == location => Ok(()),
            Some(_) => Err(Ext4Error::Corrupt(CorruptKind::InvalidInode)),
        })
    }

    pub(crate) fn clear_orphan(&self, expected: OrphanLocation) {
        self.update_state(|state| {
            debug_assert_eq!(state.orphan, Some(expected));
            state.orphan = None;
        });
    }

    /// Composes the VFS `NodeFlags`: disk-projected bits derived on read from
    /// the raw `flags` atomic, plus the runtime-only `NO_QUOTA`.
    ///
    /// Returns: the inode's VFS node flags; disk bits are re-derived each call,
    /// so no cached projection can drift from `flags`.
    ///
    /// Locking: lock-free; reads the `flags` and `no_quota` atoms, which are
    /// independent facts and not a single snapshot.
    pub(crate) fn node_flags(&self) -> kvfs::NodeFlags {
        let mut node_flags = node_flags_from_disk(self.flags.load(Ordering::Acquire));
        if self.no_quota.load(Ordering::Acquire) {
            node_flags.insert(kvfs::NodeFlags::NO_QUOTA);
        }
        node_flags
    }

    /// Sets the runtime-only quota bypass flag, like Linux setting or clearing
    /// `S_NOQUOTA` on the VFS inode.
    ///
    /// - `enabled`: whether quota accounting is bypassed for this inode.
    ///
    /// Returns: nothing.
    ///
    /// Locking: lock-free; readers use an acquire load.
    pub(crate) fn set_noquota(&self, enabled: bool) {
        self.no_quota.store(enabled, Ordering::Release);
    }

    /// Returns whether quota accounting is bypassed for this inode.
    ///
    /// Returns: `true` when the runtime-only `NO_QUOTA` flag is set.
    ///
    /// Locking: lock-free; reads the `no_quota` atomic.
    pub(crate) fn is_noquota(&self) -> bool {
        self.no_quota.load(Ordering::Acquire)
    }

    /// Returns this inode's number.
    pub fn number(&self) -> InodeNumber {
        self.number
    }

    /// Returns this inode's kind.
    ///
    /// Returns: the file type decoded when the inode was loaded.
    ///
    /// Locking: lock-free; the type bits are immutable for a resident inode.
    pub fn kind(&self) -> InodeKind {
        self.kind
    }

    /// Returns the raw Linux mode bits.
    ///
    /// Returns: the full `i_mode`, composed from the immutable type bits and
    /// the current permission bits.
    ///
    /// Locking: lock-free; reads the immutable `kind` and the `permissions`
    /// atomic.
    pub fn mode(&self) -> u16 {
        self.kind.mode_bits() | self.permissions.load(Ordering::Acquire)
    }

    /// Returns the inode owner's user ID.
    pub fn uid(&self) -> u32 {
        self.with_state(|state| state.uid())
    }

    /// Returns the inode owner's group ID.
    pub fn gid(&self) -> u32 {
        self.with_state(|state| state.gid())
    }

    /// Returns the inode owner's `(uid, gid)` pair under one state lock.
    pub fn owner(&self) -> (u32, u32) {
        self.with_state(|state| (state.uid(), state.gid()))
    }

    /// Returns the ext4 on-disk inode size in bytes.
    ///
    /// This is the kext4 equivalent of Linux ext4's `i_disksize`. A VFS inode
    /// may expose a newer visible `i_size` while buffered data is still waiting
    /// for ordered writeback.
    pub fn disk_size(&self) -> u64 {
        self.with_state(|state| state.disk_size())
    }

    /// Returns the VFS-visible inode size in bytes.
    ///
    /// Like Linux `i_size_read`, this does not acquire the mapping-state lock.
    /// Acquire makes changes preceding the observed size publication visible.
    /// This scalar read does not stabilize the size or the backing mappings;
    /// callers needing that exclusion must hold the VFS data/invalidate locks.
    pub fn size(&self) -> u64 {
        self.visible_size.load(Ordering::Acquire)
    }

    /// Samples `(visible_size, disk_size)` while excluding both size writers.
    ///
    /// The two sizes can legitimately differ during writeback or truncate;
    /// this snapshot does not combine their separate publication phases.
    pub fn sizes(&self) -> (u64, u64) {
        self.with_state(|state| (self.size(), state.disk_size()))
    }

    /// Updates the VFS-visible inode size without changing `i_disksize`.
    ///
    /// This is size publication, not a write or truncate transaction. For a
    /// resident regular file, KVFS calls it at the `write_end_set_size` or
    /// `truncate_setsize` ordering point with the inode data lock held; truncate
    /// also holds the mapping invalidate lock. Cache preparation, PTE teardown,
    /// and backing-block cleanup remain the responsibility of those paths.
    /// Non-regular inode mutations additionally require mount exclusion.
    ///
    /// This method always takes the state write lock, including when mount
    /// exclusion is held. That lock keeps shrink and extent-proof invalidation
    /// atomic with respect to mapping-state readers. Release publishes earlier
    /// changes to lockless size readers without providing cache/mapping exclusion.
    pub fn set_size(&self, size: u64) {
        self.update_state(|state| {
            if size < self.size() {
                self.clear_extent_proofs(state);
            }
            self.visible_size.store(size, Ordering::Release);
        });
    }

    /// Captures the generic inode attributes under one state lock.
    pub fn stat(&self) -> Ext4InodeStat {
        self.with_state(|state| {
            let metadata = &state.metadata;
            Ext4InodeStat {
                mode: self.mode(),
                uid: metadata.uid,
                gid: metadata.gid,
                size: self.size(),
                blocks: metadata.blocks,
                rdev: device_id_from_metadata(self.kind, metadata),
                links_count: metadata.links_count,
                atime: metadata.atime,
                ctime: metadata.ctime,
                mtime: metadata.mtime,
            }
        })
    }

    pub(crate) fn project_id(&self) -> u32 {
        self.with_metadata(|metadata| metadata.project_id)
    }

    pub(crate) fn inherited_project_id(&self, has_project: bool) -> u32 {
        if has_project && self.flags() & disk_inode::EXT4_PROJINHERIT_FL != 0 {
            self.project_id()
        } else {
            0
        }
    }

    /// Sets the POSIX permission and special bits, like Linux `chmod` updating
    /// only `i_mode` below the type mask.
    ///
    /// - `permission`: new `i_mode` bits; type bits are ignored (`& 0o7777`).
    ///
    /// Returns: nothing.
    ///
    /// Locking: lock-free; readers use an acquire load. The stored value never
    /// carries type bits, so the immutable `kind` cannot be diverged.
    pub fn set_permission(&self, permission: u16) {
        self.permissions
            .store(permission & 0o7777, Ordering::Release);
    }

    /// Updates the owner in the single resident inode state.
    pub fn set_owner(&self, uid: u32, gid: u32) {
        self.update_state(|state| {
            state.metadata.uid = uid;
            state.metadata.gid = gid;
        });
    }

    /// Updates the link count in the single resident inode state.
    pub fn set_links_count(&self, link_count: u64) {
        let link_count = u16::try_from(link_count).unwrap_or(u16::MAX);
        self.update_state(|state| state.metadata.links_count = link_count);
    }

    /// Increments the resident link count.
    pub fn increment_links_count(&self) {
        self.update_state(|state| {
            state.metadata.links_count = state.metadata.links_count.saturating_add(1);
        });
    }

    /// Decrements the resident link count.
    pub fn decrement_links_count(&self) {
        self.update_state(|state| {
            debug_assert_ne!(state.metadata.links_count, 0);
            state.metadata.links_count = state.metadata.links_count.saturating_sub(1);
        });
    }

    /// Updates the resident access time and marks changed values for writeback.
    pub fn set_atime(&self, atime: Ext4Timestamp) {
        self.update_state(|state| {
            state.has_dirty_atime |= state.metadata.atime != atime;
            state.metadata.atime = atime;
        });
    }

    /// Updates the resident modification time and marks changes for writeback.
    pub fn set_mtime(&self, mtime: Ext4Timestamp) {
        self.update_state(|state| {
            state.has_dirty_mtime |= state.metadata.mtime != mtime;
            state.metadata.mtime = mtime;
        });
    }

    /// Updates the resident status-change time and marks changes for writeback.
    pub fn set_ctime(&self, ctime: Ext4Timestamp) {
        self.update_state(|state| {
            state.has_dirty_ctime |= state.metadata.ctime != ctime;
            state.metadata.ctime = ctime;
        });
    }

    /// Updates automatic write timestamps together, returning whether this is a
    /// regular inode. Other inode kinds keep their journaled update path.
    pub(crate) fn update_regular_write_timestamps(&self, timestamp: Ext4Timestamp) -> bool {
        self.update_state(|state| {
            if self.kind != InodeKind::RegularFile {
                return false;
            }
            state.publish_write_timestamps(timestamp);
            true
        })
    }

    /// Clips a regular buffered write and publishes its automatic timestamps.
    ///
    /// The caller holds KVFS data protection and has checked mount/journal
    /// writability. `None` means the nonempty write starts at or beyond the
    /// limit; rejected and empty requests leave timestamps unchanged.
    ///
    /// The clip reads only atomically published flags and caller-supplied
    /// limits, so it runs without the state lock. Linux likewise separates the
    /// two duties and skips timestamp work when nothing changed
    /// (`need_cmtime_update`): the resident pair is compared in the read
    /// section and the write section runs only when the sampled timestamp
    /// differs. Repeated writes inside one coarse clock interval therefore
    /// never enter the write section.
    ///
    /// The comparison is stable across the two sections for the writers that
    /// can change a resident write timestamp: buffered writes
    /// (`write_iter_with_prepare`), `set_times`, `fallocate`, truncate and
    /// xattr hold the inode data lock exclusively, and writable faults hold it
    /// shared. Metadata paths outside that lock (chmod/chown ctime auto-fill,
    /// quota flag updates, unlink) can publish a different value; when they
    /// interleave, skipping keeps the value they published instead of
    /// overwriting it with this write's older sample.
    pub(crate) fn prepare_buffered_write(
        &self,
        offset: u64,
        count: usize,
        extent_max_size: u64,
        bitmap_max_size: u64,
        timestamp: Ext4Timestamp,
    ) -> Option<usize> {
        debug_assert_eq!(self.kind, InodeKind::RegularFile);
        if count == 0 {
            return Some(0);
        }
        let max_size = inode_max_file_size(self.flags(), extent_max_size, || bitmap_max_size);
        if offset >= max_size {
            return None;
        }
        let remaining = usize::try_from(max_size - offset).unwrap_or(usize::MAX);
        if self.with_state(|state| state.needs_write_timestamp(timestamp)) {
            self.update_state(|state| state.publish_write_timestamps(timestamp));
        }
        Some(count.min(remaining))
    }

    /// Updates generic allocated-block accounting in the resident inode.
    pub fn set_allocated_bytes(&self, bytes: u64) {
        debug_assert_eq!(bytes % 512, 0);
        self.update_state(|state| state.metadata.blocks = bytes / 512);
    }

    /// Adds allocated bytes to the resident block count.
    pub fn add_allocated_bytes(&self, bytes: u64) {
        debug_assert_eq!(bytes % 512, 0);
        self.update_state(|state| {
            state.metadata.blocks = state.metadata.blocks.saturating_add(bytes / 512);
        });
    }

    /// Subtracts allocated bytes from the resident block count.
    pub fn subtract_allocated_bytes(&self, bytes: u64) {
        debug_assert_eq!(bytes % 512, 0);
        self.update_state(|state| {
            state.metadata.blocks = state.metadata.blocks.saturating_sub(bytes / 512);
        });
    }

    /// Returns the raw ext4 block accounting value.
    pub fn blocks(&self) -> u64 {
        self.with_metadata(|metadata| metadata.blocks)
    }

    pub(crate) fn blocks_with_delalloc(&self, block_size: u64) -> Ext4Result<u64> {
        self.with_state(|state| {
            state
                .reserved_data_blocks
                .checked_mul(block_size / 512)
                .and_then(|reserved| state.metadata.blocks.checked_add(reserved))
                .ok_or(Ext4Error::Overflow)
        })
    }

    /// Returns the raw ext4 inode flags.
    ///
    /// Returns: `i_flags` published by the last [`set_flags`](Self::set_flags).
    ///
    /// Locking: lock-free; the single writer publishes under the inode-state
    /// write lock and readers use an acquire load.
    pub fn flags(&self) -> u32 {
        self.flags.load(Ordering::Acquire)
    }

    /// Returns an error when publishing `flags` would change the immutable
    /// `EXT4_EXTENTS_FL` mapping-format bit.
    ///
    /// - `flags`: candidate `i_flags`; non-format bits may differ freely.
    ///
    /// Returns: nothing when the update keeps the mapping format.
    ///
    /// Errors: `Corrupt` when `EXT4_EXTENTS_FL` would change, which would
    /// invalidate the lock-free mapping-interpreter reads.
    ///
    /// Locking: lock-free; reads the `flags` atomic.
    pub(crate) fn ensure_flags_publishable(&self, flags: u32) -> Ext4Result<()> {
        if (flags ^ self.flags.load(Ordering::Acquire)) & disk_inode::EXT4_EXTENTS_FL != 0 {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInode));
        }
        Ok(())
    }

    /// Publishes the inode's raw `i_flags` after an on-disk flags update.
    ///
    /// - `flags`: the new `i_flags` written to disk; the mapping-format bit
    ///   `EXT4_EXTENTS_FL` must equal the resident value.
    ///
    /// Returns: nothing.
    ///
    /// Errors: `Corrupt` when `EXT4_EXTENTS_FL` changed, which would invalidate
    /// the lock-free mapping-interpreter reads that depend on its immutability.
    ///
    /// Locking: caller must not hold the inode-state lock; this takes the write
    /// lock internally.
    pub(crate) fn set_flags(&self, flags: u32) -> Ext4Result<()> {
        self.update_state(|_state| {
            self.ensure_flags_publishable(flags)?;
            self.flags.store(flags, Ordering::Release);
            Ok(())
        })
    }

    /// Returns whether the inode carries the ext4 immutable flag.
    ///
    /// Returns: `true` when `EXT4_IMMUTABLE_FL` is set.
    ///
    /// Locking: lock-free; reads the published `i_flags` atomic.
    pub fn is_immutable(&self) -> bool {
        self.flags() & disk_inode::EXT4_IMMUTABLE_FL != 0
    }

    /// Returns whether the inode carries the ext4 append-only flag.
    ///
    /// Returns: `true` when `EXT4_APPEND_FL` is set.
    ///
    /// Locking: lock-free; reads the published `i_flags` atomic.
    pub fn is_append_only(&self) -> bool {
        self.flags() & disk_inode::EXT4_APPEND_FL != 0
    }

    pub(crate) fn mapping_root(&self) -> InodeMappingRoot {
        self.with_metadata(|metadata| InodeMappingRoot {
            inode_flags: self.flags(),
            bytes: metadata.block,
            generation: metadata.generation,
        })
    }

    pub(crate) fn file_acl_block(&self) -> u64 {
        self.with_metadata(|metadata| metadata.file_acl)
    }

    pub(crate) fn extra_isize(&self) -> u16 {
        self.with_metadata(|metadata| metadata.extra_isize)
    }

    pub(crate) fn is_extra_isize_expansion_disabled(&self) -> bool {
        self.with_state(|state| state.is_extra_isize_expansion_disabled)
    }

    pub(crate) fn disable_extra_isize_expansion(&self) {
        self.update_state(|state| state.is_extra_isize_expansion_disabled = true);
    }

    pub(crate) fn enable_extra_isize_expansion(&self) {
        self.update_state(|state| state.is_extra_isize_expansion_disabled = false);
    }

    pub(crate) fn inline_xattr_bytes(&self) -> Vec<u8> {
        self.with_metadata(|metadata| metadata.inline_xattr.clone())
    }

    /// Returns the inode generation number.
    pub fn generation(&self) -> u32 {
        self.with_metadata(|metadata| metadata.generation)
    }

    /// Returns the link count from the inode table entry.
    pub fn links_count(&self) -> u16 {
        self.with_metadata(|metadata| metadata.links_count)
    }

    /// Returns the last-access timestamp.
    pub fn atime(&self) -> Ext4Timestamp {
        self.with_metadata(|metadata| metadata.atime)
    }

    /// Returns the last-status-change timestamp.
    pub fn ctime(&self) -> Ext4Timestamp {
        self.with_metadata(|metadata| metadata.ctime)
    }

    /// Returns the last-modification timestamp.
    pub fn mtime(&self) -> Ext4Timestamp {
        self.with_metadata(|metadata| metadata.mtime)
    }

    /// Returns the raw inode `i_block` bytes.
    ///
    /// The interpretation depends on the inode kind and flags: extent root,
    /// block map, fast symlink target, or device number.
    pub fn raw_i_block(&self) -> [u8; disk_inode::INODE_BLOCK_BYTES] {
        self.with_metadata(|metadata| metadata.block)
    }

    /// Linux `ext4_can_truncate`: classification is independent of which
    /// on-disk mapping formats this implementation can currently mutate.
    pub(crate) fn can_truncate(&self, block_size: u32, has_ea_inode: bool) -> Ext4Result<bool> {
        self.with_metadata(|metadata| match self.kind {
            InodeKind::RegularFile | InodeKind::Directory => Ok(true),
            InodeKind::Symlink => {
                Ok(!metadata.is_fast_symlink(self.kind, self.flags(), block_size, has_ea_inode)?)
            }
            _ => Ok(false),
        })
    }

    pub(crate) fn fast_symlink_target(
        &self,
        filesystem_block_size: u32,
        has_ea_inode_feature: bool,
    ) -> Ext4Result<Option<Vec<u8>>> {
        let metadata = self.metadata_snapshot();
        if self.kind != InodeKind::Symlink {
            return Err(Ext4Error::Unsupported(UnsupportedKind::InodeKind));
        }
        let flags = self.flags();
        if flags & disk_inode::EXT4_INLINE_DATA_FL != 0 {
            return Err(Ext4Error::Unsupported(UnsupportedKind::InlineData));
        }
        if flags & disk_inode::EXT4_ENCRYPT_FL != 0 {
            return Err(Ext4Error::Unsupported(UnsupportedKind::EncryptedName));
        }

        let is_fast = metadata.is_fast_symlink(
            self.kind,
            flags,
            filesystem_block_size,
            has_ea_inode_feature,
        )?;
        if is_fast {
            Ok(Some(
                Self::validate_fast_symlink_target(&metadata)?.to_vec(),
            ))
        } else {
            Ok(None)
        }
    }

    fn validate_fast_symlink_target(metadata: &Ext4InodeMetadata) -> Ext4Result<&[u8]> {
        let size = usize::try_from(metadata.disk_size).map_err(|_| Ext4Error::Overflow)?;
        if size == 0 || size >= disk_inode::INODE_BLOCK_BYTES {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInode));
        }

        let target_with_terminator = metadata
            .block
            .get(..=size)
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidInode))?;
        if target_with_terminator.iter().position(|byte| *byte == 0) != Some(size) {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInode));
        }

        metadata
            .block
            .get(..size)
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidInode))
    }

    /// Returns the device number encoded in a character or block device inode.
    pub fn device_id(&self) -> Option<Ext4DeviceId> {
        self.with_metadata(|metadata| device_id_from_metadata(self.kind, metadata))
    }

    /// Returns whether this inode uses the ext4 extent-tree format.
    pub fn has_extents(&self) -> bool {
        self.flags() & disk_inode::EXT4_EXTENTS_FL != 0
    }

    pub(crate) fn uses_huge_file_accounting(&self) -> bool {
        self.flags() & disk_inode::EXT4_HUGE_FILE_FL != 0
    }

    pub(crate) fn has_indexed_directory(&self) -> bool {
        self.flags() & disk_inode::EXT4_INDEX_FL != 0
    }

    #[cfg(unittest)]
    fn unreserved_delalloc_extents(&self, start: u64, end: u64) -> Vec<(u64, u64)> {
        self.with_state(|state| unreserved_delalloc_extents_in_state(state, start, end))
    }

    #[cfg(unittest)]
    fn insert_unreserved_delalloc_extents(&self, extents: &[(u64, u64)]) -> Ext4Result<u64> {
        self.update_state(|state| {
            let new_total = validate_unreserved_delalloc_extents_in_state(state, extents)?;
            let inserted = new_total - state.reserved_data_blocks;
            insert_unreserved_delalloc_extents_in_state(state, extents, new_total);
            Ok(inserted)
        })
    }

    /// Reserves unallocated blocks in one logical range for delayed allocation.
    ///
    /// The scan-map-admission-insert sequence runs inside one critical
    /// section holding the inode-state write lock, so two racing writers on
    /// the same inode cannot both observe the same unreserved hole: the
    /// overlapping-insert state that the mount-wide write lock used to
    /// exclude is now structurally impossible, and reserves on different
    /// inodes no longer serialize. Per-inode reservations and the mount-wide
    /// aggregate are updated as one ext4-owned invariant; already allocated
    /// or already reserved subranges consume no additional reservation.
    /// Extent and counter validation precede quota and mount admission, so
    /// publishing admitted reservations cannot return an insertion error.
    ///
    /// Returns: the logical block ranges this call newly reserved, in ascending
    /// order. Subranges already covered by a reservation, or proven inside the
    /// initialized extent prefix, are not listed: the caller may only cancel
    /// what it owns.
    ///
    /// # Locking
    ///
    /// The closure must not call back into `Ext4Inode` state accessors (see
    /// [`update_state`](Self::update_state)); mapping queries run
    /// through [`Ext4SbInfo::map_blocks_with_root`] with a root sampled from
    /// the held state. Mount-wide admission takes the allocator and delalloc
    /// mutexes inside the state lock, matching the release-path lock order.
    ///
    /// # Errors
    ///
    /// Returns an extent-validation, overflow, accounting, or no-space error
    /// without publishing a partial mount reservation.
    #[expect(
        clippy::excessive_nesting,
        reason = "CG-029-shallow-control-flow: TODO(cg): legacy delayed-allocation reservation; \
                  move the per-extent mapping walk and hole coalescing into a helper"
    )]
    pub(crate) fn reserve_delalloc_range_into(
        &self,
        sb: &Ext4SbInfo,
        start: LogicalBlock,
        block_count: u64,
    ) -> Ext4Result<Vec<(u64, u64)>> {
        let end = start
            .get()
            .checked_add(block_count)
            .ok_or(Ext4Error::Overflow)?;
        // The whole range lies inside the prefix proven allocated and
        // initialized, so the slow path below would only re-prove it and return
        // without reserving: its mapping walk finds every block `Mapped` (never a
        // hole), leaves `reserved_data_blocks` unchanged, and returns `Ok(())`.
        // A published prefix also implies the slow path already passed the
        // regular-file extent-mutation admission check, and `kind` plus
        // `EXT4_EXTENTS_FL` are immutable. This bypass deliberately does not
        // repeat the bookkeeping self-check (`reserved_data_blocks >
        // delalloc_reserved_blocks`); that defensive guard still runs for every
        // range that misses the prefix.
        let block_size = u64::from(sb.layout().block_size());
        if let Some(range_end_bytes) = end.checked_mul(block_size)
            && range_end_bytes != 0
            && range_end_bytes <= self.initialized_extent_end.load(Ordering::Acquire)
        {
            return Ok(Vec::new());
        }
        self.update_state(|state| {
            regular_file_extent_mutation_supported(
                self.kind,
                self.flags(),
                sb.superblock().features().has_huge_file(),
            )?;
            if state.has_initialized_overwrite(
                start.get(),
                block_count,
                sb.layout().block_size(),
                self.size(),
            ) {
                self.publish_initialized_extent_end(state, sb.layout().block_size());
                return Ok(Vec::new());
            }
            if state.reserved_data_blocks > sb.delalloc_reserved_blocks() {
                return Err(Ext4Error::InvalidDelayedAllocationState);
            }
            if block_count == 0 {
                return Ok(Vec::new());
            }
            let root = InodeMappingRoot {
                inode_flags: self.flags(),
                bytes: state.metadata.block,
                generation: state.metadata.generation,
            };
            let mut holes = Vec::new();
            for (mut logical, extent_end) in
                unreserved_delalloc_extents_in_state(state, start.get(), end)
            {
                while logical < extent_end {
                    let mapping =
                        sb.map_blocks_with_root(self, &root, LogicalBlock::new(logical))?;
                    let mapping_len = match mapping {
                        BlockMapping::Hole { len, .. }
                        | BlockMapping::Mapped { len, .. }
                        | BlockMapping::Unwritten { len, .. } => u64::from(len.get()),
                    };
                    if mapping_len == 0 {
                        return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
                    }
                    let covered = mapping_len.min(extent_end - logical);
                    let next = logical.checked_add(covered).ok_or(Ext4Error::Overflow)?;
                    if matches!(mapping, BlockMapping::Mapped { .. }) {
                        let mapped_end = logical
                            .checked_add(mapping_len)
                            .ok_or(Ext4Error::Overflow)?;
                        state.extent_status.cache_written(logical, mapped_end);
                    }
                    if matches!(mapping, BlockMapping::Hole { .. }) {
                        if let Some((_, previous_end)) = holes.last_mut()
                            && *previous_end == logical
                        {
                            *previous_end = next;
                        } else {
                            holes.push((logical, next));
                        }
                    }
                    logical = next;
                }
            }
            self.publish_initialized_extent_end(state, sb.layout().block_size());

            let new_reserved_data_blocks =
                validate_unreserved_delalloc_extents_in_state(state, &holes)?;
            let reserved = new_reserved_data_blocks - state.reserved_data_blocks;
            // An empty reservation skips mount-wide accounting entirely: the
            // mutex pair guards the aggregate against concurrent reserves, not
            // an empty range against itself.
            if reserved == 0 {
                return Ok(Vec::new());
            }
            // Check-then-act admission stays inside one critical section so the
            // capacity check sees the same snapshot that the accumulation
            // updates; otherwise two concurrent reserves could both pass on a
            // stale free/reserved view and over-reserve the mount-wide total,
            // starving later writes and underreporting statfs. Lock order stays
            // allocator then delalloc, matching statfs.
            sb.reserve_quota_space(&state.dquots, reserved)?;
            let admission = (|| {
                let allocator = sync::mutex_lock(&sb.allocator);
                let mut delalloc_reserved_blocks = sync::mutex_lock(&sb.delalloc_reserved_blocks);
                let blocks_available = allocator
                    .free_blocks_count
                    .saturating_sub(*delalloc_reserved_blocks)
                    .saturating_sub(sb.superblock().reserved_blocks_count());
                if reserved > blocks_available {
                    return Err(Ext4Error::NoSpace);
                }
                *delalloc_reserved_blocks = delalloc_reserved_blocks
                    .checked_add(reserved)
                    .ok_or(Ext4Error::Overflow)?;
                Ok(())
            })();
            if let Err(error) = admission {
                if let Err(cleanup_error) = sb.release_quota_reservation(&state.dquots, reserved) {
                    error!(
                        "KExt4 delayed reservation rollback after {error:?} failed: \
                         {cleanup_error:?}"
                    );
                }
                return Err(error);
            }
            insert_unreserved_delalloc_extents_in_state(state, &holes, new_reserved_data_blocks);
            Ok(holes)
        })
    }

    pub(crate) fn reserved_delalloc_in_range(
        &self,
        start: LogicalBlock,
        count: u64,
    ) -> Ext4Result<u64> {
        let end = start.get().checked_add(count).ok_or(Ext4Error::Overflow)?;
        self.with_state(|state| {
            Ok(overlapping_delalloc_extents(state, start.get(), end)
                .map(|(first, last)| last.min(end) - first.max(start.get()))
                .sum())
        })
    }

    fn remove_delalloc_extent(&self, start: u64, end: u64) -> Ext4Result<u64> {
        self.remove_delalloc_extent_with(start, end, |_, _| Ok(()))
    }

    fn remove_delalloc_extent_with(
        &self,
        start: u64,
        end: u64,
        before_remove: impl FnOnce(&mut Ext4InodeState, u64) -> Ext4Result<()>,
    ) -> Ext4Result<u64> {
        if start >= end {
            return Ok(0);
        }
        self.update_state(|state| -> Ext4Result<u64> {
            let overlapping = overlapping_delalloc_extents(state, start, end).collect::<Vec<_>>();
            let released = overlapping.iter().try_fold(
                0u64,
                |total, &(extent_start, extent_end)| -> Ext4Result<u64> {
                    let released_start = extent_start.max(start);
                    let released_end = extent_end.min(end);
                    total
                        .checked_add(
                            released_end
                                .checked_sub(released_start)
                                .ok_or(Ext4Error::Overflow)?,
                        )
                        .ok_or(Ext4Error::Overflow)
                },
            )?;
            let new_reserved_data_blocks = state
                .reserved_data_blocks
                .checked_sub(released)
                .ok_or(Ext4Error::InvalidDelayedAllocationState)?;
            before_remove(state, released)?;

            for (extent_start, extent_end) in overlapping {
                state.delayed_extents.remove(&extent_start);
                let released_start = extent_start.max(start);
                let released_end = extent_end.min(end);
                if extent_start < released_start {
                    state.delayed_extents.insert(extent_start, released_start);
                }
                if released_end < extent_end {
                    state.delayed_extents.insert(released_end, extent_end);
                }
            }
            state.reserved_data_blocks = new_reserved_data_blocks;
            Ok(released)
        })
    }

    fn remove_delalloc_from(&self, first_block: u64) -> Ext4Result<u64> {
        self.remove_delalloc_extent(first_block, u64::MAX)
    }

    fn clear_delalloc_reservations(&self) -> u64 {
        self.update_state(|state| {
            let reserved = state.reserved_data_blocks;
            state.delayed_extents.clear();
            state.reserved_data_blocks = 0;
            reserved
        })
    }

    pub(crate) fn reserved_data_blocks(&self) -> u64 {
        self.with_state(|state| state.reserved_data_blocks)
    }

    pub(crate) fn has_delalloc_reservations(&self) -> bool {
        self.with_state(|state| state.reserved_data_blocks != 0)
    }

    pub(crate) fn next_delalloc_extent(&self, start: u64) -> Option<(u64, u64)> {
        self.with_state(|state| {
            if let Some((_, extent_end)) = state
                .delayed_extents
                .range(..=start)
                .next_back()
                .filter(|(_, extent_end)| **extent_end > start)
            {
                return Some((start, *extent_end));
            }
            state
                .delayed_extents
                .range(start..)
                .next()
                .map(|(extent_start, extent_end)| (*extent_start, *extent_end))
        })
    }
}

impl fmt::Debug for Ext4Inode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Ext4Inode")
            .field("number", &self.number())
            .field("flags", &self.flags())
            .field("metadata", &self.metadata_snapshot())
            .finish_non_exhaustive()
    }
}

/// Returns the maximal disjoint sub-ranges of `[start, end)` not yet covered
/// by delayed-allocation reservations, in ascending order; an empty result
/// means the whole range is already reserved. Operates on the borrowed state
/// directly — the caller already holds the inode-state lock.
fn overlapping_delalloc_extents(
    state: &Ext4InodeState,
    start: u64,
    end: u64,
) -> impl Iterator<Item = (u64, u64)> + '_ {
    let covering = state
        .delayed_extents
        .range(..start)
        .next_back()
        .filter(|(_, last)| **last > start)
        .map(|(&first, &last)| (first, last));
    covering.into_iter().chain(
        state
            .delayed_extents
            .range(start..end)
            .map(|(&first, &last)| (first, last)),
    )
}

fn unreserved_delalloc_extents_in_state(
    state: &Ext4InodeState,
    start: u64,
    end: u64,
) -> Vec<(u64, u64)> {
    let mut extents = Vec::new();
    let mut cursor = start;
    for (extent_start, extent_end) in overlapping_delalloc_extents(state, start, end) {
        if extent_end <= cursor {
            continue;
        }
        if extent_start > cursor {
            extents.push((cursor, extent_start.min(end)));
        }
        cursor = cursor.max(extent_end);
        if cursor >= end {
            break;
        }
    }
    if cursor < end {
        extents.push((cursor, end));
    }
    extents
}

fn validate_unreserved_delalloc_extents_in_state(
    state: &Ext4InodeState,
    extents: &[(u64, u64)],
) -> Ext4Result<u64> {
    let mut previous_end = None;
    let newly_reserved = extents.iter().try_fold(
        0u64,
        |total, &(extent_start, extent_end)| -> Ext4Result<u64> {
            if extent_start >= extent_end
                || previous_end.is_some_and(|previous_end| previous_end > extent_start)
            {
                return Err(Ext4Error::InvalidDelayedAllocationState);
            }
            let overlaps_left = state
                .delayed_extents
                .range(..extent_end)
                .next_back()
                .is_some_and(|(_, reserved_end)| *reserved_end > extent_start);
            if overlaps_left {
                return Err(Ext4Error::InvalidDelayedAllocationState);
            }
            previous_end = Some(extent_end);
            total
                .checked_add(
                    extent_end
                        .checked_sub(extent_start)
                        .ok_or(Ext4Error::Overflow)?,
                )
                .ok_or(Ext4Error::Overflow)
        },
    )?;
    state
        .reserved_data_blocks
        .checked_add(newly_reserved)
        .ok_or(Ext4Error::Overflow)
}

fn insert_unreserved_delalloc_extents_in_state(
    state: &mut Ext4InodeState,
    extents: &[(u64, u64)],
    new_reserved_data_blocks: u64,
) {
    for &(extent_start, extent_end) in extents {
        let left = state
            .delayed_extents
            .range(..extent_start)
            .next_back()
            .filter(|(_, reserved_end)| **reserved_end == extent_start)
            .map(|(&reserved_start, _)| reserved_start);
        let right = state.delayed_extents.remove(&extent_end);
        let merged_start = left.unwrap_or(extent_start);
        if let Some(left) = left {
            state.delayed_extents.remove(&left);
        }
        state
            .delayed_extents
            .insert(merged_start, right.unwrap_or(extent_end));
    }
    state.reserved_data_blocks = new_reserved_data_blocks;
}

/// Shared admission rule for mutating regular-file extent mappings.
///
/// Called both outside the inode-state lock (VFS entry checks) and inside it
/// (`reserve_delalloc_range` reads the fields directly from the state), so
/// the check takes values instead of an inode to stay lock-free.
pub(crate) fn regular_file_extent_mutation_supported(
    kind: InodeKind,
    inode_flags: u32,
    has_huge_file_feature: bool,
) -> Ext4Result<()> {
    if kind != InodeKind::RegularFile {
        return Err(Ext4Error::Unsupported(UnsupportedKind::InodeKind));
    }
    if inode_flags & disk_inode::EXT4_EXTENTS_FL == 0 {
        return Err(Ext4Error::Unsupported(UnsupportedKind::NonExtentInode));
    }
    if inode_flags & disk_inode::EXT4_HUGE_FILE_FL != 0 {
        if !has_huge_file_feature {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInode));
        }
        return Err(Ext4Error::Unsupported(UnsupportedKind::HugeFile));
    }
    Ok(())
}

fn device_id_from_metadata(kind: InodeKind, metadata: &Ext4InodeMetadata) -> Option<Ext4DeviceId> {
    if kind != InodeKind::CharacterDevice && kind != InodeKind::BlockDevice {
        return None;
    }

    let old_encoded = u32::from_le_bytes([
        metadata.block[0],
        metadata.block[1],
        metadata.block[2],
        metadata.block[3],
    ]);
    if old_encoded != 0 {
        return Some(decode_old_device_id(old_encoded));
    }
    Some(decode_new_device_id(u32::from_le_bytes([
        metadata.block[4],
        metadata.block[5],
        metadata.block[6],
        metadata.block[7],
    ])))
}

fn decode_old_device_id(value: u32) -> Ext4DeviceId {
    Ext4DeviceId {
        major: (value >> 8) & 0xff,
        minor: value & 0xff,
    }
}

fn decode_new_device_id(value: u32) -> Ext4DeviceId {
    Ext4DeviceId {
        major: (value & 0x000f_ff00) >> 8,
        minor: (value & 0x0000_00ff) | ((value >> 12) & 0x000f_ff00),
    }
}

fn encode_new_device_id(device: Ext4DeviceId) -> Ext4Result<u32> {
    ensure_ext4_device_id_representable(device)?;
    Ok((device.minor & 0xff) | (device.major << 8) | ((device.minor & !0xff) << 12))
}

fn ensure_ext4_device_id_representable(device: Ext4DeviceId) -> Ext4Result<()> {
    if device.major > EXT4_ENCODED_DEV_MAJOR_MAX || device.minor > EXT4_ENCODED_DEV_MINOR_MAX {
        return Err(Ext4Error::Unsupported(UnsupportedKind::DeviceId));
    }
    Ok(())
}

impl Ext4SbInfo {
    /// Returns an inline fast-symlink target, or `None` for a block-mapped target.
    pub(crate) fn fast_symlink_target(&self, inode: &Ext4Inode) -> Ext4Result<Option<Vec<u8>>> {
        inode.fast_symlink_target(
            self.layout().block_size(),
            self.superblock().features().has_ea_inode(),
        )
    }

    /// Decodes filesystem-private state for a public inode number.
    ///
    /// KVFS callers must invoke this only from the initializer reserved by its
    /// inode cache. KExt4 deliberately does not maintain a second resident
    /// identity table. Zero-link inodes are rejected on this namespace-facing
    /// path.
    ///
    /// # Errors
    ///
    /// Returns a typed corruption or unsupported-format error for an invalid
    /// inode table entry.
    pub(crate) fn load_inode_private(&self, number: InodeNumber) -> Ext4Result<Ext4Inode> {
        let inode_number = number.get();
        if inode_number == 0 || inode_number > self.superblock().inodes_count() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeNumber));
        }
        if !self.is_public_inode_number(number) {
            return Err(Ext4Error::Unsupported(UnsupportedKind::ReservedInode));
        }
        self.iget_inner(number, false, true)
    }

    /// Loads an inode through the internal path used for inodes this
    /// filesystem manages itself: freshly initialized inodes whose block root
    /// is still zeroed, and the internal journal inode which is read before
    /// the mount-time system zones are installed. Block-root validation is
    /// skipped on this path, mirroring Linux where the journal inode is
    /// loaded on a dedicated path before the system zones are set up.
    pub(crate) fn internal_iget(&self, number: InodeNumber) -> Ext4Result<Ext4Inode> {
        self.iget_inner(number, false, false)
    }

    pub(crate) fn special_iget(&self, number: InodeNumber) -> Ext4Result<Ext4Inode> {
        if !self.is_special_inode_number(number) {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeNumber));
        }
        self.iget_inner(number, false, true)
    }

    /// Returns `None` only for an invalid orphan reference. Metadata read,
    /// decode, checksum and unsupported cleanup errors remain distinguishable.
    pub(crate) fn orphan_iget_with_next(
        &self,
        number: InodeNumber,
    ) -> Ext4Result<Option<(Ext4Inode, Option<InodeNumber>)>> {
        if !self.is_valid_orphan_number(number) || !self.is_inode_allocated(number)? {
            return Ok(None);
        }
        let raw = self.raw_inode(number)?;
        let next = match raw.dtime() {
            0 => None,
            next => Some(InodeNumber::new(next)),
        };
        let inode = self.iget_from_raw(number, raw, true, true)?;
        if next.is_some_and(|next| next.get() > self.superblock().inodes_count())
            || (inode.links_count() != 0
                && !inode.can_truncate(
                    self.layout().block_size(),
                    self.superblock().features().has_ea_inode(),
                )?)
        {
            return Ok(None);
        }
        if inode.links_count() == 0 {
            self.ensure_unlinked_inode_eviction_supported(&inode)?;
        } else {
            self.ensure_regular_file_mutation_supported(&inode)?;
            self.ensure_inode_data_truncation_supported(&inode)?;
        }
        Ok(Some((inode, next)))
    }

    fn iget_inner(
        &self,
        number: InodeNumber,
        allow_zero_links: bool,
        validate_block_root: bool,
    ) -> Ext4Result<Ext4Inode> {
        let raw = self.raw_inode(number)?;
        self.iget_from_raw(number, raw, allow_zero_links, validate_block_root)
    }

    fn iget_from_raw(
        &self,
        number: InodeNumber,
        raw: disk_inode::RawInode,
        allow_zero_links: bool,
        validate_block_root: bool,
    ) -> Ext4Result<Ext4Inode> {
        let DecodedInodeFields {
            kind,
            flags,
            permissions,
            metadata,
        } = Ext4InodeMetadata::from_raw(
            raw,
            allow_zero_links,
            self.superblock().features().has_project(),
        )?;
        let inode = Ext4Inode::new(number, kind, flags, permissions, metadata);
        if validate_block_root {
            self.validate_inode_block_root(&inode)?;
        }
        Ok(inode)
    }

    /// Linux `ext4_ind_check_inode` equivalent, run once at inode load time
    /// before the inode object is published: for legacy (non-extent) inodes
    /// whose kind uses a block map, all 12 direct block pointers in `i_block[]`
    /// are validated against the system zones. Extent-format inodes and kinds
    /// that do not use `i_block[]` as a block map (e.g. fast symlinks) are
    /// left to the mapping path.
    fn validate_inode_block_root(&self, inode: &Ext4Inode) -> Ext4Result<()> {
        if inode.has_extents() {
            return Ok(());
        }
        match inode.kind() {
            InodeKind::RegularFile | InodeKind::Directory => {
                self.check_legacy_direct_blocks(inode.number(), &inode.raw_i_block())
            }
            _ => Ok(()),
        }
    }

    fn ensure_delalloc_accounting(&self, inode: &Ext4Inode) -> Ext4Result<()> {
        if inode.reserved_data_blocks() <= self.delalloc_reserved_blocks() {
            Ok(())
        } else {
            Err(Ext4Error::InvalidDelayedAllocationState)
        }
    }

    /// Reserves unallocated blocks in one logical range for delayed allocation.
    ///
    /// Delegates to [`Ext4Inode::reserve_delalloc_range_into`], which serializes
    /// the reserve sequence on the per-inode state lock instead of a
    /// mount-wide lock; callers are expected to hold only a shared mount
    /// guard.
    /// Initialized mapping ranges are cached under that same state lock;
    /// in-file overwrites reuse them while no delayed reservations remain.
    /// Extent mutations and size shrink invalidate the cached ranges.
    ///
    /// Returns: the logical block ranges newly reserved by this call; empty when
    /// the range was already reserved or proven initialized. A buffered write
    /// threads this list into its completion so cancellation cannot release
    /// reservations owned by earlier writes.
    ///
    /// # Errors
    ///
    /// Returns an aborted-journal, extent-validation, overflow, accounting,
    /// or no-space error without publishing a partial mount reservation.
    pub(crate) fn reserve_delalloc_range_into(
        &self,
        inode: &Ext4Inode,
        start: LogicalBlock,
        block_count: u64,
    ) -> Ext4Result<Vec<(u64, u64)>> {
        self.ensure_writable()?;
        inode.reserve_delalloc_range_into(self, start, block_count)
    }

    /// Releases delayed-allocation reservations overlapping a logical range.
    ///
    /// # Errors
    ///
    /// Returns an overflow or accounting-invariant error.
    pub(crate) fn release_delalloc_range(
        &self,
        inode: &Ext4Inode,
        start: LogicalBlock,
        block_count: u64,
    ) -> Ext4Result<()> {
        self.ensure_delalloc_accounting(inode)?;
        let end = start
            .get()
            .checked_add(block_count)
            .ok_or(Ext4Error::Overflow)?;
        let released = inode.remove_delalloc_extent(start.get(), end)?;
        let mut delalloc_reserved_blocks = sync::mutex_lock(&self.delalloc_reserved_blocks);
        *delalloc_reserved_blocks = delalloc_reserved_blocks
            .checked_sub(released)
            .ok_or(Ext4Error::InvalidDelayedAllocationState)?;
        drop(delalloc_reserved_blocks);
        inode.with_quota_records(|records| self.release_quota_reservation(records, released))
    }

    /// Removes reservations already converted into physical allocation.
    pub(crate) fn consume_delalloc_range(
        &self,
        inode: &Ext4Inode,
        start: LogicalBlock,
        count: u64,
    ) -> Ext4Result<()> {
        let end = start.get().checked_add(count).ok_or(Ext4Error::Overflow)?;
        let released = inode.remove_delalloc_extent_with(start.get(), end, |state, released| {
            if released != 0 {
                let sectors = i128::from(released) * i128::from(self.layout().block_size() / 512);
                let blocks = state.allocated_blocks_after(sectors, inode.flags())?;
                self.claim_quota_reservation(inode, &state.dquots, released)?;
                state.metadata.blocks = blocks;
            }
            Ok(())
        })?;
        let mut reserved = sync::mutex_lock(&self.delalloc_reserved_blocks);
        *reserved = reserved
            .checked_sub(released)
            .ok_or(Ext4Error::InvalidDelayedAllocationState)?;
        Ok(())
    }

    /// Releases delayed-allocation reservations at or beyond `first_block`.
    ///
    /// # Errors
    ///
    /// Returns an accounting-invariant error.
    pub(crate) fn truncate_delalloc_range(
        &self,
        inode: &Ext4Inode,
        first_block: LogicalBlock,
    ) -> Ext4Result<()> {
        self.ensure_delalloc_accounting(inode)?;
        let released = inode.remove_delalloc_from(first_block.get())?;
        let mut delalloc_reserved_blocks = sync::mutex_lock(&self.delalloc_reserved_blocks);
        *delalloc_reserved_blocks = delalloc_reserved_blocks
            .checked_sub(released)
            .ok_or(Ext4Error::InvalidDelayedAllocationState)?;
        drop(delalloc_reserved_blocks);
        inode.with_quota_records(|records| self.release_quota_reservation(records, released))
    }

    /// Releases every delayed-allocation reservation owned by an inode.
    ///
    /// # Errors
    ///
    /// Returns an accounting-invariant error.
    pub(crate) fn release_all_delalloc(&self, inode: &Ext4Inode) -> Ext4Result<()> {
        self.ensure_delalloc_accounting(inode)?;
        let released = inode.clear_delalloc_reservations();
        let mut delalloc_reserved_blocks = sync::mutex_lock(&self.delalloc_reserved_blocks);
        *delalloc_reserved_blocks = delalloc_reserved_blocks
            .checked_sub(released)
            .ok_or(Ext4Error::InvalidDelayedAllocationState)?;
        drop(delalloc_reserved_blocks);
        inode.with_quota_records(|records| self.release_quota_reservation(records, released))
    }

    pub(crate) fn publish_inode_metadata(
        &self,
        inode: &Ext4Inode,
        metadata: Ext4InodeMetadata,
    ) -> Ext4Result<()> {
        inode.publish_metadata(metadata)
    }

    /// Acknowledges only timestamp values included in the prepared inode entry.
    pub(crate) fn publish_inode_metadata_with_timestamps(
        &self,
        inode: &Ext4Inode,
        metadata: Ext4InodeMetadata,
        timestamps: PendingInodeTimestamps,
    ) -> Ext4Result<()> {
        inode.publish_metadata_with_timestamps(metadata, timestamps, None)
    }

    pub(crate) fn is_public_inode_number(&self, number: InodeNumber) -> bool {
        !self.is_special_inode_number(number)
            && (number.get() == disk_inode::EXT4_ROOT_INO
                || number.get() >= self.superblock().first_inode())
    }

    pub(crate) fn is_special_inode_number(&self, number: InodeNumber) -> bool {
        let inode = number.get();
        self.is_quota_inode(number)
            || (inode != disk_inode::EXT4_ROOT_INO && inode < self.superblock().first_inode())
            || self
                .superblock()
                .orphan_file_inode()
                .is_some_and(|orphan_file| orphan_file.get() == inode)
    }

    pub(crate) fn raw_inode(&self, number: InodeNumber) -> Ext4Result<disk_inode::RawInode> {
        let location = self.inode_location(number)?;
        let block = self.read_metadata_block(location.block)?;
        let end = location
            .byte_offset
            .checked_add(location.inode_size)
            .ok_or(Ext4Error::Overflow)?;
        let inode_bytes = block
            .as_ref()
            .get(location.byte_offset..end)
            .ok_or(Ext4Error::OutOfBounds)?;
        let raw = disk_inode::RawInode::decode(inode_bytes)?;
        validate_extra_isize(inode_bytes.len(), &raw)?;
        self.verify_inode_checksum(number, inode_bytes, &raw)?;
        Ok(raw)
    }

    pub(crate) fn inode_table_entry_block(
        &self,
        number: InodeNumber,
    ) -> Ext4Result<FilesystemBlock> {
        Ok(self.inode_location(number)?.block)
    }

    pub(crate) fn inode_table_entry_block_in_group(
        &self,
        number: InodeNumber,
        group_index: usize,
        inode_table_start: u64,
    ) -> Ext4Result<FilesystemBlock> {
        Ok(self
            .inode_location_in_group(number, group_index, inode_table_start)?
            .block)
    }

    pub(crate) fn initialize_inode_table_entry_in_group(
        &self,
        block_bytes: &mut [u8],
        number: InodeNumber,
        group_index: usize,
        inode_table_start: u64,
        initialization: InodeInitialization,
    ) -> Ext4Result<()> {
        let location = self.inode_location_in_group(number, group_index, inode_table_start)?;
        self.initialize_inode_table_entry_at(block_bytes, number, location, initialization)
    }

    fn initialize_inode_table_entry_at(
        &self,
        block_bytes: &mut [u8],
        number: InodeNumber,
        location: InodeLocation,
        initialization: InodeInitialization,
    ) -> Ext4Result<()> {
        let inode_bytes = inode_entry_mut(block_bytes, location)?;
        let extra_isize = self.new_inode_extra_isize(location.inode_size)?;
        encode_initialized_inode(
            inode_bytes,
            number,
            initialization,
            extra_isize,
            self.superblock().features().has_extents(),
            self.superblock().features().has_metadata_checksum(),
            self.superblock().checksum_seed(),
        )
    }

    pub(crate) fn update_inode_table_entry_allow_zero_links(
        &self,
        block_bytes: &mut [u8],
        number: InodeNumber,
        update: impl FnOnce(&mut [u8]) -> Ext4Result<()>,
    ) -> Ext4Result<DecodedInodeFields> {
        self.update_inode_table_entry_inner(block_bytes, number, true, update)
    }

    pub(crate) fn update_referenced_inode_table_entry(
        &self,
        block_bytes: &mut [u8],
        inode: &Ext4Inode,
        update: impl FnOnce(&mut [u8]) -> Ext4Result<()>,
    ) -> Ext4Result<DecodedInodeFields> {
        self.update_inode_table_entry_inner(
            block_bytes,
            inode.number(),
            inode.links_count() == 0,
            |bytes| {
                update_inode_blocks_bytes(bytes, inode.blocks())?;
                update(bytes)
            },
        )
    }

    fn update_inode_table_entry_inner(
        &self,
        block_bytes: &mut [u8],
        number: InodeNumber,
        allow_zero_links: bool,
        update: impl FnOnce(&mut [u8]) -> Ext4Result<()>,
    ) -> Ext4Result<DecodedInodeFields> {
        let location = self.inode_location(number)?;
        let inode_bytes = inode_entry_mut(block_bytes, location)?;
        let raw = disk_inode::RawInode::decode(inode_bytes)?;
        validate_extra_isize(inode_bytes.len(), &raw)?;
        self.verify_inode_checksum(number, inode_bytes, &raw)?;

        update(inode_bytes)?;
        update_inode_checksum(
            inode_bytes,
            number,
            self.superblock().features().has_metadata_checksum(),
            self.superblock().checksum_seed(),
        )?;
        let raw = disk_inode::RawInode::decode(inode_bytes)?;
        Ext4InodeMetadata::from_raw(
            raw,
            allow_zero_links,
            self.superblock().features().has_project(),
        )
    }

    /// Publishes only the prepared inode entry, preserving neighboring inodes
    /// in the latest version of the shared inode-table block.
    ///
    /// Callers serialize changes to the target inode and validate/encode its
    /// entry before requesting write access. The slot mutex serializes this
    /// merge with other inode publications and journal snapshots.
    pub(crate) fn replace_inode_table_entry(
        &self,
        access: &MetadataWriteAccess,
        number: InodeNumber,
        prepared: &[u8],
    ) -> Ext4Result<()> {
        let location = self.inode_location(number)?;
        let end = location
            .byte_offset
            .checked_add(location.inode_size)
            .ok_or(Ext4Error::Overflow)?;
        let entry = prepared
            .get(location.byte_offset..end)
            .ok_or(Ext4Error::OutOfBounds)?;
        access.update_bytes(|_, current| {
            inode_entry_mut(current, location)?.copy_from_slice(entry);
            Ok(())
        })
    }

    /// Persists one ordinary inode metadata change after best-effort expansion
    /// of the inode's extra fields.
    ///
    /// Xattr mutation and zero-link eviction use lower-level table-entry
    /// helpers so they do not recurse into migration. Owner changes expand
    /// first, then transfer the resulting complete allocation to the new owner.
    ///
    /// The `update` closure must not change `i_flags`: its sole resident owner is
    /// the [`Ext4Inode::flags`] atomic, which only `set_flags` publishes, and this
    /// path does not synchronize it. Flag changes must go through
    /// [`update_inode_flags_timestamps_metadata`](Self::update_inode_flags_timestamps_metadata).
    pub(crate) fn update_dirty_inode_metadata(
        &self,
        inode: &Ext4Inode,
        handle: &mut JournalHandle<'_>,
        update: impl FnOnce(&Self, &mut [u8]) -> Ext4Result<()>,
    ) -> Ext4Result<()> {
        self.try_expand_inode_extra_isize(inode, handle)?;

        let timestamps = inode.pending_timestamps();
        let inode_table_block = self.inode_table_entry_block(inode.number())?;
        let mut inode_table_bytes = self
            .read_metadata_block(inode_table_block)?
            .as_ref()
            .to_vec();
        let updated_inode = self.update_referenced_inode_table_entry(
            &mut inode_table_bytes,
            inode,
            |inode_bytes| {
                timestamps.encode(inode_bytes)?;
                update(self, inode_bytes)
            },
        )?;
        let inode_table_access = self.metadata_io.write_access(inode_table_block, handle)?;
        self.replace_inode_table_entry(&inode_table_access, inode.number(), &inode_table_bytes)?;
        inode.publish_metadata_with_timestamps(
            updated_inode.metadata,
            timestamps,
            Some(handle.transaction()?),
        )
    }

    /// Stages automatic timestamps once per inode writeback, not per write.
    /// Failed staging leaves the resident values dirty for retry.
    pub(crate) fn write_inode_timestamps(&mut self, inode: &Ext4Inode) -> Ext4Result<()> {
        if !inode.has_dirty_timestamps() {
            return Ok(());
        }
        self.initialize_inode_quotas(inode)?;
        let credits = JournalCredits::new(2);
        let credits = self.quota_metadata_credits(credits);
        let journal = self.metadata_journal_for_allocation(
            credits,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
            self.inode_extra_isize_allocation_blocks(inode),
        )?;
        let mut handle = journal.begin(credits)?;
        let result = self.update_dirty_inode_metadata(inode, &mut handle, |_, _| Ok(()));
        self.complete_metadata_mutation(handle, result)
    }

    pub(crate) fn update_regular_inode_write_metadata(
        &self,
        inode: &Ext4Inode,
        new_disk_size: u64,
        metadata: RegularWriteMetadata,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        self.ensure_regular_file_mutation_supported(inode)?;
        if new_disk_size < inode.disk_size() {
            return Err(Ext4Error::Unsupported(UnsupportedKind::FileSizeShrink));
        }

        self.update_regular_inode_size_metadata(inode, new_disk_size, metadata, handle)
    }

    pub(crate) fn validate_inode_timestamp_update(
        &self,
        inode: &Ext4Inode,
        timestamp: Ext4Timestamp,
    ) -> Ext4Result<()> {
        let inode_table_block = self.inode_table_entry_block(inode.number())?;
        let mut inode_table_bytes = self
            .read_metadata_block(inode_table_block)?
            .as_ref()
            .to_vec();
        self.update_referenced_inode_table_entry(&mut inode_table_bytes, inode, |inode_bytes| {
            update_inode_ctime_mtime_bytes(inode_bytes, timestamp)
        })?;
        Ok(())
    }

    pub(crate) fn update_regular_inode_size_metadata(
        &self,
        inode: &Ext4Inode,
        new_disk_size: u64,
        metadata: RegularWriteMetadata,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        self.ensure_regular_file_mutation_supported(inode)?;

        self.update_dirty_inode_metadata(inode, handle, |_filesystem, inode_bytes| {
            update_inode_size_bytes(inode_bytes, new_disk_size)?;
            match metadata {
                RegularWriteMetadata::SizeOnly => Ok(()),
                RegularWriteMetadata::Full { timestamp } => {
                    let raw = disk_inode::RawInode::decode(inode_bytes)?;
                    update_inode_timestamp_bytes(
                        inode_bytes,
                        0x0c,
                        disk_inode::CTIME_EXTRA_OFFSET,
                        raw.ctime_extra().is_some(),
                        timestamp,
                    )?;
                    update_inode_timestamp_bytes(
                        inode_bytes,
                        0x10,
                        disk_inode::MTIME_EXTRA_OFFSET,
                        raw.mtime_extra().is_some(),
                        timestamp,
                    )
                }
            }
        })
    }

    pub(crate) fn update_inode_size_metadata(
        &self,
        inode: &Ext4Inode,
        size: u64,
        timestamp: Ext4Timestamp,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        self.update_dirty_inode_metadata(inode, handle, |_filesystem, inode_bytes| {
            update_inode_size_bytes(inode_bytes, size)?;
            update_inode_ctime_mtime_bytes(inode_bytes, timestamp)
        })?;
        inode.set_size(size);
        Ok(())
    }

    pub(crate) fn update_inode_timestamps_metadata(
        &self,
        inode: &Ext4Inode,
        timestamp: Ext4Timestamp,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        self.update_dirty_inode_metadata(inode, handle, |_filesystem, inode_bytes| {
            update_inode_ctime_mtime_bytes(inode_bytes, timestamp)
        })
    }

    pub(crate) fn update_inode_ctime_metadata(
        &self,
        inode: &Ext4Inode,
        timestamp: Ext4Timestamp,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        self.update_dirty_inode_metadata(inode, handle, |_filesystem, inode_bytes| {
            update_inode_ctime_bytes(inode_bytes, timestamp)
        })
    }

    /// Upper bound for optional external-xattr allocation while dirtying an inode.
    pub(crate) fn inode_extra_isize_allocation_blocks(&self, inode: &Ext4Inode) -> u64 {
        let extra_isize = self
            .superblock()
            .want_extra_isize()
            .max(self.superblock().min_extra_isize());
        u64::from(!inode.is_extra_isize_expansion_disabled() && inode.extra_isize() < extra_isize)
    }

    /// Applies a journaled inode metadata update.
    pub(crate) fn update_inode_metadata(
        &mut self,
        inode: &Ext4Inode,
        update: Ext4InodeMetadataUpdate,
    ) -> Ext4Result<()> {
        if update.is_empty() {
            return Ok(());
        }

        self.initialize_inode_quotas(inode)?;
        if let Some((uid, gid)) = update.owner {
            self.initialize_owner_quotas(uid, gid, inode.project_id())?;
        }
        let credits = JournalCredits::new(2);
        let credits = self.quota_metadata_credits(credits);
        let journal = self.metadata_journal_for_allocation(
            credits,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
            self.inode_extra_isize_allocation_blocks(inode),
        )?;
        let mut handle = journal.begin(credits)?;
        let result = self.update_inode_metadata_in_transaction(inode, update, &mut handle);
        self.complete_metadata_mutation(handle, result)
    }

    fn update_inode_metadata_in_transaction(
        &self,
        inode: &Ext4Inode,
        update: Ext4InodeMetadataUpdate,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        self.try_expand_inode_extra_isize(inode, handle)?;
        let timestamps = inode.pending_timestamps();
        let block = self.inode_table_entry_block(inode.number())?;
        let mut bytes = self.read_metadata_block(block)?.as_ref().to_vec();
        let permission = update.mode;
        let decoded =
            self.update_referenced_inode_table_entry(&mut bytes, inode, |inode_bytes| {
                timestamps.encode(inode_bytes)?;
                let raw = disk_inode::RawInode::decode(inode_bytes)?;
                if let Some(mode) = permission {
                    put_u16(inode_bytes, 0x00, (raw.mode() & disk_inode::S_IFMT) | mode)?;
                }
                if let Some((uid, gid)) = update.owner {
                    put_u16(inode_bytes, 0x02, uid as u16)?;
                    put_u16(inode_bytes, 0x18, gid as u16)?;
                    put_u16(inode_bytes, 0x78, (uid >> 16) as u16)?;
                    put_u16(inode_bytes, 0x7a, (gid >> 16) as u16)?;
                }
                if let Some(atime) = update.atime {
                    update_inode_timestamp_bytes(
                        inode_bytes,
                        0x08,
                        disk_inode::ATIME_EXTRA_OFFSET,
                        raw.atime_extra().is_some(),
                        atime,
                    )?;
                }
                if let Some(mtime) = update.mtime {
                    update_inode_timestamp_bytes(
                        inode_bytes,
                        0x10,
                        disk_inode::MTIME_EXTRA_OFFSET,
                        raw.mtime_extra().is_some(),
                        mtime,
                    )?;
                }
                update_inode_timestamp_bytes(
                    inode_bytes,
                    0x0c,
                    disk_inode::CTIME_EXTRA_OFFSET,
                    raw.ctime_extra().is_some(),
                    update.ctime,
                )
            })?;
        let records = if let Some((uid, gid)) = update.owner {
            self.transfer_inode_quotas(inode, uid, gid, inode.project_id(), handle)?
        } else {
            None
        };
        let access = self.metadata_io.write_access(block, handle)?;
        self.replace_inode_table_entry(&access, inode.number(), &bytes)?;
        inode.publish_metadata_and_quotas(
            decoded.metadata,
            timestamps,
            Some(handle.transaction()?),
            records,
        )?;
        if let Some(permission) = permission {
            inode.set_permission(permission);
        }
        Ok(())
    }

    pub(crate) fn set_inode_project_id(
        &mut self,
        inode: &Ext4Inode,
        project_id: u32,
        timestamp: Ext4Timestamp,
    ) -> Ext4Result<()> {
        if !self.superblock().features().has_project() {
            return if project_id == 0 {
                Ok(())
            } else {
                Err(Ext4Error::Unsupported(UnsupportedKind::ProjectId))
            };
        }
        if project_id == inode.project_id() {
            return Ok(());
        }
        if project_id == u32::MAX {
            return Err(Ext4Error::Quota(kerrno::LinuxError::EINVAL));
        }
        if self.is_quota_inode(inode.number()) || inode.is_noquota() {
            return Err(Ext4Error::Quota(kerrno::LinuxError::EPERM));
        }

        self.initialize_inode_quotas(inode)?;
        if !initialized_inode_extra_field_fits(inode.extra_isize(), disk_inode::PROJID_OFFSET) {
            let credits = self.quota_metadata_credits(JournalCredits::new(2));
            let journal = self.metadata_journal_for_allocation(
                credits,
                crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
                self.inode_extra_isize_allocation_blocks(inode),
            )?;
            let mut handle = journal.begin(credits)?;
            let result = self.try_expand_inode_extra_isize(inode, &mut handle);
            self.complete_metadata_mutation(handle, result)?;
            if !initialized_inode_extra_field_fits(inode.extra_isize(), disk_inode::PROJID_OFFSET) {
                return Err(Ext4Error::Unsupported(UnsupportedKind::ProjectId));
            }
        }

        let stat = inode.stat();
        self.initialize_owner_quotas(stat.uid, stat.gid, project_id)?;
        let credits = self.quota_metadata_credits(JournalCredits::new(2));
        let journal = self.metadata_journal_for_allocation(
            credits,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
            0,
        )?;
        let mut handle = journal.begin(credits)?;
        let result = (|| {
            let timestamps = inode.pending_timestamps();
            let block = self.inode_table_entry_block(inode.number())?;
            let mut bytes = self.read_metadata_block(block)?.as_ref().to_vec();
            let decoded = self.update_referenced_inode_table_entry(&mut bytes, inode, |bytes| {
                timestamps.encode(bytes)?;
                put_u32(bytes, disk_inode::PROJID_OFFSET, project_id)?;
                update_inode_ctime_bytes(bytes, timestamp)
            })?;
            let records =
                self.transfer_inode_quotas(inode, stat.uid, stat.gid, project_id, &mut handle)?;
            let access = self.metadata_io.write_access(block, &mut handle)?;
            self.replace_inode_table_entry(&access, inode.number(), &bytes)?;
            inode.publish_metadata_and_quotas(
                decoded.metadata,
                timestamps,
                Some(handle.transaction()?),
                records,
            )
        })();
        self.complete_metadata_mutation(handle, result)
    }

    pub(crate) fn set_inode_user_flags(
        &mut self,
        inode: &Ext4Inode,
        flags: u32,
        timestamp: Ext4Timestamp,
    ) -> Ext4Result<()> {
        if self.is_quota_inode(inode.number()) || inode.is_noquota() {
            return Err(Ext4Error::Quota(kerrno::LinuxError::EPERM));
        }
        inode.ensure_flags_publishable(flags)?;
        self.initialize_inode_quotas(inode)?;
        let credits = self.quota_metadata_credits(JournalCredits::new(2));
        let journal = self.metadata_journal_for_allocation(
            credits,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
            self.inode_extra_isize_allocation_blocks(inode),
        )?;
        let mut handle = journal.begin(credits)?;
        let result = self
            .update_dirty_inode_metadata(inode, &mut handle, |_, bytes| {
                put_u32(bytes, 0x20, flags)?;
                update_inode_ctime_bytes(bytes, timestamp)
            })
            .and_then(|()| inode.set_flags(flags));
        self.complete_metadata_mutation(handle, result)
    }

    pub(crate) fn update_inode_flags_timestamps_metadata(
        &self,
        inode: &Ext4Inode,
        flags: u32,
        timestamp: Ext4Timestamp,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        // Reject a mapping-format change before touching the on-disk entry, so a
        // rejected update cannot leave the journal carrying flags the resident
        // atomic refuses to adopt.
        inode.ensure_flags_publishable(flags)?;
        self.update_dirty_inode_metadata(inode, handle, |_filesystem, inode_bytes| {
            put_u32(inode_bytes, 0x20, flags)?;
            update_inode_ctime_mtime_bytes(inode_bytes, timestamp)
        })?;
        inode.set_flags(flags)
    }

    pub(crate) fn update_inode_links_count_metadata(
        &self,
        inode: &Ext4Inode,
        links_count: u16,
        timestamp: Ext4Timestamp,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        self.update_dirty_inode_metadata(inode, handle, |_filesystem, inode_bytes| {
            put_u16(inode_bytes, 0x1a, links_count)?;
            update_inode_ctime_mtime_bytes(inode_bytes, timestamp)
        })
    }

    pub(crate) fn update_inode_links_count_ctime_metadata(
        &self,
        inode: &Ext4Inode,
        links_count: u16,
        timestamp: Ext4Timestamp,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        self.update_dirty_inode_metadata(inode, handle, |_filesystem, inode_bytes| {
            put_u16(inode_bytes, 0x1a, links_count)?;
            update_inode_ctime_bytes(inode_bytes, timestamp)
        })
    }

    pub(crate) fn update_unlinked_inode_metadata(
        &self,
        inode: &Ext4Inode,
        size: Option<u64>,
        timestamp: Ext4Timestamp,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let timestamps = inode.pending_timestamps();
        let inode_table_block = self.inode_table_entry_block(inode.number())?;
        let mut inode_table_bytes = self
            .read_metadata_block(inode_table_block)?
            .as_ref()
            .to_vec();
        let updated_inode = self.update_inode_table_entry_allow_zero_links(
            &mut inode_table_bytes,
            inode.number(),
            |inode_bytes| {
                update_inode_blocks_bytes(inode_bytes, inode.blocks())?;
                timestamps.encode(inode_bytes)?;
                put_u16(inode_bytes, 0x1a, 0)?;
                if let Some(size) = size {
                    update_inode_size_bytes(inode_bytes, size)?;
                }
                update_inode_ctime_bytes(inode_bytes, timestamp)
            },
        )?;
        let inode_table_access = self.metadata_io.write_access(inode_table_block, handle)?;
        self.replace_inode_table_entry(&inode_table_access, inode.number(), &inode_table_bytes)?;
        inode.publish_metadata_with_timestamps(
            updated_inode.metadata,
            timestamps,
            Some(handle.transaction()?),
        )
    }

    pub(crate) fn update_inode_blocks_metadata(
        &self,
        inode: &Ext4Inode,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        self.update_dirty_inode_metadata(inode, handle, |_filesystem, _inode_bytes| Ok(()))
    }

    /// Stages this resident inode's orphan link without acknowledging dirty times.
    pub(crate) fn update_inode_orphan_next(
        &self,
        inode: &Ext4Inode,
        next: Option<InodeNumber>,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let transaction = handle.transaction()?;
        self.update_orphan_chain_link(inode.number(), next, handle)?;
        inode.update_state(|state| state.sync_transaction = Some(transaction));
        Ok(())
    }

    /// Updates recovery-chain bookkeeping in the caller's transaction.
    /// A predecessor splice belongs to the removal transaction. It changes no
    /// file attributes or mappings and need not advance that predecessor's fsync
    /// target. If the transaction is not committed,
    /// recovery sees the old link and the still-allocated removed inode together.
    /// Updates to the operation's resident inode use `update_inode_orphan_next`.
    pub(crate) fn update_orphan_chain_link(
        &self,
        inode: InodeNumber,
        next: Option<InodeNumber>,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let next = next.map_or(0, |inode| inode.get());
        let inode_table_block = self.inode_table_entry_block(inode)?;
        let mut inode_table_bytes = self
            .read_metadata_block(inode_table_block)?
            .as_ref()
            .to_vec();
        let _ = self.update_inode_table_entry_allow_zero_links(
            &mut inode_table_bytes,
            inode,
            |inode_bytes| put_u32(inode_bytes, disk_inode::DTIME_OFFSET, next),
        )?;
        let inode_table_access = self.metadata_io.write_access(inode_table_block, handle)?;
        self.replace_inode_table_entry(&inode_table_access, inode, &inode_table_bytes)?;
        Ok(())
    }

    pub(crate) fn clear_inode_table_entry_in_group(
        &self,
        block_bytes: &mut [u8],
        number: InodeNumber,
        group_index: usize,
        inode_table_start: u64,
    ) -> Ext4Result<()> {
        let location = self.inode_location_in_group(number, group_index, inode_table_start)?;
        inode_entry_mut(block_bytes, location)?.fill(0);
        Ok(())
    }

    fn verify_inode_checksum(
        &self,
        number: InodeNumber,
        bytes: &[u8],
        raw: &disk_inode::RawInode,
    ) -> Ext4Result<()> {
        if !self.superblock().features().has_metadata_checksum() {
            return Ok(());
        }

        let has_checksum_hi = fits_in_extra_inode(raw, disk_inode::CHECKSUM_HI_OFFSET, 2)?;
        let checksum = inode_checksum(
            self.superblock().checksum_seed(),
            number,
            bytes,
            raw,
            has_checksum_hi,
        )?;
        let provided = if has_checksum_hi {
            u32::from(raw.checksum_lo()) | (u32::from(raw.checksum_hi()) << 16)
        } else {
            u32::from(raw.checksum_lo())
        };
        if checksum != provided {
            return Err(Ext4Error::ChecksumMismatch {
                target: ChecksumTarget::Inode(number.get()),
                expected: checksum,
                actual: provided,
            });
        }
        Ok(())
    }

    fn new_inode_extra_isize(&self, inode_size: usize) -> Ext4Result<u16> {
        let available = inode_size
            .checked_sub(disk_inode::GOOD_OLD_INODE_SIZE)
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidInodeSize))?;
        let effective_want_extra_isize = usize::from(self.superblock().want_extra_isize());
        u16::try_from(effective_want_extra_isize.min(available)).map_err(|_| Ext4Error::Overflow)
    }
}

#[derive(Clone, Copy)]
struct InodeLocation {
    block: FilesystemBlock,
    byte_offset: usize,
    inode_size: usize,
}

impl Ext4SbInfo {
    fn inode_location(&self, number: InodeNumber) -> Ext4Result<InodeLocation> {
        let inode_number = number.get();
        if inode_number == 0 || inode_number > self.superblock().inodes_count() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeNumber));
        }

        let zero_based = inode_number.checked_sub(1).ok_or(Ext4Error::Overflow)?;
        let group = zero_based / self.superblock().inodes_per_group();
        let geometry = self
            .group_geometry(crate::BlockGroupNumber::new(group))
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidInodeNumber))?;
        self.inode_location_in_group(
            number,
            usize::try_from(group).map_err(|_| Ext4Error::Overflow)?,
            geometry.inode_table(),
        )
    }

    /// Location for an inode whose block-group inode-table start is known.
    ///
    /// The inode-table start block is a frozen geometry address read from the
    /// lock-free [`Self::group_geometry`] table; allocator critical sections
    /// pass it from the same table because calling [`Self::inode_location`]
    /// there would re-enter the allocator lock.
    fn inode_location_in_group(
        &self,
        number: InodeNumber,
        group_index: usize,
        inode_table_start: u64,
    ) -> Ext4Result<InodeLocation> {
        let inode_number = number.get();
        if inode_number == 0 || inode_number > self.superblock().inodes_count() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeNumber));
        }

        let zero_based = inode_number.checked_sub(1).ok_or(Ext4Error::Overflow)?;
        let group = zero_based / self.superblock().inodes_per_group();
        if u64::from(group) != u64::try_from(group_index).map_err(|_| Ext4Error::Overflow)? {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeNumber));
        }
        let index = zero_based % self.superblock().inodes_per_group();

        let inode_size = u64::from(self.superblock().inode_size());
        let block_size = u64::from(self.layout().block_size());
        let table_offset = u64::from(index)
            .checked_mul(inode_size)
            .ok_or(Ext4Error::Overflow)?;
        let block_offset = table_offset / block_size;
        let byte_offset =
            usize::try_from(table_offset % block_size).map_err(|_| Ext4Error::Overflow)?;
        let inode_size = usize::try_from(inode_size).map_err(|_| Ext4Error::Overflow)?;
        let inode_table_block = inode_table_start
            .checked_add(block_offset)
            .ok_or(Ext4Error::Overflow)?;

        Ok(InodeLocation {
            block: FilesystemBlock::new(inode_table_block),
            byte_offset,
            inode_size,
        })
    }
}

fn inode_entry_mut(block_bytes: &mut [u8], location: InodeLocation) -> Ext4Result<&mut [u8]> {
    let end = location
        .byte_offset
        .checked_add(location.inode_size)
        .ok_or(Ext4Error::Overflow)?;
    block_bytes
        .get_mut(location.byte_offset..end)
        .ok_or(Ext4Error::OutOfBounds)
}

fn encode_initialized_inode(
    output: &mut [u8],
    number: InodeNumber,
    initialization: InodeInitialization,
    extra_isize: u16,
    has_extents_feature: bool,
    has_metadata_checksum: bool,
    checksum_seed: u32,
) -> Ext4Result<()> {
    if output.len() < disk_inode::GOOD_OLD_INODE_SIZE
        || disk_inode::GOOD_OLD_INODE_SIZE
            .checked_add(usize::from(extra_isize))
            .ok_or(Ext4Error::Overflow)?
            > output.len()
        || !extra_isize.is_multiple_of(4)
    {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidInode));
    }

    output.fill(0);
    put_u16(output, 0x00, initialization.mode())?;
    put_u16(output, 0x02, initialization.uid as u16)?;
    update_inode_size_bytes(output, initialization.size)?;
    update_inode_timestamp_bytes(
        output,
        0x08,
        disk_inode::ATIME_EXTRA_OFFSET,
        initialized_inode_extra_field_fits(extra_isize, disk_inode::ATIME_EXTRA_OFFSET),
        initialization.timestamp,
    )?;
    update_inode_timestamp_bytes(
        output,
        0x0c,
        disk_inode::CTIME_EXTRA_OFFSET,
        initialized_inode_extra_field_fits(extra_isize, disk_inode::CTIME_EXTRA_OFFSET),
        initialization.timestamp,
    )?;
    update_inode_timestamp_bytes(
        output,
        0x10,
        disk_inode::MTIME_EXTRA_OFFSET,
        initialized_inode_extra_field_fits(extra_isize, disk_inode::MTIME_EXTRA_OFFSET),
        initialization.timestamp,
    )?;
    put_u16(output, 0x18, initialization.gid as u16)?;
    put_u16(output, 0x1a, initialization.links_count)?;
    let flags = initialized_inode_flags(&initialization, has_extents_feature)?;
    put_u32(output, 0x20, flags)?;
    if flags & disk_inode::EXT4_EXTENTS_FL != 0 {
        let i_block = output
            .get_mut(
                disk_inode::I_BLOCK_OFFSET
                    ..disk_inode::I_BLOCK_OFFSET + disk_inode::INODE_BLOCK_BYTES,
            )
            .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
        disk_extent::encode_empty_root(i_block)?;
    } else {
        let i_block = output
            .get_mut(
                disk_inode::I_BLOCK_OFFSET
                    ..disk_inode::I_BLOCK_OFFSET + disk_inode::INODE_BLOCK_BYTES,
            )
            .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
        i_block.copy_from_slice(&initialization.block);
    }
    put_u32(output, 0x64, initialization.generation)?;
    put_u16(output, 0x78, (initialization.uid >> 16) as u16)?;
    put_u16(output, 0x7a, (initialization.gid >> 16) as u16)?;
    if output.len() >= disk_inode::EXTRA_ISIZE_OFFSET + 2 {
        put_u16(output, disk_inode::EXTRA_ISIZE_OFFSET, extra_isize)?;
    }
    if initialized_inode_extra_field_fits(extra_isize, disk_inode::PROJID_OFFSET) {
        put_u32(output, disk_inode::PROJID_OFFSET, initialization.project_id)?;
    } else if initialization.project_id != 0 {
        return Err(Ext4Error::Unsupported(UnsupportedKind::ProjectId));
    }
    update_inode_checksum(output, number, has_metadata_checksum, checksum_seed)
}

fn initialized_inode_extra_field_fits(extra_isize: u16, offset: usize) -> bool {
    disk_inode::GOOD_OLD_INODE_SIZE + usize::from(extra_isize) >= offset + 4
}

fn initialized_inode_flags(
    initialization: &InodeInitialization,
    has_extents_feature: bool,
) -> Ext4Result<u32> {
    let uses_extent_tree = initialization.uses_extent_tree
        || has_extents_feature
            && matches!(
                initialization.kind(),
                InodeKind::Directory | InodeKind::RegularFile
            );
    if uses_extent_tree && !has_extents_feature {
        return Err(Ext4Error::Unsupported(UnsupportedKind::NonExtentInode));
    }
    Ok(initialization.inherited_flags
        | if uses_extent_tree {
            disk_inode::EXT4_EXTENTS_FL
        } else {
            0
        })
}

fn update_inode_checksum(
    output: &mut [u8],
    number: InodeNumber,
    has_metadata_checksum: bool,
    filesystem_checksum_seed: u32,
) -> Ext4Result<()> {
    if !has_metadata_checksum {
        return Ok(());
    }

    put_u16(output, disk_inode::CHECKSUM_LO_OFFSET, 0)?;
    if output.len() >= disk_inode::CHECKSUM_HI_OFFSET + 2 {
        put_u16(output, disk_inode::CHECKSUM_HI_OFFSET, 0)?;
    }
    let raw = disk_inode::RawInode::decode(output)?;
    validate_extra_isize(output.len(), &raw)?;
    let has_checksum_hi = fits_in_extra_inode(&raw, disk_inode::CHECKSUM_HI_OFFSET, 2)?;
    let checksum = inode_checksum(
        filesystem_checksum_seed,
        number,
        output,
        &raw,
        has_checksum_hi,
    )?;
    put_u16(output, disk_inode::CHECKSUM_LO_OFFSET, checksum as u16)?;
    if has_checksum_hi {
        put_u16(
            output,
            disk_inode::CHECKSUM_HI_OFFSET,
            (checksum >> 16) as u16,
        )?;
    }
    Ok(())
}

fn put_u16(output: &mut [u8], offset: usize, value: u16) -> Ext4Result<()> {
    let end = offset.checked_add(2).ok_or(Ext4Error::Overflow)?;
    output
        .get_mut(offset..end)
        .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn put_u32(output: &mut [u8], offset: usize, value: u32) -> Ext4Result<()> {
    let end = offset.checked_add(4).ok_or(Ext4Error::Overflow)?;
    output
        .get_mut(offset..end)
        .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn update_inode_size_bytes(output: &mut [u8], size: u64) -> Ext4Result<()> {
    put_u32(output, 0x04, size as u32)?;
    put_u32(output, 0x6c, (size >> 32) as u32)
}

pub(crate) fn update_inode_blocks_bytes(output: &mut [u8], blocks: u64) -> Ext4Result<()> {
    if blocks >> 48 != 0 {
        return Err(Ext4Error::Overflow);
    }
    put_u32(output, disk_inode::BLOCKS_LO_OFFSET, blocks as u32)?;
    put_u16(output, disk_inode::BLOCKS_HI_OFFSET, (blocks >> 32) as u16)
}

fn update_inode_timestamp_bytes(
    output: &mut [u8],
    base_offset: usize,
    extra_offset: usize,
    has_extra: bool,
    timestamp: Ext4Timestamp,
) -> Ext4Result<()> {
    let encoded = timestamp.encode(has_extra)?;
    put_u32(output, base_offset, encoded.base_seconds)?;
    if let Some(extra) = encoded.extra {
        put_u32(output, extra_offset, extra)?;
    }
    Ok(())
}

fn update_inode_ctime_mtime_bytes(output: &mut [u8], timestamp: Ext4Timestamp) -> Ext4Result<()> {
    update_inode_ctime_bytes(output, timestamp)?;
    let raw = disk_inode::RawInode::decode(output)?;
    update_inode_timestamp_bytes(
        output,
        0x10,
        disk_inode::MTIME_EXTRA_OFFSET,
        raw.mtime_extra().is_some(),
        timestamp,
    )
}

pub(crate) fn update_inode_ctime_bytes(
    output: &mut [u8],
    timestamp: Ext4Timestamp,
) -> Ext4Result<()> {
    let raw = disk_inode::RawInode::decode(output)?;
    update_inode_timestamp_bytes(
        output,
        0x0c,
        disk_inode::CTIME_EXTRA_OFFSET,
        raw.ctime_extra().is_some(),
        timestamp,
    )
}

pub(crate) fn inode_checksum_seed(
    filesystem_checksum_seed: u32,
    number: InodeNumber,
    generation: u32,
) -> u32 {
    let checksum = checksum::crc32c(filesystem_checksum_seed, &number.get().to_le_bytes());
    checksum::crc32c(checksum, &generation.to_le_bytes())
}

fn validate_extra_isize(inode_len: usize, raw: &disk_inode::RawInode) -> Ext4Result<()> {
    if inode_len <= disk_inode::GOOD_OLD_INODE_SIZE {
        return Ok(());
    }
    let extra_isize = usize::from(raw.extra_isize());
    if disk_inode::GOOD_OLD_INODE_SIZE
        .checked_add(extra_isize)
        .ok_or(Ext4Error::Overflow)?
        > inode_len
        || extra_isize % 4 != 0
    {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidInode));
    }
    Ok(())
}

fn fits_in_extra_inode(
    raw: &disk_inode::RawInode,
    field_offset: usize,
    field_size: usize,
) -> Ext4Result<bool> {
    let used = field_offset
        .checked_add(field_size)
        .ok_or(Ext4Error::Overflow)?;
    let available = disk_inode::GOOD_OLD_INODE_SIZE
        .checked_add(usize::from(raw.extra_isize()))
        .ok_or(Ext4Error::Overflow)?;
    Ok(used <= available)
}

fn inode_checksum(
    filesystem_checksum_seed: u32,
    number: InodeNumber,
    bytes: &[u8],
    raw: &disk_inode::RawInode,
    has_checksum_hi: bool,
) -> Ext4Result<u32> {
    let seed = inode_checksum_seed(filesystem_checksum_seed, number, raw.generation());
    let inode_len = bytes.len();

    let first = bytes
        .get(..disk_inode::CHECKSUM_LO_OFFSET)
        .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
    let after_lo_start = disk_inode::CHECKSUM_LO_OFFSET
        .checked_add(2)
        .ok_or(Ext4Error::Overflow)?;
    let after_lo_end = disk_inode::GOOD_OLD_INODE_SIZE.min(inode_len);
    let after_lo = bytes
        .get(after_lo_start..after_lo_end)
        .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;

    let mut checksum = checksum::crc32c(seed, first);
    checksum = checksum::crc32c(checksum, &[0, 0]);
    checksum = checksum::crc32c(checksum, after_lo);

    if inode_len > disk_inode::GOOD_OLD_INODE_SIZE {
        let hi_offset = if has_checksum_hi {
            disk_inode::CHECKSUM_HI_OFFSET
        } else {
            inode_len
        };
        let before_hi = bytes
            .get(disk_inode::GOOD_OLD_INODE_SIZE..hi_offset)
            .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
        checksum = checksum::crc32c(checksum, before_hi);
        if has_checksum_hi {
            checksum = checksum::crc32c(checksum, &[0, 0]);
            let after_hi_start = disk_inode::CHECKSUM_HI_OFFSET
                .checked_add(2)
                .ok_or(Ext4Error::Overflow)?;
            let after_hi = bytes
                .get(after_hi_start..)
                .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
            checksum = checksum::crc32c(checksum, after_hi);
        }
    }

    if has_checksum_hi {
        Ok(checksum)
    } else {
        Ok(checksum & 0xffff)
    }
}

unittest::path_mod_test!(tests, "../ut-tests/inode/tests.rs");

unittest::path_mod_test!(/// QEMU unittest variants of the inode-state lock tests.
///
/// These run under `make UNITTEST=y UNITTEST_CRATE=kext4 run` (kernel build,
/// `target_os = "none"`), so they use the `unittest` assertion macros and,
/// for the concurrency cases, the kernel scheduler (`ktask`) and
/// synchronization primitives (`ksync`) instead of `std::thread`.
 mod unittests, "../ut-tests/inode/unittests.rs");

#[cfg(unittest)]
unittest::path_mod_test!(component_tests, "../ut-tests/inode/component_tests.rs");

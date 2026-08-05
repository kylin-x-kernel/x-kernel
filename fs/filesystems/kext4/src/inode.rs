// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{collections::BTreeMap, vec::Vec};
use core::fmt;

use crate::{
    BlockMapping, ChecksumTarget, CorruptKind, Ext4Error, Ext4Filesystem, Ext4Result,
    FilesystemBlock, InodeNumber, LogicalBlock, UnsupportedKind,
    disk::{checksum, extent as disk_extent, inode as disk_inode},
    file::RegularWriteMetadata,
    jbd2::{JournalCredits, JournalHandle},
    superblock::replace_metadata_access_bytes,
    sync::{self, Mutex},
};

const EXT4_ENCODED_DEV_MAJOR_MAX: u32 = 0x0fff;
const EXT4_ENCODED_DEV_MINOR_MAX: u32 = 0x000f_ffff;

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
    size: u64,
    block: [u8; disk_inode::INODE_BLOCK_BYTES],
    uses_extent_tree: bool,
    links_count: u16,
    timestamp_seconds: u32,
    generation: u32,
}

#[allow(dead_code)]
impl InodeInitialization {
    pub(crate) const fn regular_file(permissions: u16, uid: u32, gid: u32) -> Self {
        Self {
            kind: InodeKind::RegularFile,
            permissions,
            uid,
            gid,
            size: 0,
            block: [0; disk_inode::INODE_BLOCK_BYTES],
            uses_extent_tree: false,
            links_count: 1,
            timestamp_seconds: 0,
            generation: 0,
        }
    }

    pub(crate) const fn directory(permissions: u16, uid: u32, gid: u32) -> Self {
        Self {
            kind: InodeKind::Directory,
            permissions,
            uid,
            gid,
            size: 0,
            block: [0; disk_inode::INODE_BLOCK_BYTES],
            uses_extent_tree: false,
            links_count: 2,
            timestamp_seconds: 0,
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
            size: u64::try_from(target.len()).map_err(|_| Ext4Error::Overflow)?,
            block,
            uses_extent_tree: false,
            links_count: 1,
            timestamp_seconds: 0,
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
            size: u64::try_from(target_len).map_err(|_| Ext4Error::Overflow)?,
            block: [0; disk_inode::INODE_BLOCK_BYTES],
            uses_extent_tree: true,
            links_count: 1,
            timestamp_seconds: 0,
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
            size: 0,
            block,
            uses_extent_tree: false,
            links_count: 1,
            timestamp_seconds: 0,
            generation: 0,
        })
    }

    pub(crate) const fn with_owner(mut self, uid: u32, gid: u32) -> Self {
        self.uid = uid;
        self.gid = gid;
        self
    }

    pub(crate) const fn with_timestamp_seconds(mut self, timestamp_seconds: u32) -> Self {
        self.timestamp_seconds = timestamp_seconds;
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
    pub const fn new(seconds: i64, nanos: u32) -> Self {
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
    kind: InodeKind,
    mode: u16,
    uid: u32,
    gid: u32,
    disk_size: u64,
    blocks: u64,
    flags: u32,
    block: [u8; disk_inode::INODE_BLOCK_BYTES],
    file_acl: u64,
    inline_xattr: Vec<u8>,
    generation: u32,
    links_count: u16,
    atime: Ext4Timestamp,
    ctime: Ext4Timestamp,
    mtime: Ext4Timestamp,
}

impl Ext4InodeMetadata {
    fn from_raw(raw: disk_inode::RawInode, allow_zero_links: bool) -> Ext4Result<Self> {
        if raw.links_count() == 0 && !allow_zero_links {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInode));
        }
        let kind = InodeKind::from_mode(raw.mode())?;
        if raw.flags() & disk_inode::EXT4_INLINE_DATA_FL != 0 {
            return Err(Ext4Error::Unsupported(UnsupportedKind::InlineData));
        }
        if raw.flags() & disk_inode::EXT4_ENCRYPT_FL != 0 {
            return Err(Ext4Error::Unsupported(UnsupportedKind::EncryptedName));
        }
        if raw.flags() & disk_inode::EXT4_CASEFOLD_FL != 0 {
            return Err(Ext4Error::Unsupported(UnsupportedKind::CasefoldName));
        }

        Ok(Self {
            kind,
            mode: raw.mode(),
            uid: raw.uid(),
            gid: raw.gid(),
            disk_size: raw.size(),
            blocks: raw.blocks(),
            flags: raw.flags(),
            block: *raw.block(),
            file_acl: raw.file_acl(),
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
        })
    }
}

struct Ext4InodeState {
    metadata: Ext4InodeMetadata,
    visible_size: u64,
    delayed_extents: BTreeMap<u64, u64>,
    reserved_data_blocks: u64,
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
/// access this same object. It is composed directly into the bridge inode and
/// has neither an independent resident identity nor an independent reference
/// count.
pub struct Ext4Inode {
    number: InodeNumber,
    state: Mutex<Ext4InodeState>,
}

impl Ext4Inode {
    fn new(number: InodeNumber, metadata: Ext4InodeMetadata) -> Self {
        let visible_size = metadata.disk_size;
        Self {
            number,
            state: Mutex::new(Ext4InodeState {
                metadata,
                visible_size,
                delayed_extents: BTreeMap::new(),
                reserved_data_blocks: 0,
            }),
        }
    }

    fn metadata_snapshot(&self) -> Ext4InodeMetadata {
        sync::lock(&self.state).metadata.clone()
    }

    fn with_metadata<T>(&self, read: impl FnOnce(&Ext4InodeMetadata) -> T) -> T {
        read(&sync::lock(&self.state).metadata)
    }

    fn publish_metadata(&self, metadata: Ext4InodeMetadata) -> Ext4Result<()> {
        sync::lock(&self.state).metadata = metadata;
        Ok(())
    }

    /// Returns this inode's number.
    pub fn number(&self) -> InodeNumber {
        self.number
    }

    /// Returns this inode's kind.
    pub fn kind(&self) -> InodeKind {
        self.with_metadata(|metadata| metadata.kind)
    }

    /// Returns the raw Linux mode bits.
    pub fn mode(&self) -> u16 {
        self.with_metadata(|metadata| metadata.mode)
    }

    /// Returns the inode owner's user ID.
    pub fn uid(&self) -> u32 {
        self.with_metadata(|metadata| metadata.uid)
    }

    /// Returns the inode owner's group ID.
    pub fn gid(&self) -> u32 {
        self.with_metadata(|metadata| metadata.gid)
    }

    /// Returns the ext4 on-disk inode size in bytes.
    ///
    /// This is the kext4 equivalent of Linux ext4's `i_disksize`. A VFS inode
    /// may expose a newer visible `i_size` while buffered data is still waiting
    /// for ordered writeback.
    pub fn disk_size(&self) -> u64 {
        self.with_metadata(|metadata| metadata.disk_size)
    }

    /// Returns the VFS-visible inode size in bytes.
    pub fn size(&self) -> u64 {
        sync::lock(&self.state).visible_size
    }

    /// Updates the VFS-visible inode size without changing `i_disksize`.
    pub fn set_size(&self, size: u64) {
        sync::lock(&self.state).visible_size = size;
    }

    /// Captures the generic inode attributes under one state lock.
    pub fn stat(&self) -> Ext4InodeStat {
        let state = sync::lock(&self.state);
        let metadata = &state.metadata;
        Ext4InodeStat {
            mode: metadata.mode,
            uid: metadata.uid,
            gid: metadata.gid,
            size: state.visible_size,
            blocks: metadata.blocks,
            rdev: device_id_from_metadata(metadata),
            links_count: metadata.links_count,
            atime: metadata.atime,
            ctime: metadata.ctime,
            mtime: metadata.mtime,
        }
    }

    /// Updates permission bits in the single resident inode state.
    pub fn set_permission(&self, permission: u16) {
        let mut state = sync::lock(&self.state);
        state.metadata.mode =
            (state.metadata.mode & disk_inode::S_IFMT) | (permission & !disk_inode::S_IFMT);
    }

    /// Updates the owner in the single resident inode state.
    pub fn set_owner(&self, uid: u32, gid: u32) {
        let mut state = sync::lock(&self.state);
        state.metadata.uid = uid;
        state.metadata.gid = gid;
    }

    /// Updates the link count in the single resident inode state.
    pub fn set_links_count(&self, link_count: u64) {
        let link_count = u16::try_from(link_count).unwrap_or(u16::MAX);
        sync::lock(&self.state).metadata.links_count = link_count;
    }

    /// Increments the resident link count.
    pub fn increment_links_count(&self) {
        let mut state = sync::lock(&self.state);
        state.metadata.links_count = state.metadata.links_count.saturating_add(1);
    }

    /// Decrements the resident link count.
    pub fn decrement_links_count(&self) {
        let mut state = sync::lock(&self.state);
        debug_assert_ne!(state.metadata.links_count, 0);
        state.metadata.links_count = state.metadata.links_count.saturating_sub(1);
    }

    /// Updates the access time in the single resident inode state.
    pub fn set_atime(&self, atime: Ext4Timestamp) {
        sync::lock(&self.state).metadata.atime = atime;
    }

    /// Updates the modification time in the single resident inode state.
    pub fn set_mtime(&self, mtime: Ext4Timestamp) {
        sync::lock(&self.state).metadata.mtime = mtime;
    }

    /// Updates the status-change time in the single resident inode state.
    pub fn set_ctime(&self, ctime: Ext4Timestamp) {
        sync::lock(&self.state).metadata.ctime = ctime;
    }

    /// Updates generic allocated-block accounting in the resident inode.
    pub fn set_allocated_bytes(&self, bytes: u64) {
        debug_assert_eq!(bytes % 512, 0);
        sync::lock(&self.state).metadata.blocks = bytes / 512;
    }

    /// Adds allocated bytes to the resident block count.
    pub fn add_allocated_bytes(&self, bytes: u64) {
        debug_assert_eq!(bytes % 512, 0);
        let mut state = sync::lock(&self.state);
        state.metadata.blocks = state.metadata.blocks.saturating_add(bytes / 512);
    }

    /// Subtracts allocated bytes from the resident block count.
    pub fn subtract_allocated_bytes(&self, bytes: u64) {
        debug_assert_eq!(bytes % 512, 0);
        let mut state = sync::lock(&self.state);
        state.metadata.blocks = state.metadata.blocks.saturating_sub(bytes / 512);
    }

    /// Returns the raw ext4 block accounting value.
    pub fn blocks(&self) -> u64 {
        self.with_metadata(|metadata| metadata.blocks)
    }

    /// Returns the raw ext4 inode flags.
    pub fn flags(&self) -> u32 {
        self.with_metadata(|metadata| metadata.flags)
    }

    /// Returns whether the inode carries the ext4 immutable flag.
    pub fn is_immutable(&self) -> bool {
        self.flags() & disk_inode::EXT4_IMMUTABLE_FL != 0
    }

    /// Returns whether the inode carries the ext4 append-only flag.
    pub fn is_append_only(&self) -> bool {
        self.flags() & disk_inode::EXT4_APPEND_FL != 0
    }

    pub(crate) fn block_mapping_root(&self) -> (u32, [u8; disk_inode::INODE_BLOCK_BYTES]) {
        self.with_metadata(|metadata| (metadata.flags, metadata.block))
    }

    pub(crate) fn file_acl_block(&self) -> u64 {
        self.with_metadata(|metadata| metadata.file_acl)
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

    pub(crate) fn fast_symlink_target(
        &self,
        filesystem_block_size: u32,
        has_ea_inode_feature: bool,
    ) -> Ext4Result<Option<Vec<u8>>> {
        let metadata = self.metadata_snapshot();
        if metadata.kind != InodeKind::Symlink {
            return Err(Ext4Error::Unsupported(UnsupportedKind::InodeKind));
        }
        if metadata.flags & disk_inode::EXT4_INLINE_DATA_FL != 0 {
            return Err(Ext4Error::Unsupported(UnsupportedKind::InlineData));
        }
        if metadata.flags & disk_inode::EXT4_ENCRYPT_FL != 0 {
            return Err(Ext4Error::Unsupported(UnsupportedKind::EncryptedName));
        }

        let ea_blocks = if metadata.file_acl == 0 {
            0
        } else {
            u64::from(filesystem_block_size) / 512
        };
        let data_blocks = metadata
            .blocks
            .checked_sub(ea_blocks)
            .ok_or(Ext4Error::Overflow)?;
        let is_fast = if has_ea_inode_feature {
            metadata.disk_size != 0 && metadata.disk_size < disk_inode::INODE_BLOCK_BYTES as u64
        } else {
            data_blocks == 0
        };
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
        self.with_metadata(device_id_from_metadata)
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

    fn unreserved_delalloc_extents(&self, start: u64, end: u64) -> Vec<(u64, u64)> {
        let state = sync::lock(&self.state);
        let mut extents = Vec::new();
        let mut cursor = start;
        for (&extent_start, &extent_end) in state.delayed_extents.range(..end) {
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

    fn insert_unreserved_delalloc_extents(&self, extents: &[(u64, u64)]) -> Ext4Result<u64> {
        let mut state = sync::lock(&self.state);
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
        let new_reserved_data_blocks = state
            .reserved_data_blocks
            .checked_add(newly_reserved)
            .ok_or(Ext4Error::Overflow)?;

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
        Ok(newly_reserved)
    }

    fn remove_delalloc_extent(&self, start: u64, end: u64) -> Ext4Result<u64> {
        if start >= end {
            return Ok(0);
        }
        let mut state = sync::lock(&self.state);
        let overlapping = state
            .delayed_extents
            .range(..end)
            .filter(|(extent_start, extent_end)| **extent_end > start && **extent_start < end)
            .map(|(&extent_start, &extent_end)| (extent_start, extent_end))
            .collect::<Vec<_>>();
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
    }

    fn remove_delalloc_from(&self, first_block: u64) -> Ext4Result<u64> {
        self.remove_delalloc_extent(first_block, u64::MAX)
    }

    fn clear_delalloc_reservations(&self) -> u64 {
        let mut state = sync::lock(&self.state);
        let reserved = state.reserved_data_blocks;
        state.delayed_extents.clear();
        state.reserved_data_blocks = 0;
        reserved
    }

    fn reserved_data_blocks(&self) -> u64 {
        sync::lock(&self.state).reserved_data_blocks
    }

    pub(crate) fn has_delalloc_reservations(&self) -> bool {
        sync::lock(&self.state).reserved_data_blocks != 0
    }

    pub(crate) fn next_delalloc_extent(&self, start: u64) -> Option<(u64, u64)> {
        let state = sync::lock(&self.state);
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
    }
}

impl fmt::Debug for Ext4Inode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Ext4Inode")
            .field("number", &self.number())
            .field("metadata", &self.metadata_snapshot())
            .finish_non_exhaustive()
    }
}

fn device_id_from_metadata(metadata: &Ext4InodeMetadata) -> Option<Ext4DeviceId> {
    if metadata.kind != InodeKind::CharacterDevice && metadata.kind != InodeKind::BlockDevice {
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

impl Ext4Filesystem {
    /// Returns an inline fast-symlink target, or `None` for a block-mapped target.
    pub fn fast_symlink_target(&self, inode: &Ext4Inode) -> Ext4Result<Option<Vec<u8>>> {
        inode.fast_symlink_target(
            self.layout().block_size(),
            self.superblock().features().has_ea_inode(),
        )
    }

    /// Loads filesystem-private state for the root inode.
    pub fn root_inode(&self) -> Ext4Result<Ext4Inode> {
        self.load_inode_private(InodeNumber::new(disk_inode::EXT4_ROOT_INO))
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
    pub fn load_inode_private(&self, number: InodeNumber) -> Ext4Result<Ext4Inode> {
        let inode_number = number.get();
        if inode_number == 0 || inode_number > self.superblock().inodes_count() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeNumber));
        }
        if !self.is_public_inode_number(number) {
            return Err(Ext4Error::Unsupported(UnsupportedKind::ReservedInode));
        }
        self.iget_inner(number, false)
    }

    pub(crate) fn internal_iget(&self, number: InodeNumber) -> Ext4Result<Ext4Inode> {
        self.iget_inner(number, false)
    }

    pub(crate) fn orphan_iget(&self, number: InodeNumber) -> Ext4Result<Ext4Inode> {
        self.iget_inner(number, true)
    }

    fn iget_inner(&self, number: InodeNumber, allow_zero_links: bool) -> Ext4Result<Ext4Inode> {
        let metadata = Ext4InodeMetadata::from_raw(self.raw_inode(number)?, allow_zero_links)?;
        Ok(Ext4Inode::new(number, metadata))
    }

    fn ensure_delalloc_accounting(&self, inode: &Ext4Inode) -> Ext4Result<()> {
        if inode.reserved_data_blocks() <= self.delalloc_reserved_blocks {
            Ok(())
        } else {
            Err(Ext4Error::InvalidDelayedAllocationState)
        }
    }

    /// Reserves unallocated blocks in one logical range for delayed allocation.
    ///
    /// Per-inode extent state and the mount-wide aggregate are updated by this
    /// operation as one ext4-owned invariant. Already allocated or already
    /// reserved subranges do not consume another reservation.
    ///
    /// # Errors
    ///
    /// Returns an extent-validation, overflow, accounting, or no-space error
    /// without publishing a partial mount reservation.
    pub fn reserve_delalloc_range(
        &mut self,
        inode: &Ext4Inode,
        start: LogicalBlock,
        block_count: u64,
    ) -> Ext4Result<()> {
        self.ensure_delalloc_accounting(inode)?;
        if block_count == 0 {
            return Ok(());
        }
        let end = start
            .get()
            .checked_add(block_count)
            .ok_or(Ext4Error::Overflow)?;
        let mut holes = Vec::new();
        for (mut logical, extent_end) in inode.unreserved_delalloc_extents(start.get(), end) {
            while logical < extent_end {
                let mapping = self.map_blocks(inode, LogicalBlock::new(logical))?;
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

        let reserved = holes.iter().try_fold(0u64, |total, (start, end)| {
            total
                .checked_add(end.checked_sub(*start).ok_or(Ext4Error::Overflow)?)
                .ok_or(Ext4Error::Overflow)
        })?;
        if reserved > self.blocks_available_for_reservation() {
            return Err(Ext4Error::NoSpace);
        }
        let new_total = self
            .delalloc_reserved_blocks
            .checked_add(reserved)
            .ok_or(Ext4Error::Overflow)?;
        let inserted_blocks = inode.insert_unreserved_delalloc_extents(&holes)?;
        assert_eq!(
            inserted_blocks, reserved,
            "validated delayed-allocation holes must preserve their block count"
        );
        self.delalloc_reserved_blocks = new_total;
        Ok(())
    }

    /// Releases delayed-allocation reservations overlapping a logical range.
    ///
    /// # Errors
    ///
    /// Returns an overflow or accounting-invariant error.
    pub fn release_delalloc_range(
        &mut self,
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
        self.delalloc_reserved_blocks = self
            .delalloc_reserved_blocks
            .checked_sub(released)
            .ok_or(Ext4Error::InvalidDelayedAllocationState)?;
        Ok(())
    }

    /// Releases delayed-allocation reservations at or beyond `first_block`.
    ///
    /// # Errors
    ///
    /// Returns an accounting-invariant error.
    pub fn truncate_delalloc_range(
        &mut self,
        inode: &Ext4Inode,
        first_block: LogicalBlock,
    ) -> Ext4Result<()> {
        self.ensure_delalloc_accounting(inode)?;
        let released = inode.remove_delalloc_from(first_block.get())?;
        self.delalloc_reserved_blocks = self
            .delalloc_reserved_blocks
            .checked_sub(released)
            .ok_or(Ext4Error::InvalidDelayedAllocationState)?;
        Ok(())
    }

    /// Releases every delayed-allocation reservation owned by an inode.
    ///
    /// # Errors
    ///
    /// Returns an accounting-invariant error.
    pub fn release_all_delalloc(&mut self, inode: &Ext4Inode) -> Ext4Result<()> {
        self.ensure_delalloc_accounting(inode)?;
        let released = inode.clear_delalloc_reservations();
        self.delalloc_reserved_blocks = self
            .delalloc_reserved_blocks
            .checked_sub(released)
            .ok_or(Ext4Error::InvalidDelayedAllocationState)?;
        Ok(())
    }

    pub(crate) fn publish_inode_metadata(
        &self,
        inode: &Ext4Inode,
        metadata: Ext4InodeMetadata,
    ) -> Ext4Result<()> {
        inode.publish_metadata(metadata)
    }

    fn is_public_inode_number(&self, number: InodeNumber) -> bool {
        number.get() == disk_inode::EXT4_ROOT_INO || number.get() >= self.superblock().first_inode()
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

    pub(crate) fn initialize_inode_table_entry(
        &self,
        block_bytes: &mut [u8],
        number: InodeNumber,
        initialization: InodeInitialization,
    ) -> Ext4Result<()> {
        let location = self.inode_location(number)?;
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

    pub(crate) fn update_inode_table_entry(
        &self,
        block_bytes: &mut [u8],
        number: InodeNumber,
        update: impl FnOnce(&mut [u8]) -> Ext4Result<()>,
    ) -> Ext4Result<Ext4InodeMetadata> {
        self.update_inode_table_entry_inner(block_bytes, number, false, update)
    }

    pub(crate) fn update_inode_table_entry_allow_zero_links(
        &self,
        block_bytes: &mut [u8],
        number: InodeNumber,
        update: impl FnOnce(&mut [u8]) -> Ext4Result<()>,
    ) -> Ext4Result<Ext4InodeMetadata> {
        self.update_inode_table_entry_inner(block_bytes, number, true, update)
    }

    pub(crate) fn update_referenced_inode_table_entry(
        &self,
        block_bytes: &mut [u8],
        inode: &Ext4Inode,
        update: impl FnOnce(&mut [u8]) -> Ext4Result<()>,
    ) -> Ext4Result<Ext4InodeMetadata> {
        self.update_inode_table_entry_inner(
            block_bytes,
            inode.number(),
            inode.links_count() == 0,
            update,
        )
    }

    fn update_inode_table_entry_inner(
        &self,
        block_bytes: &mut [u8],
        number: InodeNumber,
        allow_zero_links: bool,
        update: impl FnOnce(&mut [u8]) -> Ext4Result<()>,
    ) -> Ext4Result<Ext4InodeMetadata> {
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
        Ext4InodeMetadata::from_raw(raw, allow_zero_links)
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

        let inode_table_block = self.inode_table_entry_block(inode.number())?;
        let mut inode_table_bytes = self
            .read_metadata_block(inode_table_block)?
            .as_ref()
            .to_vec();
        let updated_inode = self.update_referenced_inode_table_entry(
            &mut inode_table_bytes,
            inode,
            |inode_bytes| {
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
            },
        )?;
        let inode_table_access = self.metadata_io.write_access(inode_table_block, handle)?;
        replace_metadata_access_bytes(&inode_table_access, inode_table_bytes)?;
        self.publish_inode_metadata(inode, updated_inode)
    }

    pub(crate) fn update_inode_size_metadata(
        &self,
        inode: &Ext4Inode,
        size: u64,
        timestamp: Ext4Timestamp,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let inode_table_block = self.inode_table_entry_block(inode.number())?;
        let mut inode_table_bytes = self
            .read_metadata_block(inode_table_block)?
            .as_ref()
            .to_vec();
        let updated_inode = self.update_referenced_inode_table_entry(
            &mut inode_table_bytes,
            inode,
            |inode_bytes| {
                update_inode_size_bytes(inode_bytes, size)?;
                update_inode_ctime_mtime_bytes(inode_bytes, timestamp)
            },
        )?;
        let inode_table_access = self.metadata_io.write_access(inode_table_block, handle)?;
        replace_metadata_access_bytes(&inode_table_access, inode_table_bytes)?;
        self.publish_inode_metadata(inode, updated_inode)?;
        inode.set_size(size);
        Ok(())
    }

    pub(crate) fn update_inode_timestamps_metadata(
        &self,
        inode: &Ext4Inode,
        timestamp: Ext4Timestamp,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let inode_table_block = self.inode_table_entry_block(inode.number())?;
        let mut inode_table_bytes = self
            .read_metadata_block(inode_table_block)?
            .as_ref()
            .to_vec();
        let updated_inode = self.update_referenced_inode_table_entry(
            &mut inode_table_bytes,
            inode,
            |inode_bytes| update_inode_ctime_mtime_bytes(inode_bytes, timestamp),
        )?;
        let inode_table_access = self.metadata_io.write_access(inode_table_block, handle)?;
        replace_metadata_access_bytes(&inode_table_access, inode_table_bytes)?;
        self.publish_inode_metadata(inode, updated_inode)
    }

    pub(crate) fn update_inode_ctime_metadata(
        &self,
        inode: &Ext4Inode,
        timestamp: Ext4Timestamp,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let inode_table_block = self.inode_table_entry_block(inode.number())?;
        let mut inode_table_bytes = self
            .read_metadata_block(inode_table_block)?
            .as_ref()
            .to_vec();
        let updated_inode = self.update_referenced_inode_table_entry(
            &mut inode_table_bytes,
            inode,
            |inode_bytes| update_inode_ctime_bytes(inode_bytes, timestamp),
        )?;
        let inode_table_access = self.metadata_io.write_access(inode_table_block, handle)?;
        replace_metadata_access_bytes(&inode_table_access, inode_table_bytes)?;
        self.publish_inode_metadata(inode, updated_inode)
    }

    /// Applies a journaled inode metadata update.
    pub fn update_inode_metadata(
        &mut self,
        inode: &Ext4Inode,
        update: Ext4InodeMetadataUpdate,
    ) -> Ext4Result<()> {
        if update.is_empty() {
            return Ok(());
        }

        let credits = JournalCredits::new(2);
        let journal = self.metadata_journal_for_mutation(
            credits,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
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
        let inode_table_block = self.inode_table_entry_block(inode.number())?;
        let mut inode_table_bytes = self
            .read_metadata_block(inode_table_block)?
            .as_ref()
            .to_vec();
        let updated_inode = self.update_referenced_inode_table_entry(
            &mut inode_table_bytes,
            inode,
            |inode_bytes| {
                let raw = disk_inode::RawInode::decode(inode_bytes)?;
                if let Some(mode) = update.mode {
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
            },
        )?;
        let inode_table_access = self.metadata_io.write_access(inode_table_block, handle)?;
        replace_metadata_access_bytes(&inode_table_access, inode_table_bytes)?;
        self.publish_inode_metadata(inode, updated_inode)
    }

    pub(crate) fn update_inode_flags_timestamps_metadata(
        &self,
        inode: &Ext4Inode,
        flags: u32,
        timestamp: Ext4Timestamp,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let inode_table_block = self.inode_table_entry_block(inode.number())?;
        let mut inode_table_bytes = self
            .read_metadata_block(inode_table_block)?
            .as_ref()
            .to_vec();
        let updated_inode = self.update_referenced_inode_table_entry(
            &mut inode_table_bytes,
            inode,
            |inode_bytes| {
                put_u32(inode_bytes, 0x20, flags)?;
                update_inode_ctime_mtime_bytes(inode_bytes, timestamp)
            },
        )?;
        let inode_table_access = self.metadata_io.write_access(inode_table_block, handle)?;
        replace_metadata_access_bytes(&inode_table_access, inode_table_bytes)?;
        self.publish_inode_metadata(inode, updated_inode)
    }

    pub(crate) fn update_inode_links_count_metadata(
        &self,
        inode: &Ext4Inode,
        links_count: u16,
        timestamp: Ext4Timestamp,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let inode_table_block = self.inode_table_entry_block(inode.number())?;
        let mut inode_table_bytes = self
            .read_metadata_block(inode_table_block)?
            .as_ref()
            .to_vec();
        let updated_inode =
            self.update_inode_table_entry(&mut inode_table_bytes, inode.number(), |inode_bytes| {
                put_u16(inode_bytes, 0x1a, links_count)?;
                update_inode_ctime_mtime_bytes(inode_bytes, timestamp)
            })?;
        let inode_table_access = self.metadata_io.write_access(inode_table_block, handle)?;
        replace_metadata_access_bytes(&inode_table_access, inode_table_bytes)?;
        self.publish_inode_metadata(inode, updated_inode)
    }

    pub(crate) fn update_inode_links_count_ctime_metadata(
        &self,
        inode: &Ext4Inode,
        links_count: u16,
        timestamp: Ext4Timestamp,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let inode_table_block = self.inode_table_entry_block(inode.number())?;
        let mut inode_table_bytes = self
            .read_metadata_block(inode_table_block)?
            .as_ref()
            .to_vec();
        let updated_inode =
            self.update_inode_table_entry(&mut inode_table_bytes, inode.number(), |inode_bytes| {
                put_u16(inode_bytes, 0x1a, links_count)?;
                update_inode_ctime_bytes(inode_bytes, timestamp)
            })?;
        let inode_table_access = self.metadata_io.write_access(inode_table_block, handle)?;
        replace_metadata_access_bytes(&inode_table_access, inode_table_bytes)?;
        self.publish_inode_metadata(inode, updated_inode)
    }

    pub(crate) fn update_unlinked_inode_metadata(
        &self,
        inode: &Ext4Inode,
        size: Option<u64>,
        timestamp: Ext4Timestamp,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let inode_table_block = self.inode_table_entry_block(inode.number())?;
        let mut inode_table_bytes = self
            .read_metadata_block(inode_table_block)?
            .as_ref()
            .to_vec();
        let updated_inode = self.update_inode_table_entry_allow_zero_links(
            &mut inode_table_bytes,
            inode.number(),
            |inode_bytes| {
                put_u16(inode_bytes, 0x1a, 0)?;
                if let Some(size) = size {
                    update_inode_size_bytes(inode_bytes, size)?;
                }
                update_inode_ctime_bytes(inode_bytes, timestamp)
            },
        )?;
        let inode_table_access = self.metadata_io.write_access(inode_table_block, handle)?;
        replace_metadata_access_bytes(&inode_table_access, inode_table_bytes)?;
        self.publish_inode_metadata(inode, updated_inode)
    }

    pub(crate) fn update_inode_blocks_metadata(
        &self,
        inode: &Ext4Inode,
        blocks: u64,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let inode_table_block = self.inode_table_entry_block(inode.number())?;
        let mut inode_table_bytes = self
            .read_metadata_block(inode_table_block)?
            .as_ref()
            .to_vec();
        let updated_inode = self.update_referenced_inode_table_entry(
            &mut inode_table_bytes,
            inode,
            |inode_bytes| update_inode_blocks_bytes(inode_bytes, blocks),
        )?;
        let inode_table_access = self.metadata_io.write_access(inode_table_block, handle)?;
        replace_metadata_access_bytes(&inode_table_access, inode_table_bytes)?;
        self.publish_inode_metadata(inode, updated_inode)
    }

    pub(crate) fn update_inode_orphan_next(
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
        replace_metadata_access_bytes(&inode_table_access, inode_table_bytes)?;
        Ok(())
    }

    pub(crate) fn clear_inode_table_entry(
        &self,
        block_bytes: &mut [u8],
        number: InodeNumber,
    ) -> Ext4Result<()> {
        let location = self.inode_location(number)?;
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
        let requested = self
            .superblock()
            .want_extra_isize()
            .max(self.superblock().min_extra_isize());
        let requested = usize::from(requested);
        if requested != 0 {
            return u16::try_from(requested.min(available)).map_err(|_| Ext4Error::Overflow);
        }
        if self.superblock().features().has_metadata_checksum()
            && disk_inode::CHECKSUM_HI_OFFSET + 2 <= inode_size
        {
            let checksum_extra =
                disk_inode::CHECKSUM_HI_OFFSET + 2 - disk_inode::GOOD_OLD_INODE_SIZE;
            return u16::try_from(checksum_extra).map_err(|_| Ext4Error::Overflow);
        }
        Ok(0)
    }
}

#[derive(Clone, Copy)]
struct InodeLocation {
    block: FilesystemBlock,
    byte_offset: usize,
    inode_size: usize,
}

impl Ext4Filesystem {
    fn inode_location(&self, number: InodeNumber) -> Ext4Result<InodeLocation> {
        let inode_number = number.get();
        if inode_number == 0 || inode_number > self.superblock().inodes_count() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeNumber));
        }

        let zero_based = inode_number.checked_sub(1).ok_or(Ext4Error::Overflow)?;
        let group = zero_based / self.superblock().inodes_per_group();
        let index = zero_based % self.superblock().inodes_per_group();
        let descriptor = self
            .group(crate::BlockGroupNumber::new(group))
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidInodeNumber))?;

        let inode_size = u64::from(self.superblock().inode_size());
        let block_size = u64::from(self.layout().block_size());
        let table_offset = u64::from(index)
            .checked_mul(inode_size)
            .ok_or(Ext4Error::Overflow)?;
        let block_offset = table_offset / block_size;
        let byte_offset =
            usize::try_from(table_offset % block_size).map_err(|_| Ext4Error::Overflow)?;
        let inode_size = usize::try_from(inode_size).map_err(|_| Ext4Error::Overflow)?;
        let inode_table_block = descriptor
            .inode_table()
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
    put_u32(output, 0x08, initialization.timestamp_seconds)?;
    put_u32(output, 0x0c, initialization.timestamp_seconds)?;
    put_u32(output, 0x10, initialization.timestamp_seconds)?;
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
    update_inode_checksum(output, number, has_metadata_checksum, checksum_seed)
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
    Ok(if uses_extent_tree {
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

fn update_inode_blocks_bytes(output: &mut [u8], blocks: u64) -> Ext4Result<()> {
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

#[cfg(test)]
mod tests {
    use super::{
        EncodedTimestamp, Ext4DeviceId, Ext4Inode, Ext4InodeMetadata, Ext4Timestamp,
        InodeInitialization, InodeKind, RawTimestampFields, disk_inode,
    };
    use crate::{CorruptKind, Ext4Error, InodeNumber, UnsupportedKind};

    const BLOCK_SIZE: u32 = 4096;

    #[test]
    fn exposes_immutable_and_append_only_inode_flags() {
        let inode = symlink_inode(
            9,
            0,
            disk_inode::EXT4_IMMUTABLE_FL | disk_inode::EXT4_APPEND_FL,
            0,
            fast_symlink_block(b"hello.txt"),
        );

        assert!(inode.is_immutable());
        assert!(inode.is_append_only());
    }

    #[test]
    fn classifies_fast_symlink_by_data_blocks_not_size_only() {
        let block = fast_symlink_block(b"hello.txt");
        let inode = symlink_inode(9, 0, 0, 0, block);

        let storage = inode
            .fast_symlink_target(BLOCK_SIZE, false)
            .expect("classify symlink storage");

        assert_eq!(storage.as_deref(), Some(b"hello.txt".as_slice()));
    }

    #[test]
    fn classifies_short_block_mapped_symlink_as_block_mapped() {
        let inode = symlink_inode(9, 8, disk_inode::EXT4_EXTENTS_FL, 0, [0; 60]);

        let storage = inode
            .fast_symlink_target(BLOCK_SIZE, false)
            .expect("classify symlink storage");

        assert_eq!(storage, None);
    }

    #[test]
    fn excludes_xattr_blocks_from_fast_symlink_data_blocks() {
        let block = fast_symlink_block(b"hello.txt");
        let inode = symlink_inode(9, 8, 0, 42, block);

        let storage = inode
            .fast_symlink_target(BLOCK_SIZE, false)
            .expect("classify symlink storage");

        assert_eq!(storage.as_deref(), Some(b"hello.txt".as_slice()));
    }

    #[test]
    fn classifies_symlink_with_filesystem_ea_inode_feature_not_inode_flag() {
        let block = fast_symlink_block(b"hello.txt");
        let inode = symlink_inode(9, 8, 0, 0, block);

        let storage = inode
            .fast_symlink_target(BLOCK_SIZE, true)
            .expect("classify symlink storage");

        assert_eq!(storage.as_deref(), Some(b"hello.txt".as_slice()));
    }

    #[test]
    fn ignores_inode_ea_inode_flag_when_filesystem_feature_is_absent() {
        let inode = symlink_inode(9, 8, disk_inode::EXT4_EA_INODE_FL, 0, [0; 60]);

        let storage = inode
            .fast_symlink_target(BLOCK_SIZE, false)
            .expect("classify symlink storage");

        assert_eq!(storage, None);
    }

    #[test]
    fn rejects_zero_length_fast_symlink() {
        let inode = symlink_inode(0, 0, 0, 0, fast_symlink_block(b"hello.txt"));

        assert_eq!(
            inode.fast_symlink_target(BLOCK_SIZE, false),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidInode))
        );
    }

    #[test]
    fn rejects_fast_symlink_size_that_reaches_past_i_block() {
        let inode = symlink_inode(60, 0, 0, 0, [b'a'; disk_inode::INODE_BLOCK_BYTES]);

        assert_eq!(
            inode.fast_symlink_target(BLOCK_SIZE, false),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidInode))
        );
    }

    #[test]
    fn rejects_fast_symlink_without_nul_terminator_at_size() {
        let mut block = [b'a'; disk_inode::INODE_BLOCK_BYTES];
        block[..9].copy_from_slice(b"hello.txt");
        let inode = symlink_inode(9, 0, 0, 0, block);

        assert_eq!(
            inode.fast_symlink_target(BLOCK_SIZE, false),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidInode))
        );
    }

    #[test]
    fn rejects_fast_symlink_with_embedded_nul_before_size() {
        let mut block = fast_symlink_block(b"hello.txt");
        block[4] = 0;
        let inode = symlink_inode(9, 0, 0, 0, block);

        assert_eq!(
            inode.fast_symlink_target(BLOCK_SIZE, false),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidInode))
        );
    }

    #[test]
    fn decodes_old_device_id_from_i_block_zero() {
        let mut block = [0; disk_inode::INODE_BLOCK_BYTES];
        put_u32(&mut block, 0, (12 << 8) | 34);
        let inode = special_inode(InodeKind::CharacterDevice, disk_inode::S_IFCHR, block);

        let device = inode.device_id().expect("character device has rdev");

        assert_eq!(device.major(), 12);
        assert_eq!(device.minor(), 34);
    }

    #[test]
    fn decodes_new_device_id_from_i_block_one() {
        let mut block = [0; disk_inode::INODE_BLOCK_BYTES];
        put_u32(&mut block, 4, encode_new_device_id(0xabc, 0xdef0));
        let inode = special_inode(InodeKind::BlockDevice, disk_inode::S_IFBLK, block);

        let device = inode.device_id().expect("block device has rdev");

        assert_eq!(device.major(), 0xabc);
        assert_eq!(device.minor(), 0xdef0);
    }

    #[test]
    fn rejects_device_id_outside_ext4_new_encoding() {
        assert_eq!(
            Ext4DeviceId::new_checked(0x1000, 0),
            Err(Ext4Error::Unsupported(UnsupportedKind::DeviceId))
        );
        assert_eq!(
            Ext4DeviceId::new_checked(0, 0x0010_0000),
            Err(Ext4Error::Unsupported(UnsupportedKind::DeviceId))
        );
        assert!(Ext4DeviceId::new_checked(0x0fff, 0x000f_ffff).is_ok());
    }

    #[test]
    fn special_inode_initialization_rejects_unencodable_device_id() {
        assert_eq!(
            InodeInitialization::special(
                InodeKind::CharacterDevice,
                0o666,
                Some(Ext4DeviceId::new(0x1000, 0)),
                0,
                0,
            ),
            Err(Ext4Error::Unsupported(UnsupportedKind::DeviceId))
        );
    }
    #[test]
    fn decodes_signed_timestamp_without_extra_epoch() {
        let timestamp =
            Ext4Timestamp::try_from(timestamp_fields(0xffff_ffff, None)).expect("decode timestamp");

        assert_eq!(timestamp, Ext4Timestamp::new(-1, 0));
    }

    #[test]
    fn decodes_timestamp_with_extra_epoch_and_nanoseconds() {
        let timestamp =
            Ext4Timestamp::try_from(timestamp_fields(0x8000_0000, Some((123_456_789 << 2) | 1)))
                .expect("decode timestamp");

        assert_eq!(timestamp, Ext4Timestamp::new(2_147_483_648, 123_456_789));
    }

    #[test]
    fn rejects_timestamp_with_invalid_nanoseconds() {
        assert_eq!(
            Ext4Timestamp::try_from(timestamp_fields(0, Some(1_000_000_000 << 2))),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidInode))
        );
    }

    #[test]
    fn encodes_timestamp_without_extra_field() {
        let encoded = Ext4Timestamp::new(-1, 0)
            .encode(false)
            .expect("encode timestamp");

        assert_eq!(
            encoded,
            EncodedTimestamp {
                base_seconds: 0xffff_ffff,
                extra: None,
            }
        );
    }

    #[test]
    fn truncates_nanoseconds_without_extra_timestamp_field() {
        let encoded = Ext4Timestamp::new(1, 1)
            .encode(false)
            .expect("encode legacy timestamp");

        assert_eq!(
            encoded,
            EncodedTimestamp {
                base_seconds: 1,
                extra: None,
            }
        );
    }

    #[test]
    fn clamps_seconds_without_extra_timestamp_field() {
        let encoded = Ext4Timestamp::new(i64::MAX, 0)
            .encode(false)
            .expect("encode legacy timestamp");

        assert_eq!(
            encoded,
            EncodedTimestamp {
                base_seconds: i32::MAX as u32,
                extra: None,
            }
        );
    }

    #[test]
    fn encodes_timestamp_with_extra_epoch_and_nanoseconds() {
        let encoded = Ext4Timestamp::new(2_147_483_648, 123_456_789)
            .encode(true)
            .expect("encode timestamp");

        assert_eq!(
            encoded,
            EncodedTimestamp {
                base_seconds: 0x8000_0000,
                extra: Some((123_456_789 << 2) | 1),
            }
        );
    }

    fn symlink_inode(
        size: u64,
        blocks: u64,
        flags: u32,
        file_acl: u64,
        block: [u8; disk_inode::INODE_BLOCK_BYTES],
    ) -> Ext4Inode {
        test_inode(Ext4InodeMetadata {
            kind: InodeKind::Symlink,
            mode: disk_inode::S_IFLNK,
            uid: 0,
            gid: 0,
            disk_size: size,
            blocks,
            flags,
            block,
            file_acl,
            inline_xattr: alloc::vec::Vec::new(),
            generation: 0,
            links_count: 1,
            atime: timestamp(0, 0),
            ctime: timestamp(0, 0),
            mtime: timestamp(0, 0),
        })
    }

    fn special_inode(
        kind: InodeKind,
        mode: u16,
        block: [u8; disk_inode::INODE_BLOCK_BYTES],
    ) -> Ext4Inode {
        test_inode(Ext4InodeMetadata {
            kind,
            mode,
            uid: 0,
            gid: 0,
            disk_size: 0,
            blocks: 0,
            flags: 0,
            block,
            file_acl: 0,
            inline_xattr: alloc::vec::Vec::new(),
            generation: 0,
            links_count: 1,
            atime: timestamp(0, 0),
            ctime: timestamp(0, 0),
            mtime: timestamp(0, 0),
        })
    }

    fn test_inode(metadata: Ext4InodeMetadata) -> Ext4Inode {
        Ext4Inode::new(InodeNumber::new(12), metadata)
    }

    fn regular_inode_metadata() -> Ext4InodeMetadata {
        Ext4InodeMetadata {
            kind: InodeKind::RegularFile,
            mode: disk_inode::S_IFREG | 0o644,
            uid: 0,
            gid: 0,
            disk_size: 0,
            blocks: 0,
            flags: disk_inode::EXT4_EXTENTS_FL,
            block: [0; disk_inode::INODE_BLOCK_BYTES],
            file_acl: 0,
            inline_xattr: alloc::vec::Vec::new(),
            generation: 1,
            links_count: 1,
            atime: timestamp(0, 0),
            ctime: timestamp(0, 0),
            mtime: timestamp(0, 0),
        }
    }

    #[test]
    fn delayed_allocation_extents_merge_split_and_count_blocks() {
        let inode = test_inode(regular_inode_metadata());
        assert_eq!(
            inode.insert_unreserved_delalloc_extents(&[(3, 8)]).unwrap(),
            5
        );
        assert_eq!(
            inode
                .insert_unreserved_delalloc_extents(&[(10, 12)])
                .unwrap(),
            2
        );
        assert_eq!(
            inode
                .insert_unreserved_delalloc_extents(&[(8, 10)])
                .unwrap(),
            2
        );
        assert_eq!(inode.remove_delalloc_extent(5, 10).unwrap(), 5);
        assert_eq!(
            inode.unreserved_delalloc_extents(0, 13),
            alloc::vec![(0, 3), (5, 10), (12, 13)]
        );
        assert_eq!(inode.remove_delalloc_from(4).unwrap(), 3);
        assert_eq!(inode.clear_delalloc_reservations(), 1);
        assert!(!inode.has_delalloc_reservations());
    }

    const fn timestamp(seconds: i64, nanos: u32) -> Ext4Timestamp {
        Ext4Timestamp::new(seconds, nanos)
    }

    const fn timestamp_fields(base_seconds: u32, extra: Option<u32>) -> RawTimestampFields {
        RawTimestampFields::new(base_seconds, extra)
    }

    fn fast_symlink_block(target: &[u8]) -> [u8; disk_inode::INODE_BLOCK_BYTES] {
        let mut block = [0; disk_inode::INODE_BLOCK_BYTES];
        block[..target.len()].copy_from_slice(target);
        block
    }

    fn encode_new_device_id(major: u32, minor: u32) -> u32 {
        (minor & 0xff) | (major << 8) | ((minor & !0xff) << 12)
    }

    fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
}

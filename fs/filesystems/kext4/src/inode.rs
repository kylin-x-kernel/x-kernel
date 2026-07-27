// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;

use crate::{
    ChecksumTarget, CorruptKind, Ext4Error, Ext4Filesystem, Ext4Result, FilesystemBlock,
    InodeNumber, UnsupportedKind,
    disk::{checksum, codec, extent as disk_extent, inode as disk_inode},
    file::RegularWriteMetadata,
    jbd2::{JournalCredits, JournalHandle},
    superblock::replace_metadata_access_bytes,
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
        const NSEC_PER_SEC: u32 = 1_000_000_000;

        if self.nanos >= NSEC_PER_SEC {
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
        const NSEC_PER_SEC: u32 = 1_000_000_000;

        let mut seconds = i64::from(i32::from_le_bytes(raw.base_seconds.to_le_bytes()));
        let nanos = if let Some(extra) = raw.extra {
            let epoch = extra & EXT4_EPOCH_MASK;
            if epoch != 0 {
                seconds = seconds
                    .checked_add(i64::from(epoch) << 32)
                    .ok_or(Ext4Error::Overflow)?;
            }
            let nanos = extra >> EXT4_EPOCH_BITS;
            if nanos >= NSEC_PER_SEC {
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

/// Storage form used by a symbolic link target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SymlinkStorage<'a> {
    /// The target bytes are stored directly in the inode `i_block` area.
    Fast(&'a [u8]),
    /// The target bytes are stored in regular data blocks mapped by the inode.
    BlockMapped,
}

/// Decoded read-only ext4 inode metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Ext4Inode {
    number: InodeNumber,
    kind: InodeKind,
    mode: u16,
    uid: u32,
    gid: u32,
    size: u64,
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
    dtime: u32,
}

impl Ext4Inode {
    pub(crate) fn from_raw(number: InodeNumber, raw: disk_inode::RawInode) -> Ext4Result<Self> {
        Self::from_raw_inner(number, raw, false)
    }

    pub(crate) fn from_raw_allow_zero_links(
        number: InodeNumber,
        raw: disk_inode::RawInode,
    ) -> Ext4Result<Self> {
        Self::from_raw_inner(number, raw, true)
    }

    fn from_raw_inner(
        number: InodeNumber,
        raw: disk_inode::RawInode,
        allow_zero_links: bool,
    ) -> Ext4Result<Self> {
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
            number,
            kind,
            mode: raw.mode(),
            uid: raw.uid(),
            gid: raw.gid(),
            size: raw.size(),
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
            dtime: raw.dtime(),
        })
    }

    /// Returns this inode's number.
    pub const fn number(&self) -> InodeNumber {
        self.number
    }

    /// Returns this inode's kind.
    pub const fn kind(&self) -> InodeKind {
        self.kind
    }

    /// Returns the raw Linux mode bits.
    pub const fn mode(&self) -> u16 {
        self.mode
    }

    /// Returns the inode owner's user ID.
    pub const fn uid(&self) -> u32 {
        self.uid
    }

    /// Returns the inode owner's group ID.
    pub const fn gid(&self) -> u32 {
        self.gid
    }

    /// Returns the ext4 on-disk inode size in bytes.
    ///
    /// This is the kext4 equivalent of Linux ext4's `i_disksize`. A VFS inode
    /// may expose a newer visible `i_size` while buffered data is still waiting
    /// for ordered writeback.
    pub const fn disk_size(&self) -> u64 {
        self.size
    }

    /// Returns the ext4 on-disk inode size in bytes.
    pub const fn size(&self) -> u64 {
        self.disk_size()
    }

    /// Returns the raw ext4 block accounting value.
    pub const fn blocks(&self) -> u64 {
        self.blocks
    }

    /// Returns the raw ext4 inode flags.
    pub const fn flags(&self) -> u32 {
        self.flags
    }

    pub(crate) const fn extent_bytes(&self) -> &[u8; disk_inode::INODE_BLOCK_BYTES] {
        &self.block
    }

    pub(crate) const fn file_acl_block(&self) -> u64 {
        self.file_acl
    }

    pub(crate) fn inline_xattr_bytes(&self) -> &[u8] {
        &self.inline_xattr
    }

    pub(crate) const fn generation(&self) -> u32 {
        self.generation
    }

    /// Returns the link count from the inode table entry.
    pub const fn links_count(&self) -> u16 {
        self.links_count
    }

    /// Returns the last-access timestamp.
    pub const fn atime(&self) -> Ext4Timestamp {
        self.atime
    }

    /// Returns the last-status-change timestamp.
    pub const fn ctime(&self) -> Ext4Timestamp {
        self.ctime
    }

    /// Returns the last-modification timestamp.
    pub const fn mtime(&self) -> Ext4Timestamp {
        self.mtime
    }

    pub(crate) fn orphan_next(&self) -> Option<InodeNumber> {
        match self.dtime {
            0 => None,
            next => Some(InodeNumber::new(next)),
        }
    }

    /// Returns the raw inode `i_block` bytes.
    ///
    /// The interpretation depends on the inode kind and flags: extent root,
    /// block map, fast symlink target, or device number.
    pub fn raw_i_block(&self) -> &[u8] {
        &self.block
    }

    pub(crate) fn symlink_storage(
        &self,
        filesystem_block_size: u32,
        has_ea_inode_feature: bool,
    ) -> Ext4Result<SymlinkStorage<'_>> {
        if self.kind != InodeKind::Symlink {
            return Err(Ext4Error::Unsupported(UnsupportedKind::InodeKind));
        }
        if self.flags & disk_inode::EXT4_INLINE_DATA_FL != 0 {
            return Err(Ext4Error::Unsupported(UnsupportedKind::InlineData));
        }
        if self.flags & disk_inode::EXT4_ENCRYPT_FL != 0 {
            return Err(Ext4Error::Unsupported(UnsupportedKind::EncryptedName));
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
        let is_fast = if has_ea_inode_feature {
            self.size != 0 && self.size < disk_inode::INODE_BLOCK_BYTES as u64
        } else {
            data_blocks == 0
        };
        if is_fast {
            Ok(SymlinkStorage::Fast(self.validate_fast_symlink_target()?))
        } else {
            Ok(SymlinkStorage::BlockMapped)
        }
    }

    fn validate_fast_symlink_target(&self) -> Ext4Result<&[u8]> {
        let size = usize::try_from(self.size).map_err(|_| Ext4Error::Overflow)?;
        if size == 0 || size >= disk_inode::INODE_BLOCK_BYTES {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInode));
        }

        let target_with_terminator = self
            .raw_i_block()
            .get(..=size)
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidInode))?;
        if target_with_terminator.iter().position(|byte| *byte == 0) != Some(size) {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInode));
        }

        self.raw_i_block()
            .get(..size)
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidInode))
    }

    /// Returns the device number encoded in a character or block device inode.
    pub fn device_id(&self) -> Ext4Result<Option<Ext4DeviceId>> {
        if self.kind != InodeKind::CharacterDevice && self.kind != InodeKind::BlockDevice {
            return Ok(None);
        }

        let old_encoded = codec::le_u32(&self.block, 0)?;
        if old_encoded != 0 {
            return Ok(Some(decode_old_device_id(old_encoded)));
        }
        Ok(Some(decode_new_device_id(codec::le_u32(&self.block, 4)?)))
    }

    pub(crate) const fn has_extents(&self) -> bool {
        self.flags & disk_inode::EXT4_EXTENTS_FL != 0
    }

    pub(crate) const fn uses_huge_file_accounting(&self) -> bool {
        self.flags & disk_inode::EXT4_HUGE_FILE_FL != 0
    }

    pub(crate) const fn has_indexed_directory(&self) -> bool {
        self.flags & disk_inode::EXT4_INDEX_FL != 0
    }
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
    /// Classifies how a symbolic link target is stored.
    pub fn symlink_storage<'a>(&self, inode: &'a Ext4Inode) -> Ext4Result<SymlinkStorage<'a>> {
        inode.symlink_storage(
            self.layout().block_size(),
            self.superblock().features().has_ea_inode(),
        )
    }

    /// Reads and validates the root inode.
    pub fn root_inode(&self) -> Ext4Result<Ext4Inode> {
        self.inode(InodeNumber::new(disk_inode::EXT4_ROOT_INO))
    }

    /// Reads and validates one inode table entry.
    pub fn inode(&self, number: InodeNumber) -> Ext4Result<Ext4Inode> {
        let inode_number = number.get();
        if inode_number == 0 || inode_number > self.superblock().inodes_count() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeNumber));
        }
        if !self.is_public_inode_number(number) {
            return Err(Ext4Error::Unsupported(UnsupportedKind::ReservedInode));
        }
        self.internal_inode(number)
    }

    pub(crate) fn internal_inode(&self, number: InodeNumber) -> Ext4Result<Ext4Inode> {
        let raw = self.raw_inode(number)?;
        Ext4Inode::from_raw(number, raw)
    }

    pub(crate) fn orphan_inode(&self, number: InodeNumber) -> Ext4Result<Ext4Inode> {
        let raw = self.raw_inode(number)?;
        Ext4Inode::from_raw_allow_zero_links(number, raw)
    }

    /// Reads an inode already held alive by an upper-layer inode reference.
    ///
    /// Unlike namespace lookup, this accepts a zero-link inode because an open
    /// file may remain usable between unlink and final eviction.
    pub fn referenced_inode(&self, number: InodeNumber) -> Ext4Result<Ext4Inode> {
        let inode_number = number.get();
        if inode_number == 0 || inode_number > self.superblock().inodes_count() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeNumber));
        }
        if !self.is_public_inode_number(number) {
            return Err(Ext4Error::Unsupported(UnsupportedKind::ReservedInode));
        }
        self.orphan_inode(number)
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
    ) -> Ext4Result<Ext4Inode> {
        self.update_inode_table_entry_inner(block_bytes, number, false, update)
    }

    pub(crate) fn update_inode_table_entry_allow_zero_links(
        &self,
        block_bytes: &mut [u8],
        number: InodeNumber,
        update: impl FnOnce(&mut [u8]) -> Ext4Result<()>,
    ) -> Ext4Result<Ext4Inode> {
        self.update_inode_table_entry_inner(block_bytes, number, true, update)
    }

    pub(crate) fn update_referenced_inode_table_entry(
        &self,
        block_bytes: &mut [u8],
        inode: &Ext4Inode,
        update: impl FnOnce(&mut [u8]) -> Ext4Result<()>,
    ) -> Ext4Result<Ext4Inode> {
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
    ) -> Ext4Result<Ext4Inode> {
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
        if allow_zero_links {
            Ext4Inode::from_raw_allow_zero_links(number, raw)
        } else {
            Ext4Inode::from_raw(number, raw)
        }
    }

    pub(crate) fn update_regular_inode_write_metadata(
        &self,
        inode: &Ext4Inode,
        new_disk_size: u64,
        metadata: RegularWriteMetadata,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
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
    ) -> Ext4Result<Ext4Inode> {
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
        Ok(updated_inode)
    }

    pub(crate) fn update_inode_size_metadata(
        &self,
        inode: &Ext4Inode,
        size: u64,
        timestamp: Ext4Timestamp,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
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
        Ok(updated_inode)
    }

    pub(crate) fn update_inode_timestamps_metadata(
        &self,
        inode: &Ext4Inode,
        timestamp: Ext4Timestamp,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
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
        Ok(updated_inode)
    }

    pub(crate) fn update_inode_ctime_metadata(
        &self,
        inode: &Ext4Inode,
        timestamp: Ext4Timestamp,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
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
        Ok(updated_inode)
    }

    /// Applies a journaled inode metadata update.
    pub fn update_inode_metadata(
        &mut self,
        inode: &Ext4Inode,
        update: Ext4InodeMetadataUpdate,
    ) -> Ext4Result<Ext4Inode> {
        if update.is_empty() {
            return Ok(inode.clone());
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
    ) -> Ext4Result<Ext4Inode> {
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
        Ok(updated_inode)
    }

    pub(crate) fn update_inode_flags_timestamps_metadata(
        &self,
        inode: &Ext4Inode,
        flags: u32,
        timestamp: Ext4Timestamp,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
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
        Ok(updated_inode)
    }

    pub(crate) fn update_inode_links_count_metadata(
        &self,
        inode: &Ext4Inode,
        links_count: u16,
        timestamp: Ext4Timestamp,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
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
        Ok(updated_inode)
    }

    pub(crate) fn update_inode_links_count_ctime_metadata(
        &self,
        inode: &Ext4Inode,
        links_count: u16,
        timestamp: Ext4Timestamp,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
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
        Ok(updated_inode)
    }

    pub(crate) fn update_unlinked_inode_metadata(
        &self,
        inode: &Ext4Inode,
        size: Option<u64>,
        timestamp: Ext4Timestamp,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
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
        Ok(updated_inode)
    }

    pub(crate) fn update_inode_blocks_metadata(
        &self,
        inode: &Ext4Inode,
        blocks: u64,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
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
        Ok(updated_inode)
    }

    pub(crate) fn update_inode_orphan_next(
        &self,
        inode: &Ext4Inode,
        next: Option<InodeNumber>,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
        let next = next.map_or(0, |inode| inode.get());
        let inode_table_block = self.inode_table_entry_block(inode.number())?;
        let mut inode_table_bytes = self
            .read_metadata_block(inode_table_block)?
            .as_ref()
            .to_vec();
        let updated_inode = self.update_inode_table_entry_allow_zero_links(
            &mut inode_table_bytes,
            inode.number(),
            |inode_bytes| put_u32(inode_bytes, disk_inode::DTIME_OFFSET, next),
        )?;
        let inode_table_access = self.metadata_io.write_access(inode_table_block, handle)?;
        replace_metadata_access_bytes(&inode_table_access, inode_table_bytes)?;
        Ok(updated_inode)
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
        EncodedTimestamp, Ext4DeviceId, Ext4Inode, Ext4Timestamp, InodeInitialization, InodeKind,
        RawTimestampFields, SymlinkStorage, disk_inode,
    };
    use crate::{CorruptKind, Ext4Error, InodeNumber, UnsupportedKind};

    const BLOCK_SIZE: u32 = 4096;

    #[test]
    fn classifies_fast_symlink_by_data_blocks_not_size_only() {
        let block = fast_symlink_block(b"hello.txt");
        let inode = symlink_inode(9, 0, 0, 0, block);

        let storage = inode
            .symlink_storage(BLOCK_SIZE, false)
            .expect("classify symlink storage");

        assert!(matches!(storage, SymlinkStorage::Fast(target) if target == b"hello.txt"));
    }

    #[test]
    fn classifies_short_block_mapped_symlink_as_block_mapped() {
        let inode = symlink_inode(9, 8, disk_inode::EXT4_EXTENTS_FL, 0, [0; 60]);

        let storage = inode
            .symlink_storage(BLOCK_SIZE, false)
            .expect("classify symlink storage");

        assert_eq!(storage, SymlinkStorage::BlockMapped);
    }

    #[test]
    fn excludes_xattr_blocks_from_fast_symlink_data_blocks() {
        let block = fast_symlink_block(b"hello.txt");
        let inode = symlink_inode(9, 8, 0, 42, block);

        let storage = inode
            .symlink_storage(BLOCK_SIZE, false)
            .expect("classify symlink storage");

        assert!(matches!(storage, SymlinkStorage::Fast(target) if target == b"hello.txt"));
    }

    #[test]
    fn classifies_symlink_with_filesystem_ea_inode_feature_not_inode_flag() {
        let block = fast_symlink_block(b"hello.txt");
        let inode = symlink_inode(9, 8, 0, 0, block);

        let storage = inode
            .symlink_storage(BLOCK_SIZE, true)
            .expect("classify symlink storage");

        assert!(matches!(storage, SymlinkStorage::Fast(target) if target == b"hello.txt"));
    }

    #[test]
    fn ignores_inode_ea_inode_flag_when_filesystem_feature_is_absent() {
        let inode = symlink_inode(9, 8, disk_inode::EXT4_EA_INODE_FL, 0, [0; 60]);

        let storage = inode
            .symlink_storage(BLOCK_SIZE, false)
            .expect("classify symlink storage");

        assert_eq!(storage, SymlinkStorage::BlockMapped);
    }

    #[test]
    fn rejects_zero_length_fast_symlink() {
        let inode = symlink_inode(0, 0, 0, 0, fast_symlink_block(b"hello.txt"));

        assert_eq!(
            inode.symlink_storage(BLOCK_SIZE, false),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidInode))
        );
    }

    #[test]
    fn rejects_fast_symlink_size_that_reaches_past_i_block() {
        let inode = symlink_inode(60, 0, 0, 0, [b'a'; disk_inode::INODE_BLOCK_BYTES]);

        assert_eq!(
            inode.symlink_storage(BLOCK_SIZE, false),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidInode))
        );
    }

    #[test]
    fn rejects_fast_symlink_without_nul_terminator_at_size() {
        let mut block = [b'a'; disk_inode::INODE_BLOCK_BYTES];
        block[..9].copy_from_slice(b"hello.txt");
        let inode = symlink_inode(9, 0, 0, 0, block);

        assert_eq!(
            inode.symlink_storage(BLOCK_SIZE, false),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidInode))
        );
    }

    #[test]
    fn rejects_fast_symlink_with_embedded_nul_before_size() {
        let mut block = fast_symlink_block(b"hello.txt");
        block[4] = 0;
        let inode = symlink_inode(9, 0, 0, 0, block);

        assert_eq!(
            inode.symlink_storage(BLOCK_SIZE, false),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidInode))
        );
    }

    #[test]
    fn decodes_old_device_id_from_i_block_zero() {
        let mut block = [0; disk_inode::INODE_BLOCK_BYTES];
        put_u32(&mut block, 0, (12 << 8) | 34);
        let inode = special_inode(InodeKind::CharacterDevice, disk_inode::S_IFCHR, block);

        let device = inode
            .device_id()
            .expect("decode device id")
            .expect("character device has rdev");

        assert_eq!(device.major(), 12);
        assert_eq!(device.minor(), 34);
    }

    #[test]
    fn decodes_new_device_id_from_i_block_one() {
        let mut block = [0; disk_inode::INODE_BLOCK_BYTES];
        put_u32(&mut block, 4, encode_new_device_id(0xabc, 0xdef0));
        let inode = special_inode(InodeKind::BlockDevice, disk_inode::S_IFBLK, block);

        let device = inode
            .device_id()
            .expect("decode device id")
            .expect("block device has rdev");

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
        Ext4Inode {
            number: InodeNumber::new(12),
            kind: InodeKind::Symlink,
            mode: disk_inode::S_IFLNK,
            uid: 0,
            gid: 0,
            size,
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
            dtime: 0,
        }
    }

    fn special_inode(
        kind: InodeKind,
        mode: u16,
        block: [u8; disk_inode::INODE_BLOCK_BYTES],
    ) -> Ext4Inode {
        Ext4Inode {
            number: InodeNumber::new(12),
            kind,
            mode,
            uid: 0,
            gid: 0,
            size: 0,
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
            dtime: 0,
        }
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

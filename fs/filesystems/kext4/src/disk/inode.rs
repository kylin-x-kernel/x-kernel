// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;

use crate::{CorruptKind, Ext4Error, Ext4Result, disk::codec};

pub(crate) const GOOD_OLD_INODE_SIZE: usize = 128;
pub(crate) const INODE_BLOCK_BYTES: usize = 60;
pub(crate) const DTIME_OFFSET: usize = 0x14;
pub(crate) const BLOCKS_LO_OFFSET: usize = 0x1c;
pub(crate) const I_BLOCK_OFFSET: usize = 0x28;
pub(crate) const BLOCKS_HI_OFFSET: usize = 0x74;
pub(crate) const CHECKSUM_LO_OFFSET: usize = 0x7c;
pub(crate) const EXTRA_ISIZE_OFFSET: usize = 0x80;
pub(crate) const CHECKSUM_HI_OFFSET: usize = 0x82;
pub(crate) const CTIME_EXTRA_OFFSET: usize = 0x84;
pub(crate) const MTIME_EXTRA_OFFSET: usize = 0x88;
pub(crate) const ATIME_EXTRA_OFFSET: usize = 0x8c;
pub(crate) const EXT4_ROOT_INO: u32 = 2;

pub(crate) const S_IFMT: u16 = 0xf000;
pub(crate) const S_IFIFO: u16 = 0x1000;
pub(crate) const S_IFCHR: u16 = 0x2000;
pub(crate) const S_IFDIR: u16 = 0x4000;
pub(crate) const S_IFBLK: u16 = 0x6000;
pub(crate) const S_IFREG: u16 = 0x8000;
pub(crate) const S_IFLNK: u16 = 0xa000;
pub(crate) const S_IFSOCK: u16 = 0xc000;

pub(crate) const EXT4_IMMUTABLE_FL: u32 = 0x0000_0010;
pub(crate) const EXT4_APPEND_FL: u32 = 0x0000_0020;
pub(crate) const EXT4_ENCRYPT_FL: u32 = 0x0000_0800;
pub(crate) const EXT4_INDEX_FL: u32 = 0x0000_1000;
pub(crate) const EXT4_HUGE_FILE_FL: u32 = 0x0004_0000;
pub(crate) const EXT4_EXTENTS_FL: u32 = 0x0008_0000;
pub(crate) const EXT4_INLINE_DATA_FL: u32 = 0x1000_0000;
pub(crate) const EXT4_CASEFOLD_FL: u32 = 0x4000_0000;
#[cfg(test)]
pub(crate) const EXT4_EA_INODE_FL: u32 = 0x0020_0000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RawInode {
    mode: u16,
    uid: u32,
    gid: u32,
    size: u64,
    blocks: u64,
    flags: u32,
    block: [u8; INODE_BLOCK_BYTES],
    file_acl: u64,
    inline_xattr: Vec<u8>,
    generation: u32,
    links_count: u16,
    atime: u32,
    ctime: u32,
    mtime: u32,
    dtime: u32,
    atime_extra: Option<u32>,
    ctime_extra: Option<u32>,
    mtime_extra: Option<u32>,
    checksum_lo: u16,
    extra_isize: u16,
    checksum_hi: u16,
}

impl RawInode {
    pub(crate) fn decode(input: &[u8]) -> Ext4Result<Self> {
        let mode = codec::le_u16(input, 0x00)?;
        let uid_lo = codec::le_u16(input, 0x02)?;
        let size_lo = codec::le_u32(input, 0x04)?;
        let atime = codec::le_u32(input, 0x08)?;
        let ctime = codec::le_u32(input, 0x0c)?;
        let mtime = codec::le_u32(input, 0x10)?;
        let dtime = codec::le_u32(input, DTIME_OFFSET)?;
        let gid_lo = codec::le_u16(input, 0x18)?;
        let links_count = codec::le_u16(input, 0x1a)?;
        let blocks_lo = codec::le_u32(input, BLOCKS_LO_OFFSET)?;
        let flags = codec::le_u32(input, 0x20)?;
        let block = codec::bytes(input, 0x28)?;
        let generation = codec::le_u32(input, 0x64)?;
        let file_acl_lo = codec::le_u32(input, 0x68)?;
        let size_high = codec::le_u32(input, 0x6c)?;
        let file_acl_high = u64::from(codec::le_u16(input, 0x70)?);
        let blocks_high = u64::from(codec::le_u16(input, BLOCKS_HI_OFFSET)?);
        let uid_high = codec::le_u16(input, 0x78)?;
        let gid_high = codec::le_u16(input, 0x7a)?;
        let checksum_lo = codec::le_u16(input, CHECKSUM_LO_OFFSET)?;
        let extra_isize = if input.len() >= EXTRA_ISIZE_OFFSET + 2 {
            codec::le_u16(input, EXTRA_ISIZE_OFFSET)?
        } else {
            0
        };
        let checksum_hi = if input.len() >= CHECKSUM_HI_OFFSET + 2 {
            codec::le_u16(input, CHECKSUM_HI_OFFSET)?
        } else {
            0
        };
        let inline_xattr_offset = GOOD_OLD_INODE_SIZE
            .checked_add(usize::from(extra_isize))
            .ok_or(Ext4Error::Overflow)?;
        if inline_xattr_offset > input.len() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInode));
        }
        let inline_xattr = Vec::from(
            input
                .get(inline_xattr_offset..)
                .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?,
        );
        let ctime_extra = decode_extra_u32(input, extra_isize, CTIME_EXTRA_OFFSET)?;
        let mtime_extra = decode_extra_u32(input, extra_isize, MTIME_EXTRA_OFFSET)?;
        let atime_extra = decode_extra_u32(input, extra_isize, ATIME_EXTRA_OFFSET)?;

        Ok(Self {
            mode,
            uid: u32::from(uid_lo) | (u32::from(uid_high) << 16),
            gid: u32::from(gid_lo) | (u32::from(gid_high) << 16),
            size: u64::from(size_lo) | (u64::from(size_high) << 32),
            blocks: u64::from(blocks_lo) | (blocks_high << 32),
            flags,
            block,
            file_acl: u64::from(file_acl_lo) | (file_acl_high << 32),
            inline_xattr,
            generation,
            links_count,
            atime,
            ctime,
            mtime,
            dtime,
            atime_extra,
            ctime_extra,
            mtime_extra,
            checksum_lo,
            extra_isize,
            checksum_hi,
        })
    }

    pub(crate) const fn mode(&self) -> u16 {
        self.mode
    }

    pub(crate) const fn uid(&self) -> u32 {
        self.uid
    }

    pub(crate) const fn gid(&self) -> u32 {
        self.gid
    }

    pub(crate) const fn size(&self) -> u64 {
        self.size
    }

    pub(crate) const fn blocks(&self) -> u64 {
        self.blocks
    }

    pub(crate) const fn flags(&self) -> u32 {
        self.flags
    }

    pub(crate) const fn block(&self) -> &[u8; INODE_BLOCK_BYTES] {
        &self.block
    }

    pub(crate) const fn file_acl(&self) -> u64 {
        self.file_acl
    }

    pub(crate) fn inline_xattr(&self) -> &[u8] {
        &self.inline_xattr
    }

    pub(crate) const fn generation(&self) -> u32 {
        self.generation
    }

    pub(crate) const fn links_count(&self) -> u16 {
        self.links_count
    }

    pub(crate) const fn atime(&self) -> u32 {
        self.atime
    }

    pub(crate) const fn ctime(&self) -> u32 {
        self.ctime
    }

    pub(crate) const fn mtime(&self) -> u32 {
        self.mtime
    }

    pub(crate) const fn dtime(&self) -> u32 {
        self.dtime
    }

    pub(crate) const fn atime_extra(&self) -> Option<u32> {
        self.atime_extra
    }

    pub(crate) const fn ctime_extra(&self) -> Option<u32> {
        self.ctime_extra
    }

    pub(crate) const fn mtime_extra(&self) -> Option<u32> {
        self.mtime_extra
    }

    pub(crate) const fn checksum_lo(&self) -> u16 {
        self.checksum_lo
    }

    pub(crate) const fn extra_isize(&self) -> u16 {
        self.extra_isize
    }

    pub(crate) const fn checksum_hi(&self) -> u16 {
        self.checksum_hi
    }
}

fn decode_extra_u32(input: &[u8], extra_isize: u16, offset: usize) -> Ext4Result<Option<u32>> {
    if !extra_field_fits(extra_isize, offset, 4)? {
        return Ok(None);
    }
    Ok(Some(codec::le_u32(input, offset)?))
}

fn extra_field_fits(extra_isize: u16, offset: usize, size: usize) -> Ext4Result<bool> {
    let end = offset.checked_add(size).ok_or(Ext4Error::Overflow)?;
    let available = GOOD_OLD_INODE_SIZE
        .checked_add(usize::from(extra_isize))
        .ok_or(Ext4Error::Overflow)?;
    Ok(end <= available)
}

#[cfg(test)]
mod tests {
    use super::{
        ATIME_EXTRA_OFFSET, CTIME_EXTRA_OFFSET, EXTRA_ISIZE_OFFSET, MTIME_EXTRA_OFFSET, RawInode,
    };

    #[test]
    fn decodes_owner_and_timestamp_fields() {
        let mut bytes = [0; 256];
        put_u16(&mut bytes, 0x02, 0x5678);
        put_u32(&mut bytes, 0x08, 11);
        put_u32(&mut bytes, 0x0c, 22);
        put_u32(&mut bytes, 0x10, 33);
        put_u32(&mut bytes, 0x14, 44);
        put_u16(&mut bytes, 0x18, 0xdef0);
        put_u32(&mut bytes, 0x68, 0x7654_3210);
        put_u16(&mut bytes, 0x70, 0xfedc);
        put_u16(&mut bytes, 0x78, 0x1234);
        put_u16(&mut bytes, 0x7a, 0x9abc);
        put_u16(&mut bytes, EXTRA_ISIZE_OFFSET, 16);
        put_u32(&mut bytes, CTIME_EXTRA_OFFSET, 0x0000_0001);
        put_u32(&mut bytes, MTIME_EXTRA_OFFSET, 0x0000_0002);
        put_u32(&mut bytes, ATIME_EXTRA_OFFSET, 0x0000_0003);

        let inode = RawInode::decode(&bytes).expect("decode inode metadata fields");

        assert_eq!(inode.uid(), 0x1234_5678);
        assert_eq!(inode.gid(), 0x9abc_def0);
        assert_eq!(inode.file_acl(), 0xfedc_7654_3210);
        assert_eq!(inode.atime(), 11);
        assert_eq!(inode.ctime(), 22);
        assert_eq!(inode.mtime(), 33);
        assert_eq!(inode.dtime(), 44);
        assert_eq!(inode.atime_extra(), Some(0x0000_0003));
        assert_eq!(inode.ctime_extra(), Some(0x0000_0001));
        assert_eq!(inode.mtime_extra(), Some(0x0000_0002));
    }

    fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
        bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Inode-scoped file extent mapping interfaces.

use crate::{VfsError, VfsInode, VfsResult};

bitflags::bitflags! {
    /// Controls how an inode extent map is prepared.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct FiemapFlags: u32 {
        /// Write dirty file data before collecting the extent map.
        const SYNC = 1 << 0;
        /// Query the extended-attribute block mapping.
        const XATTR = 1 << 1;
        /// Ask the filesystem to cache extent metadata before reporting.
        const CACHE = 1 << 2;
    }

    /// Properties of one reported FIEMAP extent.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct FiemapExtentFlags: u32 {
        /// This is the last extent in the queried range.
        const LAST = 1 << 0;
        /// The physical location is unknown.
        const UNKNOWN = 1 << 1;
        /// Physical allocation is delayed.
        const DELALLOC = 1 << 2;
        /// Data cannot be decoded by reading the block device directly.
        const ENCODED = 1 << 3;
        /// File data is encrypted.
        const DATA_ENCRYPTED = 1 << 7;
        /// Logical, physical, or length values may not be block-aligned.
        const NOT_ALIGNED = 1 << 8;
        /// File data is stored inline with metadata.
        const DATA_INLINE = 1 << 9;
        /// File data shares a packed tail block.
        const DATA_TAIL = 1 << 10;
        /// Blocks are allocated but have not been initialized.
        const UNWRITTEN = 1 << 11;
        /// Adjacent block mappings were merged for reporting.
        const MERGED = 1 << 12;
        /// Physical blocks are shared with another file.
        const SHARED = 1 << 13;
    }
}

/// Writes validated FIEMAP extent fields to an ABI-specific destination.
pub trait FiemapExtentWriter {
    /// Writes one extent at `index` in the caller-provided output array.
    fn write_extent(
        &mut self,
        index: u32,
        logical: u64,
        physical: u64,
        length: u64,
        flags: FiemapExtentFlags,
    ) -> VfsResult<()>;
}

/// Safe FIEMAP request state passed to an inode operation.
///
/// This owns the request flags and extent counters while borrowing a writer
/// that encapsulates the caller-specific output destination.
pub struct FiemapExtentInfo<'a> {
    flags: FiemapFlags,
    extents_max: u32,
    extents_mapped: u32,
    writer: &'a mut dyn FiemapExtentWriter,
}

impl<'a> FiemapExtentInfo<'a> {
    /// Creates request state for an extent array with `extents_max` entries.
    pub fn new(
        flags: FiemapFlags,
        extents_max: u32,
        writer: &'a mut dyn FiemapExtentWriter,
    ) -> Self {
        Self {
            flags,
            extents_max,
            extents_mapped: 0,
            writer,
        }
    }

    /// Returns the current input/output request flags.
    pub const fn flags(&self) -> FiemapFlags {
        self.flags
    }

    /// Removes filesystem-consumed request flags before generic preparation.
    pub fn remove_flags(&mut self, flags: FiemapFlags) {
        self.flags.remove(flags);
    }

    /// Returns the number of extents counted or written so far.
    pub const fn mapped_extents(&self) -> u32 {
        self.extents_mapped
    }

    /// Validates a request and performs an optional data-only writeback.
    ///
    /// `max_file_size` is the filesystem's cached limit for this inode format.
    /// `supported_flags` contains filesystem-specific flags already handled by
    /// the caller. `SYNC` is accepted for every inode FIEMAP implementation.
    pub fn prepare(
        &mut self,
        inode: &VfsInode,
        start: u64,
        length: &mut u64,
        max_file_size: u64,
        supported_flags: FiemapFlags,
    ) -> VfsResult<()> {
        if *length == 0 {
            return Err(VfsError::InvalidInput);
        }

        if start >= max_file_size {
            return Err(VfsError::FileTooLarge);
        }
        if *length > max_file_size || max_file_size - *length < start {
            *length = max_file_size - start;
        }

        let accepted_flags = supported_flags | FiemapFlags::SYNC;
        let unsupported_bits = self.flags.bits() & !accepted_flags.bits();
        if unsupported_bits != 0 {
            self.flags = FiemapFlags::from_bits_retain(unsupported_bits);
            return Err(VfsError::from(kerrno::LinuxError::EBADR));
        }

        if self.flags.contains(FiemapFlags::SYNC) {
            inode.sync(true)?;
        }
        Ok(())
    }

    /// Validates and emits one mapped extent.
    ///
    /// Returns `false` when the output array is full or `LAST` completes the
    /// mapping. A zero-sized output array counts extents without writing.
    pub fn fill_next_extent(
        &mut self,
        logical: u64,
        physical: u64,
        length: u64,
        mut flags: FiemapExtentFlags,
    ) -> VfsResult<bool> {
        if length == 0
            || logical.checked_add(length).is_none()
            || physical.checked_add(length).is_none()
        {
            return Err(VfsError::InvalidInput);
        }
        if flags.contains(FiemapExtentFlags::DELALLOC) {
            flags.insert(FiemapExtentFlags::UNKNOWN);
        }
        if flags.contains(FiemapExtentFlags::DATA_ENCRYPTED) {
            flags.insert(FiemapExtentFlags::ENCODED);
        }
        if flags.intersects(FiemapExtentFlags::DATA_INLINE | FiemapExtentFlags::DATA_TAIL) {
            flags.insert(FiemapExtentFlags::NOT_ALIGNED);
        }

        if self.extents_max == 0 {
            self.extents_mapped = self
                .extents_mapped
                .checked_add(1)
                .ok_or(VfsError::OutOfRange)?;
            return Ok(!flags.contains(FiemapExtentFlags::LAST));
        }
        if self.extents_mapped >= self.extents_max {
            return Ok(false);
        }

        self.writer
            .write_extent(self.extents_mapped, logical, physical, length, flags)?;
        self.extents_mapped = self
            .extents_mapped
            .checked_add(1)
            .ok_or(VfsError::OutOfRange)?;
        Ok(self.extents_mapped < self.extents_max && !flags.contains(FiemapExtentFlags::LAST))
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::vec::Vec;

    use unittest::def_test;

    use super::{FiemapExtentFlags, FiemapExtentInfo, FiemapExtentWriter, FiemapFlags};
    use crate::VfsResult;

    #[derive(Default)]
    struct CollectingWriter {
        extents: Vec<(u64, u64, u64, FiemapExtentFlags)>,
    }

    impl FiemapExtentWriter for CollectingWriter {
        fn write_extent(
            &mut self,
            index: u32,
            logical: u64,
            physical: u64,
            length: u64,
            flags: FiemapExtentFlags,
        ) -> VfsResult<()> {
            assert_eq!(usize::try_from(index).ok(), Some(self.extents.len()));
            self.extents.push((logical, physical, length, flags));
            Ok(())
        }
    }

    #[def_test]
    fn fill_next_extent_adds_implied_flags() {
        let mut writer = CollectingWriter::default();
        let mut info = FiemapExtentInfo::new(FiemapFlags::empty(), 1, &mut writer);

        assert!(
            !info
                .fill_next_extent(0, 0, 4096, FiemapExtentFlags::DELALLOC)
                .expect("delayed extent should be valid")
        );
        assert_eq!(info.mapped_extents(), 1);
        assert!(
            writer.extents[0]
                .3
                .contains(FiemapExtentFlags::DELALLOC | FiemapExtentFlags::UNKNOWN)
        );
    }

    #[def_test]
    fn zero_capacity_counts_until_last_extent() {
        let mut writer = CollectingWriter::default();
        let mut info = FiemapExtentInfo::new(FiemapFlags::empty(), 0, &mut writer);

        assert!(
            info.fill_next_extent(0, 4096, 4096, FiemapExtentFlags::empty())
                .expect("first extent should count")
        );
        assert!(
            !info
                .fill_next_extent(8192, 12288, 4096, FiemapExtentFlags::LAST)
                .expect("last extent should count")
        );
        assert_eq!(info.mapped_extents(), 2);
        assert!(writer.extents.is_empty());
    }

    #[def_test]
    fn fill_next_extent_rejects_invalid_ranges() {
        let mut writer = CollectingWriter::default();
        let mut info = FiemapExtentInfo::new(FiemapFlags::empty(), 1, &mut writer);

        assert!(
            info.fill_next_extent(0, 0, 0, FiemapExtentFlags::empty())
                .is_err()
        );
        assert!(
            info.fill_next_extent(u64::MAX, 0, 1, FiemapExtentFlags::empty())
                .is_err()
        );
        assert!(
            info.fill_next_extent(0, u64::MAX, 1, FiemapExtentFlags::empty())
                .is_err()
        );
    }
}

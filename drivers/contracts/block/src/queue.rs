// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Block request-queue geometry and topology.

use driver_base::{DriverError, DriverResult};

/// Shift from bytes to Linux block sectors.
pub const SECTOR_SHIFT: u32 = 9;

/// Size in bytes of the fixed sector unit used by the block core.
pub const SECTOR_SIZE: u32 = 1 << SECTOR_SHIFT;

// X-Kernel currently uses 4-KiB base pages and has no transparent-huge-page
// block-size path. This matches Linux `BLK_MAX_BLOCK_SIZE` in that model.
const MAX_BLOCK_SIZE: u32 = 4096;

/// Hardware geometry and I/O topology exported by a request queue.
///
/// This is the currently used subset of Linux `struct queue_limits`. All sizes
/// and offsets are expressed in bytes. A logical block is never smaller than a
/// sector and is always a power of two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueLimits {
    logical_block_size: u32,
    physical_block_size: u32,
    alignment_offset: u32,
    io_min: u32,
    io_opt: u32,
}

impl QueueLimits {
    /// Creates queue limits for a device that reports only a logical block size.
    ///
    /// The physical and minimum I/O sizes default to the logical block size;
    /// the optimal I/O size and alignment offset default to zero, matching the
    /// Linux queue-limit defaults for unspecified topology.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError::InvalidInput`] unless `logical_block_size` is
    /// between 512 and 4096 bytes inclusive and is a power of two.
    pub fn new(logical_block_size: usize) -> DriverResult<Self> {
        let logical_block_size =
            u32::try_from(logical_block_size).map_err(|_| DriverError::InvalidInput)?;
        if !(SECTOR_SIZE..=MAX_BLOCK_SIZE).contains(&logical_block_size)
            || !logical_block_size.is_power_of_two()
        {
            return Err(DriverError::InvalidInput);
        }

        Ok(Self {
            logical_block_size,
            physical_block_size: logical_block_size,
            alignment_offset: 0,
            io_min: logical_block_size,
            io_opt: 0,
        })
    }

    /// Returns the logical block size in bytes.
    pub const fn logical_block_size(&self) -> u32 {
        self.logical_block_size
    }

    /// Returns the physical block size in bytes.
    pub const fn physical_block_size(&self) -> u32 {
        self.physical_block_size
    }

    /// Returns the offset in bytes to the device's preferred alignment.
    pub const fn alignment_offset(&self) -> u32 {
        self.alignment_offset
    }

    /// Returns the minimum preferred I/O size in bytes.
    pub const fn io_min(&self) -> u32 {
        self.io_min
    }

    /// Returns the optimal I/O size in bytes, or zero when unspecified.
    pub const fn io_opt(&self) -> u32 {
        self.io_opt
    }

    pub(crate) fn logical_block_sectors(&self) -> u64 {
        u64::from(self.logical_block_size / SECTOR_SIZE)
    }
}

/// A block request queue and its device limits.
///
/// Request dispatch state is added with the block multi-queue path. Keeping
/// limits on this object now gives every consumer one canonical geometry source.
pub struct RequestQueue {
    limits: QueueLimits,
}

impl RequestQueue {
    /// Creates a request queue with validated device limits.
    pub const fn new(limits: QueueLimits) -> Self {
        Self { limits }
    }

    /// Returns the queue's hardware geometry and I/O topology.
    pub const fn limits(&self) -> &QueueLimits {
        &self.limits
    }
}

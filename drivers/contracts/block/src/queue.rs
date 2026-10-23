// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Block request-queue geometry and topology.

use alloc::sync::Arc;

use driver_base::{DriverError, DriverResult};

use crate::Bio;

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

/// A block request queue with exactly one data dispatch contract.
pub struct RequestQueue {
    limits: QueueLimits,
    dispatch: Dispatch,
}

pub(crate) enum Dispatch {
    Bio(Arc<dyn crate::BioOperations>),
    Request(Arc<dyn BlkMqOperations>),
}

impl RequestQueue {
    pub(crate) fn new(limits: QueueLimits, dispatch: Dispatch) -> Self {
        Self { limits, dispatch }
    }

    pub(crate) fn submit(&self, bio: Bio) {
        match &self.dispatch {
            Dispatch::Bio(operations) => operations.submit_bio(bio),
            Dispatch::Request(operations) => {
                let request = Request {
                    bio,
                    vector_index: 0,
                    vector_offset: 0,
                };
                operations.queue_rq(request);
            }
        }
    }

    /// Returns the queue's hardware geometry and I/O topology.
    pub const fn limits(&self) -> &QueueLimits {
        &self.limits
    }
}

/// Driver dispatch for request-based disks, corresponding to Linux blk_mq_ops.
pub trait BlkMqOperations: Send + Sync {
    /// Takes ownership in sleepable task context, independently of completion.
    /// The device queue retains it until DMA reclamation and calls [`Request::complete`],
    /// including on failure.
    ///
    /// Ownership prevents duplicate completion, but cannot require completion.
    /// Dropping the request does not publish a result or cancel device access;
    /// omitting `complete` leaves synchronous callers blocked indefinitely.
    fn queue_rq(&self, request: Request);
}

/// One owned, in-flight bio. Hardware admission and descriptors stay with the driver.
#[must_use]
pub struct Request {
    bio: Bio,
    vector_index: usize,
    vector_offset: usize,
}

impl Request {
    /// Returns the operation being executed.
    pub fn operation(&self) -> crate::BioOperation {
        self.bio.operation()
    }

    /// Returns the whole-disk sector at the current transfer cursor.
    pub fn sector(&self) -> u64 {
        self.bio.sector()
    }

    /// Returns the memory extents while hardware does not own their bytes.
    pub fn vectors_mut(&mut self) -> impl ExactSizeIterator<Item = crate::BioVecRef<'_>> {
        self.bio
            .vectors_from_mut(self.vector_index, self.vector_offset)
    }

    /// Advances a retired, sector-aligned transfer prefix without releasing any
    /// memory owners. The terminal callback still receives every original extent.
    /// Call only after hardware relinquishes the submitted prefix.
    ///
    /// # Errors
    /// Returns InvalidInput without moving the cursor for a non-sector-aligned
    /// count or one exceeding the remaining request.
    pub fn advance(&mut self, bytes: usize) -> DriverResult {
        if !bytes.is_multiple_of(SECTOR_SIZE as usize) {
            return Err(DriverError::InvalidInput);
        }
        let (mut index, mut offset, mut left) = (self.vector_index, self.vector_offset, bytes);
        while let Some(len) = self.bio.vector_len(index) {
            let count = (len - offset).min(left);
            offset += count;
            left -= count;
            if offset != len {
                break;
            }
            index += 1;
            offset = 0;
        }
        if left != 0 {
            return Err(DriverError::InvalidInput);
        }
        let sector = self
            .bio
            .sector()
            .checked_add((bytes / SECTOR_SIZE as usize) as u64)
            .ok_or(DriverError::InvalidInput)?;
        self.vector_index = index;
        self.vector_offset = offset;
        self.bio.remap(sector);
        Ok(())
    }

    /// Whether the transfer cursor reached the end of all data extents.
    /// Flush requests have no data and require their own device completion.
    pub fn is_data_complete(&self) -> bool {
        self.bio.vector_len(self.vector_index).is_none()
    }

    /// Runs a contiguous-only read backend, staging scatter extents if needed.
    ///
    /// # Errors
    /// Propagates invalid memory/operation, length overflow and backend errors.
    pub fn read_contiguous(
        &mut self,
        read: impl FnOnce(&mut [u8]) -> DriverResult,
    ) -> DriverResult {
        self.bio
            .read_contiguous_at(self.vector_index, self.vector_offset, read)
    }

    /// Runs a contiguous-only write backend without copying a single extent.
    ///
    /// # Errors
    /// Propagates invalid operation, length overflow and backend errors.
    pub fn write_contiguous(&mut self, write: impl FnOnce(&[u8]) -> DriverResult) -> DriverResult {
        self.bio
            .write_contiguous_at(self.vector_index, self.vector_offset, write)
    }

    /// Completes the request after all device references have been retired.
    pub fn complete(self, status: DriverResult) {
        self.bio.end_io(status);
    }
}

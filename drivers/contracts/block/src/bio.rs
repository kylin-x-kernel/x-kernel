// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Owned I/O descriptions, independent of the submitting task's stack.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{ops::Range, ptr::NonNull};

use crate::{BlockDevice, DriverError, DriverResult, SECTOR_SIZE};

/// Operation performed by a bio or request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BioOperation {
    /// Read storage into memory.
    Read,
    /// Write memory to storage.
    Write,
    /// Make previously completed writes persistent.
    Flush,
}

/// Storage whose initialized bytes remain at a stable address during I/O.
///
/// An implementation may own an allocation or an exclusive access token for
/// existing storage. It must retain both storage and access until it is dropped.
///
/// # Safety
/// Both views must describe the same initialized allocation at a stable address
/// and length. Moving the owner must not move the bytes. No other party may
/// access the storage incompatibly with these borrows, including during DMA.
/// Destruction must be valid in the bio's completion context.
pub unsafe trait BioMemory: Send {
    /// Returns the stable initialized storage.
    fn bytes(&self) -> &[u8];
    /// Returns exclusive access to that same storage.
    fn bytes_mut(&mut self) -> &mut [u8];
}

// SAFETY: Box owns an initialized, stable allocation exclusively; moving the
// box does not move its bytes, and both views borrow that allocation.
unsafe impl BioMemory for Box<[u8]> {
    fn bytes(&self) -> &[u8] {
        self
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        self
    }
}

enum Memory {
    Buffer(Box<[u8]>),
    Owned(Box<dyn BioMemory>),
    Read(NonNull<[u8]>),
    Write(NonNull<[u8]>),
}

/// Borrowed access to a bio extent, without transferring its memory owner.
///
/// Keeping this view or one of its byte slices alive borrows the bio, so safe
/// code cannot complete the bio while still accessing the extent.
pub struct BioVecRef<'a> {
    vector: &'a mut BioVec,
    offset: usize,
}

impl BioVecRef<'_> {
    /// Returns shared access while the extent is not owned by DMA.
    pub fn bytes(&self) -> &[u8] {
        &self.vector.bytes()[self.offset..]
    }

    /// Returns exclusive access while the extent is not owned by DMA.
    ///
    /// # Errors
    /// Returns InvalidInput for a shared synchronous write source.
    pub fn bytes_mut(&mut self) -> DriverResult<&mut [u8]> {
        self.vector
            .bytes_mut()
            .map(|bytes| &mut bytes[self.offset..])
    }
}

/// One memory range and the responsibility for keeping it available to I/O.
/// Extent boundaries have no device-alignment meaning.
pub struct BioVec {
    memory: Memory,
    range: Range<usize>,
}

// SAFETY: Owned storage supplies Send and exclusive access via BioMemory.
// Borrowed ranges are constructed only by the synchronous adapter, which
// retains the borrow until terminal completion; writes expose shared access
// only, and reads retain exclusive access for the entire transfer.
unsafe impl Send for BioVec {}

impl BioVec {
    /// Transfers a byte allocation without copying it.
    pub fn new(buffer: Box<[u8]>) -> Self {
        let len = buffer.len();
        Self {
            memory: Memory::Buffer(buffer),
            range: 0..len,
        }
    }

    /// Retains an existing memory owner and selects an initialized byte range.
    ///
    /// # Errors
    /// Returns InvalidInput and the original owner for an empty, reversed or
    /// out-of-bounds range. No storage is erased or discarded on failure.
    pub fn from_memory<M: BioMemory + 'static>(
        memory: M,
        range: Range<usize>,
    ) -> Result<Self, (DriverError, M)> {
        if range.start >= range.end || range.end > memory.bytes().len() {
            return Err((DriverError::InvalidInput, memory));
        }
        Ok(Self {
            memory: Memory::Owned(Box::new(memory)),
            range,
        })
    }

    /// Adapts scatter memory for a backend requiring one contiguous read buffer.
    /// A single range is passed directly; only scatter requests use staging.
    ///
    /// # Errors
    /// Propagates inaccessible memory, length overflow, and backend errors.
    fn read_contiguous(
        vectors: &mut [Self],
        start_offset: usize,
        read: impl FnOnce(&mut [u8]) -> DriverResult,
    ) -> DriverResult {
        if let [vector] = vectors {
            return read(&mut vector.bytes_mut()?[start_offset..]);
        }
        let len = Self::total_len(vectors)? - start_offset;
        let mut bytes = alloc::vec![0; len];
        read(&mut bytes)?;
        let mut offset = 0;
        for (i, vector) in vectors.iter_mut().enumerate() {
            let target = &mut vector.bytes_mut()?[if i == 0 { start_offset } else { 0 }..];
            let end = offset + target.len();
            target.copy_from_slice(&bytes[offset..end]);
            offset = end;
        }
        Ok(())
    }

    /// Adapts scatter memory for a backend requiring one contiguous write buffer.
    /// A single range remains shared and is passed without copying.
    ///
    /// # Errors
    /// Propagates length overflow and backend errors.
    fn write_contiguous(
        vectors: &[Self],
        start_offset: usize,
        write: impl FnOnce(&[u8]) -> DriverResult,
    ) -> DriverResult {
        if let [vector] = vectors {
            return write(&vector.bytes()[start_offset..]);
        }
        let len = Self::total_len(vectors)? - start_offset;
        let mut bytes = alloc::vec::Vec::with_capacity(len);
        for (i, vector) in vectors.iter().enumerate() {
            bytes.extend_from_slice(&vector.bytes()[if i == 0 { start_offset } else { 0 }..]);
        }
        write(&bytes)
    }

    fn total_len(vectors: &[Self]) -> DriverResult<usize> {
        vectors.iter().try_fold(0usize, |len, vector| {
            len.checked_add(vector.bytes().len())
                .ok_or(DriverError::InvalidInput)
        })
    }

    /// # Safety
    /// The exclusive borrow must remain live until completion has retired all
    /// device access and the completion callback has disposed of this vector.
    pub(crate) unsafe fn borrowed_read(buffer: &mut [u8]) -> Self {
        let len = buffer.len();
        Self {
            memory: Memory::Read(NonNull::from(buffer)),
            range: 0..len,
        }
    }

    /// # Safety
    /// The shared borrow must remain live until completion has retired all
    /// device access and the completion callback has disposed of this vector.
    pub(crate) unsafe fn borrowed_write(buffer: &[u8]) -> Self {
        Self {
            memory: Memory::Write(NonNull::from(buffer)),
            range: 0..buffer.len(),
        }
    }

    /// Returns bytes when the extent is not owned by DMA.
    pub fn bytes(&self) -> &[u8] {
        let bytes = match &self.memory {
            Memory::Buffer(buffer) => buffer,
            Memory::Owned(memory) => memory.bytes(),
            // SAFETY: the synchronous adapter retains the caller's borrow
            // through completion. This view is shared and bounded by range.
            Memory::Read(pointer) | Memory::Write(pointer) => unsafe { pointer.as_ref() },
        };
        &bytes[self.range.clone()]
    }

    /// Returns exclusive access when the extent is not owned by DMA.
    ///
    /// # Errors
    /// Returns InvalidInput for a shared source supplied to synchronous write.
    pub fn bytes_mut(&mut self) -> DriverResult<&mut [u8]> {
        let bytes = match &mut self.memory {
            Memory::Buffer(buffer) => buffer,
            Memory::Owned(memory) => memory.bytes_mut(),
            // SAFETY: borrowed_read retains exclusive access until completion;
            // &mut self serializes CPU access before/after device ownership.
            Memory::Read(pointer) => unsafe { pointer.as_mut() },
            Memory::Write(_) => return Err(DriverError::InvalidInput),
        };
        Ok(&mut bytes[self.range.clone()])
    }
}

/// An owned I/O addressed in 512-byte sectors of a block-device view.
///
/// Submission transfers the bio, its device reference and all memory extents
/// to the driver. The driver may retain it after submission returns and must
/// call `end_io` only after retiring every DMA reference. Completion returns
/// the allocations to the caller through the callback, like Linux bi_end_io.
/// Extents may start at any byte address; backends and their DMA providers
/// adapt hardware alignment, mapping and cache requirements. Extent boundaries
/// need not align with sectors. The callback must not sleep: completion can run
/// in IRQ-enabled softirq context.
#[must_use]
pub struct Bio {
    device: Arc<BlockDevice>,
    sector: u64,
    operation: BioOperation,
    vectors: Vec<BioVec>,
    end_io: BioEndIo,
}

pub(crate) enum BioEndIo {
    Callback(Box<dyn FnOnce(DriverResult, Vec<BioVec>) + Send>),
    Wait(Arc<kspin::SpinNoIrq<crate::completion::BioWaitState>>),
}

impl Bio {
    /// Creates an I/O with a terminal callback. Validation occurs at submission.
    pub fn new(
        device: Arc<BlockDevice>,
        sector: u64,
        operation: BioOperation,
        vectors: Vec<BioVec>,
        end_io: impl FnOnce(DriverResult, Vec<BioVec>) + Send + 'static,
    ) -> Self {
        Self::with_completion(
            device,
            sector,
            operation,
            vectors,
            BioEndIo::Callback(Box::new(end_io)),
        )
    }

    pub(crate) fn with_completion(
        device: Arc<BlockDevice>,
        sector: u64,
        operation: BioOperation,
        vectors: Vec<BioVec>,
        end_io: BioEndIo,
    ) -> Self {
        Self {
            device,
            sector,
            operation,
            vectors,
            end_io,
        }
    }

    /// Returns the target device view.
    pub fn device(&self) -> &Arc<BlockDevice> {
        &self.device
    }

    /// Returns the device-relative sector before dispatch, whole-disk sector afterwards.
    pub fn sector(&self) -> u64 {
        self.sector
    }

    /// Returns the requested operation.
    pub fn operation(&self) -> BioOperation {
        self.operation
    }

    /// Returns the memory extents in transfer order, without inventing sector boundaries.
    pub fn vectors_mut(&mut self) -> impl ExactSizeIterator<Item = BioVecRef<'_>> {
        self.vectors_from_mut(0, 0)
    }

    pub(crate) fn vectors_from_mut(
        &mut self,
        index: usize,
        offset: usize,
    ) -> impl ExactSizeIterator<Item = BioVecRef<'_>> {
        self.vectors[index..]
            .iter_mut()
            .enumerate()
            .map(move |(i, vector)| BioVecRef {
                vector,
                offset: if i == 0 { offset } else { 0 },
            })
    }

    pub(crate) fn vector_len(&self, index: usize) -> Option<usize> {
        self.vectors.get(index).map(|vector| vector.range.len())
    }

    /// Adapts this bio for a backend accepting one contiguous read buffer.
    /// A single range is passed directly; only scatter requests use staging.
    ///
    /// # Errors
    /// Propagates inaccessible memory, length overflow and backend errors.
    pub fn read_contiguous(
        &mut self,
        read: impl FnOnce(&mut [u8]) -> DriverResult,
    ) -> DriverResult {
        self.read_contiguous_at(0, 0, read)
    }

    pub(crate) fn read_contiguous_at(
        &mut self,
        index: usize,
        offset: usize,
        read: impl FnOnce(&mut [u8]) -> DriverResult,
    ) -> DriverResult {
        if self.operation != BioOperation::Read {
            return Err(DriverError::InvalidInput);
        }
        BioVec::read_contiguous(&mut self.vectors[index..], offset, read)
    }

    /// Adapts this bio for a backend accepting one contiguous write buffer.
    /// A single range is passed directly as shared memory, without copying.
    ///
    /// # Errors
    /// Propagates invalid operation, length overflow and backend errors.
    pub fn write_contiguous(&mut self, write: impl FnOnce(&[u8]) -> DriverResult) -> DriverResult {
        self.write_contiguous_at(0, 0, write)
    }

    pub(crate) fn write_contiguous_at(
        &mut self,
        index: usize,
        offset: usize,
        write: impl FnOnce(&[u8]) -> DriverResult,
    ) -> DriverResult {
        if self.operation != BioOperation::Write {
            return Err(DriverError::InvalidInput);
        }
        BioVec::write_contiguous(&self.vectors[index..], offset, write)
    }

    /// Consumes the bio to publish its terminal result and return its memory.
    /// The backend must retire all device access before calling this method.
    pub fn end_io(self, status: DriverResult) {
        match self.end_io {
            BioEndIo::Callback(callback) => callback(status, self.vectors),
            BioEndIo::Wait(state) => {
                crate::completion::BioWaitState::complete(&state, (status, self.vectors))
            }
        }
    }

    pub(crate) fn validate(&self) -> DriverResult<u64> {
        if self.operation == BioOperation::Flush {
            return if self.vectors.is_empty() && self.sector == 0 {
                Ok(0)
            } else {
                Err(DriverError::InvalidInput)
            };
        }
        if self.operation == BioOperation::Read
            && self
                .vectors
                .iter()
                .any(|vector| matches!(vector.memory, Memory::Write(_)))
        {
            return Err(DriverError::InvalidInput);
        }
        let bytes = BioVec::total_len(&self.vectors)?;
        if bytes == 0 || !bytes.is_multiple_of(self.device.block_size()) {
            return Err(DriverError::InvalidInput);
        }
        Ok((bytes / SECTOR_SIZE as usize) as u64)
    }

    pub(crate) fn remap(&mut self, sector: u64) {
        self.sector = sector;
    }
}

/// Direct-bio data operations. Disk allocation requires this or BlkMqOperations.
/// There is no synchronous driver fallback.
pub trait BioOperations: crate::BlockDeviceOperations {
    /// Takes ownership in sleepable task context and eventually calls [`Bio::end_io`],
    /// including on failure, after retiring all device access.
    ///
    /// Ownership prevents duplicate completion, but cannot require completion.
    /// Dropping the bio does not publish a result or cancel device access;
    /// omitting `end_io` leaves synchronous callers blocked indefinitely.
    fn submit_bio(&self, bio: Bio);
}

impl<T: BioOperations + ?Sized> BioOperations for Arc<T> {
    fn submit_bio(&self, bio: Bio) {
        (**self).submit_bio(bio);
    }
}

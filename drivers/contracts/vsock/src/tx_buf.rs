// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Fixed-size TX buffers shared by vsock drivers and their consumers.

use alloc::{sync::Arc, vec::Vec};

use driver_base::{DriverError, DriverResult};
use kspin::SpinNoPreempt;

/// Maximum payload carried by one virtio-vsock data packet.
pub const TX_MAX_PAYLOAD_SIZE: usize = 64 * 1024;

/// A pool of fixed-size [`VsockTxBuf`] slots backed by one contiguous allocation.
///
/// Drivers choose the slot count and buffer length when constructing the pool. The pool does
/// not know the protocol header size; a driver sets the header length on a buffer before
/// handing it to the upper layer or filling a control packet.
pub struct VsockTxBufPool {
    slot_count: usize,
    buf_len: usize,
    storage: Vec<u8>,
    /// Stable base pointer into `storage`.
    ///
    /// Captured from `Vec::as_mut_ptr()` while `storage` is uniquely owned in `new()`. Slot
    /// slices must not be derived from a shared `Vec` reference, and the backing allocation is
    /// never resized or replaced afterwards.
    base: *mut u8,
    /// Free-slot offsets.
    ///
    /// Allocation and release are task-context operations; a driver that reclaims TX buffers from
    /// IRQ context must defer the release to task context instead of taking this preemption lock.
    free_offsets: SpinNoPreempt<Vec<usize>>,
}

// SAFETY: `base` points into the pool's own `storage` allocation, which is created in `new()`,
// never resized or replaced, and freed only after the last `Arc<VsockTxBufPool>` drops. Slot
// offsets are handed to exactly one `VsockTxBuf` at a time, and a buffer only reaches its own
// slot through reads (`&self`) or `&mut VsockTxBuf` writes, so two threads cannot form
// overlapping slices. Moving the pool moves the `Vec` handle, not its heap allocation.
unsafe impl Send for VsockTxBufPool {}

// SAFETY: shared access reads through `base` only within the caller's own slot or reads
// immutable geometry; writes require `&mut VsockTxBuf`, whose slot ownership rule above excludes
// every other access to that range.
unsafe impl Sync for VsockTxBufPool {}

impl VsockTxBufPool {
    /// Creates a pool with `slot_count` buffers of `buf_len` bytes each.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError::InvalidInput`] for zero slots or a zero buffer length, and
    /// [`DriverError::NoMemory`] if the backing allocation fails.
    pub fn new(slot_count: usize, buf_len: usize) -> DriverResult<Arc<Self>> {
        if slot_count == 0 || buf_len == 0 {
            return Err(DriverError::InvalidInput);
        }
        let storage_len = slot_count
            .checked_mul(buf_len)
            .ok_or(DriverError::InvalidInput)?;

        // Keep allocation failure on the ordinary driver error path rather than panicking during
        // device probing.
        let mut storage = Vec::new();
        storage
            .try_reserve_exact(storage_len)
            .map_err(|_| DriverError::NoMemory)?;
        storage.resize(storage_len, 0);
        // Capture the writable base pointer while `storage` is uniquely owned; slot slices are
        // derived from it instead of from a shared `Vec` reference.
        let base = storage.as_mut_ptr();

        let mut free_offsets = Vec::new();
        free_offsets
            .try_reserve_exact(slot_count)
            .map_err(|_| DriverError::NoMemory)?;
        for slot in 0..slot_count {
            free_offsets.push(slot * buf_len);
        }

        Ok(Arc::new(Self {
            slot_count,
            buf_len,
            storage,
            base,
            free_offsets: SpinNoPreempt::new(free_offsets),
        }))
    }

    /// Returns the number of buffers owned by the pool.
    pub const fn capacity(&self) -> usize {
        self.slot_count
    }

    /// Returns the capacity of each buffer in bytes.
    pub const fn buffer_len(&self) -> usize {
        self.buf_len
    }

    /// Allocates one buffer from the pool.
    ///
    /// Returns `None` when every slot is currently owned by a [`VsockTxBuf`]. Dropping a buffer
    /// returns its slot to the pool.
    pub fn alloc_buf(self: &Arc<Self>) -> Option<VsockTxBuf> {
        let pool_offset = self.free_offsets.lock().pop()?;
        Some(VsockTxBuf {
            pool: Arc::clone(self),
            pool_offset,
            buf_len: self.buf_len,
            hdr_len: 0,
            payload_len: 0,
        })
    }

    fn release_offset(&self, pool_offset: usize) {
        debug_assert_eq!(pool_offset % self.buf_len, 0);
        debug_assert!(pool_offset < self.storage.len());
        self.free_offsets.lock().push(pool_offset);
    }
}

/// An owned fixed-size TX buffer allocated from a [`VsockTxBufPool`].
///
/// The buffer layout mirrors `NetBuf`:
///
/// ```text
///   ______________________ capacity ______________________
///  /                                                      \
/// +------------------+------------------+------------------+
/// |      Header      |      Payload     |      Unused      |
/// +------------------+------------------+------------------+
/// |\__ hdr_len __/ \__ payload_len __/
/// |
/// buffer start
/// ```
///
/// Dropping the buffer returns its slot to the owning pool.
pub struct VsockTxBuf {
    pool: Arc<VsockTxBufPool>,
    pool_offset: usize,
    buf_len: usize,
    hdr_len: usize,
    payload_len: usize,
}

impl VsockTxBuf {
    fn get_slice(&self, start: usize, len: usize) -> &[u8] {
        debug_assert!(start <= self.buf_len);
        debug_assert!(len <= self.buf_len - start);
        let ptr = self.pool.base.wrapping_add(self.pool_offset + start);
        // SAFETY: `base` is captured from `as_mut_ptr()` in `new()` while the storage is uniquely
        // owned, and stays valid because the allocation is never resized or moved and lives
        // through this buffer's `Arc`. `check_frame_len` keeps `start + len <= buf_len` through
        // the setters, and `alloc_buf` hands the slot offset to one buffer, so no mutable alias
        // exists.
        unsafe { core::slice::from_raw_parts(ptr, len) }
    }

    fn get_slice_mut(&mut self, start: usize, len: usize) -> &mut [u8] {
        debug_assert!(start <= self.buf_len);
        debug_assert!(len <= self.buf_len - start);
        let ptr = self.pool.base.wrapping_add(self.pool_offset + start);
        // SAFETY: `base` is captured from `as_mut_ptr()` in `new()` while the storage is uniquely
        // owned, so writing through it is permitted; the allocation is never resized or moved and
        // lives through this buffer's `Arc`. `&mut self` solely owns the slot, which `alloc_buf`
        // hands to one buffer, and `check_frame_len` keeps `start + len <= buf_len`, so `len`
        // initialized bytes are in bounds and unaliased.
        unsafe { core::slice::from_raw_parts_mut(ptr, len) }
    }

    /// Returns the total capacity of the buffer in bytes.
    pub const fn capacity(&self) -> usize {
        self.buf_len
    }

    /// Returns the current header length.
    pub const fn hdr_len(&self) -> usize {
        self.hdr_len
    }

    /// Returns the current payload length.
    pub const fn payload_len(&self) -> usize {
        self.payload_len
    }

    /// Returns the maximum payload length available after the current header.
    pub const fn payload_capacity(&self) -> usize {
        self.buf_len - self.hdr_len
    }

    /// Returns the header bytes.
    pub fn header(&self) -> &[u8] {
        self.get_slice(0, self.hdr_len)
    }

    /// Returns the mutable header bytes.
    pub fn header_mut(&mut self) -> &mut [u8] {
        self.get_slice_mut(0, self.hdr_len)
    }

    /// Returns the payload bytes.
    pub fn payload(&self) -> &[u8] {
        self.get_slice(self.hdr_len, self.payload_len)
    }

    /// Returns the mutable payload bytes.
    pub fn payload_mut(&mut self) -> &mut [u8] {
        self.get_slice_mut(self.hdr_len, self.payload_len)
    }

    /// Returns the complete frame, header followed by payload.
    pub fn frame(&self) -> &[u8] {
        self.get_slice(0, self.hdr_len + self.payload_len)
    }

    /// Returns the complete mutable frame, header followed by payload.
    pub fn frame_mut(&mut self) -> &mut [u8] {
        self.get_slice_mut(0, self.hdr_len + self.payload_len)
    }

    /// Returns the entire buffer, including unused tail space.
    pub fn buffer(&self) -> &[u8] {
        self.get_slice(0, self.buf_len)
    }

    /// Returns the entire mutable buffer, including unused tail space.
    pub fn buffer_mut(&mut self) -> &mut [u8] {
        self.get_slice_mut(0, self.buf_len)
    }

    /// Sets the header length.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError::InvalidInput`] when `header_len + payload_len` exceeds capacity.
    pub fn set_hdr_len(&mut self, header_len: usize) -> DriverResult<()> {
        check_frame_len(header_len, self.payload_len, self.buf_len)?;
        self.hdr_len = header_len;
        Ok(())
    }

    /// Sets the payload length.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError::InvalidInput`] when `header_len + payload_len` exceeds capacity.
    pub fn set_payload_len(&mut self, payload_len: usize) -> DriverResult<()> {
        check_frame_len(self.hdr_len, payload_len, self.buf_len)?;
        self.payload_len = payload_len;
        Ok(())
    }

    /// Clears the header and payload lengths while retaining slot ownership.
    pub fn reset(&mut self) {
        self.hdr_len = 0;
        self.payload_len = 0;
    }
}

impl Drop for VsockTxBuf {
    fn drop(&mut self) {
        self.pool.release_offset(self.pool_offset);
    }
}

fn check_frame_len(hdr_len: usize, payload_len: usize, buf_len: usize) -> DriverResult<()> {
    let frame_len = hdr_len
        .checked_add(payload_len)
        .ok_or(DriverError::InvalidInput)?;
    if frame_len > buf_len {
        return Err(DriverError::InvalidInput);
    }
    Ok(())
}

unittest::path_mod_test!(tx_buf_unittest, "../ut-tests/tx_buf.rs");

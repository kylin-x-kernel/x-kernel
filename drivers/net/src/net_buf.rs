//! Network buffer types and pool allocator.
use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};
use core::ptr::NonNull;

use spin::Mutex;

use crate::{DriverError, DriverResult};

/// A raw buffer handle for network devices.
pub struct NetBufHandle {
    // The raw pointer of the owning object.
    owner_ptr: NonNull<u8>,
    // The pointer to the payload data.
    data_ptr: NonNull<u8>,
    data_len: usize,
}

impl NetBufHandle {
    /// Create a new [`NetBufHandle`].
    pub fn new(owner_ptr: NonNull<u8>, data_ptr: NonNull<u8>, data_len: usize) -> Self {
        Self {
            owner_ptr,
            data_ptr,
            data_len,
        }
    }

    /// Return raw pointer of the owner object.
    pub fn owner_ptr<T>(&self) -> *mut T {
        self.owner_ptr.as_ptr() as *mut T
    }

    /// Return the payload length.
    pub fn len(&self) -> usize {
        self.data_len
    }

    /// Returns true if the payload is empty.
    pub fn is_empty(&self) -> bool {
        self.data_len == 0
    }

    /// Return the payload as `&[u8]`.
    pub fn data(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.data_ptr.as_ptr() as *const u8, self.data_len) }
    }

    /// Return the payload as `&mut [u8]`.
    pub fn data_mut(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.data_ptr.as_ptr(), self.data_len) }
    }
}

const MIN_BUFFER_LEN: usize = 1526;
const MAX_BUFFER_LEN: usize = 65535;

/// A RAII network buffer wrapped in a [`Box`].
pub type NetBufBox = Box<NetBuf>;

/// A RAII network buffer.
///
/// It should be allocated from the [`NetBufPool`], and it will be
/// deallocated into the pool automatically when dropped.
///
/// The layout of the buffer is:
///
/// ```text
///   ______________________ capacity ______________________
///  /                                                      \
/// +------------------+------------------+------------------+
/// |      Header      |      Packet      |      Unused      |
/// +------------------+------------------+------------------+
/// |\__ hdr_len __/ \__ payload_len __/
/// |
/// buf_ptr
/// ```
pub struct NetBuf {
    hdr_len: usize,
    payload_len: usize,
    buf_len: usize,
    base_ptr: NonNull<u8>,
    pool_offset: usize,
    pool: Arc<NetBufPool>,
}

unsafe impl Send for NetBuf {}
unsafe impl Sync for NetBuf {}

impl NetBuf {
    const unsafe fn get_slice(&self, start: usize, len: usize) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.base_ptr.as_ptr().add(start), len) }
    }

    const unsafe fn get_slice_mut(&mut self, start: usize, len: usize) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.base_ptr.as_ptr().add(start), len) }
    }

    /// Returns the capacity of the buffer.
    pub const fn capacity(&self) -> usize {
        self.buf_len
    }

    /// Returns the length of the header part.
    pub const fn hdr_len(&self) -> usize {
        self.hdr_len
    }

    /// Returns the length of the payload part.
    pub const fn payload_len(&self) -> usize {
        self.payload_len
    }

    /// Returns the header part of the buffer.
    pub const fn header(&self) -> &[u8] {
        unsafe { self.get_slice(0, self.hdr_len) }
    }

    /// Returns the payload part of the buffer.
    pub const fn payload(&self) -> &[u8] {
        unsafe { self.get_slice(self.hdr_len, self.payload_len) }
    }

    /// Returns the mutable reference to the payload part.
    pub const fn payload_mut(&mut self) -> &mut [u8] {
        unsafe { self.get_slice_mut(self.hdr_len, self.payload_len) }
    }

    /// Returns the full frame (header + payload) as a contiguous slice.
    pub const fn frame(&self) -> &[u8] {
        unsafe { self.get_slice(0, self.hdr_len + self.payload_len) }
    }

    /// Returns the entire buffer.
    pub const fn buffer(&self) -> &[u8] {
        unsafe { self.get_slice(0, self.buf_len) }
    }

    /// Returns the mutable reference to the entire buffer.
    pub const fn buffer_mut(&mut self) -> &mut [u8] {
        unsafe { self.get_slice_mut(0, self.buf_len) }
    }

    /// Set the length of the header part.
    pub fn set_hdr_len(&mut self, hdr_len: usize) {
        debug_assert!(hdr_len + self.payload_len <= self.buf_len);
        self.hdr_len = hdr_len;
    }

    /// Set the length of the payload part.
    pub fn set_payload_len(&mut self, payload_len: usize) {
        debug_assert!(self.hdr_len + payload_len <= self.buf_len);
        self.payload_len = payload_len;
    }

    /// Converts the buffer into a [`NetBufHandle`].
    pub fn into_handle(mut self: Box<Self>) -> NetBufHandle {
        let data_ptr = self.payload_mut().as_mut_ptr();
        let data_len = self.payload_len;
        NetBufHandle::new(
            NonNull::new(Box::into_raw(self) as *mut u8).unwrap(),
            NonNull::new(data_ptr).unwrap(),
            data_len,
        )
    }

    /// Restore [`NetBuf`] from a handle.
    ///
    /// # Safety
    ///
    /// This function is unsafe because it may cause some memory issues,
    /// so we must ensure that it is called after calling `into_handle`.
    pub unsafe fn from_handle(handle: NetBufHandle) -> Box<Self> {
        unsafe { Box::from_raw(handle.owner_ptr::<Self>()) }
    }
}

impl Drop for NetBuf {
    /// Deallocates the buffer into the [`NetBufPool`].
    fn drop(&mut self) {
        self.pool.release_offset(self.pool_offset);
    }
}

/// A pool of [`NetBuf`]s to speed up buffer allocation.
///
/// It divides a large memory into several equal parts for each buffer.
pub struct NetBufPool {
    slot_count: usize,
    buf_len: usize,
    storage: Vec<u8>,
    free_offsets: Mutex<Vec<usize>>,
}

impl NetBufPool {
    /// Creates a new pool with the given `slot_count`, and all buffer lengths are
    /// set to `buf_len`.
    pub fn new(slot_count: usize, buf_len: usize) -> DriverResult<Arc<Self>> {
        if slot_count == 0 {
            return Err(DriverError::InvalidInput);
        }
        if !(MIN_BUFFER_LEN..=MAX_BUFFER_LEN).contains(&buf_len) {
            return Err(DriverError::InvalidInput);
        }

        let storage = vec![0; slot_count * buf_len];
        let mut free_offsets = Vec::with_capacity(slot_count);
        for i in 0..slot_count {
            free_offsets.push(i * buf_len);
        }
        Ok(Arc::new(Self {
            slot_count,
            buf_len,
            storage,
            free_offsets: Mutex::new(free_offsets),
        }))
    }

    /// Returns the capacity of the pool.
    pub const fn capacity(&self) -> usize {
        self.slot_count
    }

    /// Returns the length of each buffer.
    pub const fn buffer_len(&self) -> usize {
        self.buf_len
    }

    /// Allocates a buffer from the pool.
    ///
    /// Returns `None` if no buffer is available.
    pub fn alloc_buf(self: &Arc<Self>) -> Option<NetBuf> {
        let pool_offset = self.free_offsets.lock().pop()?;
        let buf_ptr =
            unsafe { NonNull::new(self.storage.as_ptr().add(pool_offset) as *mut u8).unwrap() };
        Some(NetBuf {
            hdr_len: 0,
            payload_len: 0,
            buf_len: self.buf_len,
            base_ptr: buf_ptr,
            pool_offset,
            pool: Arc::clone(self),
        })
    }

    /// Allocates a buffer wrapped in a [`Box`] from the pool.
    ///
    /// Returns `None` if no buffer is available.
    pub fn alloc_boxed(self: &Arc<Self>) -> Option<NetBufBox> {
        Some(Box::new(self.alloc_buf()?))
    }

    /// Deallocates a buffer at the given offset.
    ///
    /// `pool_offset` must be a multiple of `buf_len`.
    fn release_offset(&self, pool_offset: usize) {
        debug_assert_eq!(pool_offset % self.buf_len, 0);
        self.free_offsets.lock().push(pool_offset);
    }
}

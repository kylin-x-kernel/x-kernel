//! Driver for VirtIO block devices.

use crate::config::{ReadOnly, read_config};
use crate::hal::Hal;
use crate::queue::VirtQueue;
use crate::transport::{InterruptStatus, Transport};
use crate::{Error, Result};
use bitflags::bitflags;
use log::info;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

const QUEUE: u16 = 0;
const QUEUE_SIZE: u16 = 16;
/// Maximum data descriptors after reserving the header and status descriptors.
pub const MAX_DATA_SEGMENTS: usize = QUEUE_SIZE as usize - 2;
const SUPPORTED_FEATURES: BlkFeature = BlkFeature::RO
    .union(BlkFeature::SEG_MAX)
    .union(BlkFeature::SIZE_MAX)
    .union(BlkFeature::FLUSH)
    .union(BlkFeature::RING_INDIRECT_DESC)
    .union(BlkFeature::RING_EVENT_IDX)
    .union(BlkFeature::VERSION_1)
    .union(BlkFeature::ACCESS_PLATFORM);

/// Driver for a VirtIO block device.
///
/// This is a simple virtual block device, e.g. disk.
///
/// Read and write requests (and other exotic requests) are placed in the queue and serviced
/// (probably out of order) by the device except where noted.
///
/// # Example
///
/// ```
/// # use kvirtiodrivers::{Error, Hal};
/// # use kvirtiodrivers::transport::Transport;
/// use kvirtiodrivers::device::blk::{VirtIOBlk, SECTOR_SIZE};
///
/// # fn example<HalImpl: Hal, T: Transport>(transport: T) -> Result<(), Error> {
/// let mut disk = VirtIOBlk::<HalImpl, _>::new(transport)?;
///
/// println!("VirtIO block device: {} kB", disk.capacity() * SECTOR_SIZE as u64 / 2);
///
/// // Read sector 0 and then copy it to sector 1.
/// let mut buf = [0; SECTOR_SIZE];
/// disk.read_blocks(0, &mut buf)?;
/// disk.write_blocks(1, &buf)?;
/// # Ok(())
/// # }
/// ```
pub struct VirtIOBlk<H: Hal, T: Transport> {
    transport: T,
    queue: VirtQueue<H, { QUEUE_SIZE as usize }>,
    capacity: u64,
    negotiated_features: BlkFeature,
    max_segments: usize,
    max_segment_size: usize,
}

impl<H: Hal, T: Transport> VirtIOBlk<H, T> {
    /// Create a new VirtIO-Blk driver.
    pub fn new(mut transport: T) -> Result<Self> {
        let negotiated_features = transport.begin_init(SUPPORTED_FEATURES);

        // Read configuration space.
        let capacity = transport.read_consistent(|| {
            Ok((read_config!(transport, BlkConfig, capacity_low)? as u64)
                | ((read_config!(transport, BlkConfig, capacity_high)? as u64) << 32))
        })?;
        info!("found a block device of size {}KB", capacity / 2);

        let max_segments = if negotiated_features.contains(BlkFeature::SEG_MAX) {
            read_config!(transport, BlkConfig, seg_max)? as usize
        } else {
            1
        }
        .clamp(1, MAX_DATA_SEGMENTS);
        let max_segment_size = if negotiated_features.contains(BlkFeature::SIZE_MAX) {
            read_config!(transport, BlkConfig, size_max)? as usize
        } else {
            u32::MAX as usize
        };
        if max_segment_size == 0 {
            return Err(Error::InvalidParam);
        }
        let queue = VirtQueue::new(
            &mut transport,
            QUEUE,
            negotiated_features.contains(BlkFeature::RING_INDIRECT_DESC),
            negotiated_features.contains(BlkFeature::RING_EVENT_IDX),
            negotiated_features.contains(BlkFeature::ACCESS_PLATFORM),
        )?;
        transport.finish_init();

        Ok(VirtIOBlk {
            transport,
            queue,
            capacity,
            negotiated_features,
            max_segments,
            max_segment_size,
        })
    }

    /// Gets the capacity of the block device, in 512 byte ([`SECTOR_SIZE`]) sectors.
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Returns true if the block device is read-only, or false if it allows writes.
    pub fn readonly(&self) -> bool {
        self.negotiated_features.contains(BlkFeature::RO)
    }

    /// Acknowledges a pending interrupt, if any.
    ///
    /// Returns true if there was an interrupt to acknowledge.
    pub fn ack_interrupt(&mut self) -> InterruptStatus {
        self.transport.ack_interrupt()
    }

    /// Enables interrupts from the device.
    pub fn enable_interrupts(&mut self) {
        self.queue.set_dev_notify(true);
    }

    /// Disables interrupts from the device.
    pub fn disable_interrupts(&mut self) {
        self.queue.set_dev_notify(false);
    }

    /// Sends the given request to the device and waits for a response, with no extra data.
    fn request(&mut self, request: BlkReq) -> Result {
        let mut resp = BlkResp::default();
        self.queue.add_notify_wait_pop(
            &[request.as_bytes()],
            &mut [resp.as_mut_bytes()],
            &mut self.transport,
        )?;
        resp.status.into()
    }

    /// Sends the given request to the device and waits for a response, including the given data.
    fn request_read(&mut self, request: BlkReq, data: &mut [u8]) -> Result {
        let mut resp = BlkResp::default();
        self.queue.add_notify_wait_pop(
            &[request.as_bytes()],
            &mut [data, resp.as_mut_bytes()],
            &mut self.transport,
        )?;
        resp.status.into()
    }

    /// Sends the given request and data to the device and waits for a response.
    fn request_write(&mut self, request: BlkReq, data: &[u8]) -> Result {
        let mut resp = BlkResp::default();
        self.queue.add_notify_wait_pop(
            &[request.as_bytes(), data],
            &mut [resp.as_mut_bytes()],
            &mut self.transport,
        )?;
        resp.status.into()
    }

    /// Requests the device to flush any pending writes to storage.
    ///
    /// This will be ignored if the device doesn't support the `VIRTIO_BLK_F_FLUSH` feature.
    pub fn flush(&mut self) -> Result {
        if self.negotiated_features.contains(BlkFeature::FLUSH) {
            self.request(BlkReq {
                type_: ReqType::Flush,
                ..Default::default()
            })
        } else {
            Ok(())
        }
    }

    /// Gets the device ID.
    ///
    /// The ID is written as ASCII into the given buffer, which must be 20 bytes long, and the used
    /// length returned.
    pub fn device_id(&mut self, id: &mut [u8; 20]) -> Result<usize> {
        self.request_read(
            BlkReq {
                type_: ReqType::GetId,
                ..Default::default()
            },
            id,
        )?;

        let length = id.iter().position(|&x| x == 0).unwrap_or(20);
        Ok(length)
    }

    /// Reads one or more blocks into the given buffer.
    ///
    /// The buffer length must be a non-zero multiple of [`SECTOR_SIZE`].
    ///
    /// Blocks until the read completes or there is an error.
    pub fn read_blocks(&mut self, block_id: usize, buf: &mut [u8]) -> Result {
        self.validate_segments(core::iter::once(buf.len()))?;
        assert_ne!(buf.len(), 0);
        assert_eq!(buf.len() % SECTOR_SIZE, 0);
        self.request_read(
            BlkReq {
                type_: ReqType::In,
                reserved: 0,
                sector: block_id as u64,
            },
            buf,
        )
    }

    /// Submits a request to read one or more blocks, but returns immediately without waiting for
    /// the read to complete.
    ///
    /// # Arguments
    ///
    /// * `block_id` - The identifier of the first block to read.
    /// * `req` - A buffer which the driver can use for the request to send to the device. The
    ///   contents don't matter as `read_blocks_nb` will initialise it, but like the other buffers
    ///   it needs to be valid (and not otherwise used) until the corresponding
    ///   `complete_read_blocks` call. Its length must be a non-zero multiple of [`SECTOR_SIZE`].
    /// * `buf` - The buffer in memory into which the block should be read.
    /// * `resp` - A mutable reference to a variable provided by the caller
    ///   to contain the status of the request. The caller can safely
    ///   read the variable only after the request is complete.
    ///
    /// # Usage
    ///
    /// It will submit request to the VirtIO block device and return a token identifying
    /// the position of the first Descriptor in the chain. If there are not enough
    /// Descriptors to allocate, then it returns [`Error::QueueFull`].
    ///
    /// The caller can then call `peek_used` with the returned token to check whether the device has
    /// finished handling the request. Once it has, the caller must call `complete_read_blocks` with
    /// the same buffers before reading the response.
    ///
    /// ```
    /// # use kvirtiodrivers::{Error, Hal};
    /// # use kvirtiodrivers::device::blk::VirtIOBlk;
    /// # use kvirtiodrivers::transport::Transport;
    /// use kvirtiodrivers::device::blk::{BlkReq, BlkResp, RespStatus};
    ///
    /// # fn example<H: Hal, T: Transport>(blk: &mut VirtIOBlk<H, T>) -> Result<(), Error> {
    /// let mut request = BlkReq::default();
    /// let mut buffer = [0; 512];
    /// let mut response = BlkResp::default();
    /// let token = unsafe { blk.read_blocks_nb(42, &mut request, &mut buffer, &mut response) }?;
    ///
    /// // Wait for an interrupt to tell us that the request completed...
    /// assert_eq!(blk.peek_used(), Some(token));
    ///
    /// unsafe {
    ///   blk.complete_read_blocks(token, &request, &mut buffer, &mut response)?;
    /// }
    /// if response.status() == RespStatus::OK {
    ///   println!("Successfully read block.");
    /// } else {
    ///   println!("Error {:?} reading block.", response.status());
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Safety
    ///
    /// `req`, `buf` and `resp` are still borrowed by the underlying VirtIO block device even after
    /// this method returns. Thus, it is the caller's responsibility to guarantee that they are not
    /// accessed before the request is completed in order to avoid data races.
    pub unsafe fn read_blocks_nb(
        &mut self,
        block_id: usize,
        req: &mut BlkReq,
        buf: &mut [u8],
        resp: &mut BlkResp,
    ) -> Result<u16> {
        // SAFETY: the single segment has exactly the lifetime promised by the caller.
        unsafe { self.read_blocks_sg_nb(block_id, req, &mut [buf], resp) }
    }

    /// Completes a read operation which was started by `read_blocks_nb`.
    ///
    /// # Safety
    ///
    /// The same buffers (`req`, `buf` and `resp`) must be passed in again as were passed to
    /// `read_blocks_nb` when it returned the token.
    pub unsafe fn complete_read_blocks(
        &mut self,
        token: u16,
        req: &BlkReq,
        buf: &mut [u8],
        resp: &mut BlkResp,
    ) -> Result<()> {
        // SAFETY: this is the original segment for the supplied token.
        unsafe { self.complete_read_blocks_sg(token, req, &mut [buf], resp) }
    }

    /// Writes the contents of the given buffer to a block or blocks.
    ///
    /// The buffer length must be a non-zero multiple of [`SECTOR_SIZE`].
    ///
    /// Blocks until the write is complete or there is an error.
    pub fn write_blocks(&mut self, block_id: usize, buf: &[u8]) -> Result {
        self.validate_segments(core::iter::once(buf.len()))?;
        assert_ne!(buf.len(), 0);
        assert_eq!(buf.len() % SECTOR_SIZE, 0);
        self.request_write(
            BlkReq {
                type_: ReqType::Out,
                sector: block_id as u64,
                ..Default::default()
            },
            buf,
        )
    }

    /// Submits a request to write one or more blocks, but returns immediately without waiting for
    /// the write to complete.
    ///
    /// # Arguments
    ///
    /// * `block_id` - The identifier of the first block to write.
    /// * `req` - A buffer which the driver can use for the request to send to the device. The
    ///   contents don't matter as `read_blocks_nb` will initialise it, but like the other buffers
    ///   it needs to be valid (and not otherwise used) until the corresponding
    ///   `complete_write_blocks` call.
    /// * `buf` - The buffer in memory containing the data to write to the blocks. Its length must
    ///   be a non-zero multiple of [`SECTOR_SIZE`].
    /// * `resp` - A mutable reference to a variable provided by the caller
    ///   to contain the status of the request. The caller can safely
    ///   read the variable only after the request is complete.
    ///
    /// # Usage
    ///
    /// See [VirtIOBlk::read_blocks_nb].
    ///
    /// # Safety
    ///
    /// See  [VirtIOBlk::read_blocks_nb].
    pub unsafe fn write_blocks_nb(
        &mut self,
        block_id: usize,
        req: &mut BlkReq,
        buf: &[u8],
        resp: &mut BlkResp,
    ) -> Result<u16> {
        // SAFETY: the single segment has exactly the lifetime promised by the caller.
        unsafe { self.write_blocks_sg_nb(block_id, req, &[buf], resp) }
    }

    /// Completes a write operation which was started by `write_blocks_nb`.
    ///
    /// # Safety
    ///
    /// The same buffers (`req`, `buf` and `resp`) must be passed in again as were passed to
    /// `write_blocks_nb` when it returned the token.
    pub unsafe fn complete_write_blocks(
        &mut self,
        token: u16,
        req: &BlkReq,
        buf: &[u8],
        resp: &mut BlkResp,
    ) -> Result<()> {
        // SAFETY: this is the original segment for the supplied token.
        unsafe { self.complete_write_blocks_sg(token, req, &[buf], resp) }
    }

    /// Maximum data segments in one descriptor chain, including the virtqueue limit.
    pub fn max_segments(&self) -> usize {
        self.max_segments
    }

    /// Maximum bytes in each data segment, as negotiated with the device.
    ///
    /// This may be smaller than a sector; sector alignment applies to the
    /// combined length of a read or write request.
    pub fn max_segment_size(&self) -> usize {
        self.max_segment_size
    }

    fn validate_segments(&self, lengths: impl ExactSizeIterator<Item = usize>) -> Result {
        if lengths.len() == 0 || lengths.len() > self.max_segments {
            return Err(Error::InvalidParam);
        }
        let mut total = 0usize;
        for len in lengths {
            if len == 0 || len > self.max_segment_size {
                return Err(Error::InvalidParam);
            }
            total = total.checked_add(len).ok_or(Error::InvalidParam)?;
        }
        if !total.is_multiple_of(SECTOR_SIZE) {
            return Err(Error::InvalidParam);
        }
        Ok(())
    }

    /// Submits a scatter read without copying payloads. Only the combined length
    /// must be sector-aligned; each segment must satisfy the advertised limits.
    /// Invalid input publishes no descriptors. QueueFull may be retried unchanged.
    ///
    /// # Safety
    /// All payloads, `req` and `resp` must remain live, stable and inaccessible to
    /// the CPU until `complete_read_blocks_sg` retires the returned token. The
    /// temporary slice of references itself need not survive this call.
    pub unsafe fn read_blocks_sg_nb(
        &mut self,
        block_id: usize,
        req: &mut BlkReq,
        buffers: &mut [&mut [u8]],
        resp: &mut BlkResp,
    ) -> Result<u16> {
        self.validate_segments(buffers.iter().map(|buf| buf.len()))?;
        *req = BlkReq {
            type_: ReqType::In,
            reserved: 0,
            sector: block_id as u64,
        };
        let mut outputs: [&mut [u8]; MAX_DATA_SEGMENTS + 1] = core::array::from_fn(|_| &mut [][..]);
        let count = buffers.len();
        for (output, buffer) in outputs.iter_mut().zip(buffers.iter_mut()) {
            *output = buffer;
        }
        outputs[count] = resp.as_mut_bytes();
        // SAFETY: the caller retains all disjoint buffers until completion.
        let token = unsafe {
            self.queue
                .add(&[req.as_bytes()], &mut outputs[..count + 1])?
        };
        if self.queue.should_notify() {
            self.transport.notify(QUEUE);
        }
        Ok(token)
    }

    /// Retires a scatter read and returns the device status after unmapping.
    ///
    /// # Safety
    /// Pass the same ordered segments and protocol storage used to submit token.
    /// The token must have been reported by `peek_used` and not yet reclaimed.
    pub unsafe fn complete_read_blocks_sg(
        &mut self,
        token: u16,
        req: &BlkReq,
        buffers: &mut [&mut [u8]],
        resp: &mut BlkResp,
    ) -> Result {
        let mut outputs: [&mut [u8]; MAX_DATA_SEGMENTS + 1] = core::array::from_fn(|_| &mut [][..]);
        let count = buffers.len();
        for (output, buffer) in outputs.iter_mut().zip(buffers.iter_mut()) {
            *output = buffer;
        }
        outputs[count] = resp.as_mut_bytes();
        // SAFETY: the caller supplies token's exact original storage after completion.
        unsafe {
            self.queue
                .pop_used(token, &[req.as_bytes()], &mut outputs[..count + 1])?;
        }
        resp.status.into()
    }

    /// Submits a scatter write without copying payloads. Segment limits and
    /// lifetime requirements are the same as `read_blocks_sg_nb`.
    ///
    /// # Safety
    /// Keep all payloads and protocol storage stable and inaccessible until
    /// `complete_write_blocks_sg` retires the returned token.
    pub unsafe fn write_blocks_sg_nb(
        &mut self,
        block_id: usize,
        req: &mut BlkReq,
        buffers: &[&[u8]],
        resp: &mut BlkResp,
    ) -> Result<u16> {
        self.validate_segments(buffers.iter().map(|buf| buf.len()))?;
        *req = BlkReq {
            type_: ReqType::Out,
            reserved: 0,
            sector: block_id as u64,
        };
        let mut inputs = [&[][..]; MAX_DATA_SEGMENTS + 1];
        inputs[0] = req.as_bytes();
        inputs[1..buffers.len() + 1].copy_from_slice(buffers);
        // SAFETY: the caller retains all disjoint buffers until completion.
        let token = unsafe {
            self.queue
                .add(&inputs[..buffers.len() + 1], &mut [resp.as_mut_bytes()])?
        };
        if self.queue.should_notify() {
            self.transport.notify(QUEUE);
        }
        Ok(token)
    }

    /// Retires a scatter write and returns the device status after unmapping.
    ///
    /// # Safety
    /// Pass the same ordered segments and protocol storage used to submit token.
    /// The token must have been reported by `peek_used` and not yet reclaimed.
    pub unsafe fn complete_write_blocks_sg(
        &mut self,
        token: u16,
        req: &BlkReq,
        buffers: &[&[u8]],
        resp: &mut BlkResp,
    ) -> Result {
        let mut inputs = [&[][..]; MAX_DATA_SEGMENTS + 1];
        inputs[0] = req.as_bytes();
        inputs[1..buffers.len() + 1].copy_from_slice(buffers);
        // SAFETY: the caller supplies token's exact original storage after completion.
        unsafe {
            self.queue.pop_used(
                token,
                &inputs[..buffers.len() + 1],
                &mut [resp.as_mut_bytes()],
            )?;
        }
        resp.status.into()
    }

    /// Submits a flush request, but returns immediately without waiting for it to complete.
    ///
    /// This is the non-blocking counterpart of [`Self::flush`]. The caller must
    /// keep `req` and `resp` valid (and not otherwise accessed) until the
    /// corresponding [`Self::complete_flush`] call succeeds.
    ///
    /// Returns `Ok(())` immediately (without notifying the device) if the
    /// `VIRTIO_BLK_F_FLUSH` feature was not negotiated — the flush is a no-op
    /// in that case and there is no token to complete.
    ///
    /// # Safety
    ///
    /// `req` and `resp` are still borrowed by the underlying VirtIO block
    /// device even after this method returns. The caller must not access them
    /// before the request is completed (via [`Self::complete_flush`]) to avoid
    /// data races.
    pub unsafe fn flush_nb(&mut self, req: &mut BlkReq, resp: &mut BlkResp) -> Result<Option<u16>> {
        if !self.negotiated_features.contains(BlkFeature::FLUSH) {
            return Ok(None);
        }
        *req = BlkReq {
            type_: ReqType::Flush,
            ..Default::default()
        };
        // SAFETY: The caller promises that `req` and `resp` are not accessed
        // before the request is completed.
        let token = unsafe {
            self.queue
                .add(&[req.as_bytes()], &mut [resp.as_mut_bytes()])?
        };
        if self.queue.should_notify() {
            self.transport.notify(QUEUE);
        }
        Ok(Some(token))
    }

    /// Completes a flush operation started by [`Self::flush_nb`].
    ///
    /// `token` must be the value returned by the corresponding `flush_nb` call.
    ///
    /// # Safety
    ///
    /// The same `req` and `resp` must be passed in again as were passed to
    /// `flush_nb` when it returned the token.
    pub unsafe fn complete_flush(
        &mut self,
        token: u16,
        req: &BlkReq,
        resp: &mut BlkResp,
    ) -> Result<()> {
        // SAFETY: The caller promises that `req` and `resp` are the same that
        // were passed to the corresponding `flush_nb` call.
        unsafe {
            self.queue
                .pop_used(token, &[req.as_bytes()], &mut [resp.as_mut_bytes()])?;
        }
        resp.status.into()
    }

    /// Fetches the token of the next completed request from the used ring and returns it, without
    /// removing it from the used ring. If there are no pending completed requests returns `None`.
    pub fn peek_used(&mut self) -> Option<u16> {
        self.queue.peek_used()
    }

    /// Returns the size of the device's VirtQueue.
    ///
    /// This can be used to tell the caller how many channels to monitor on.
    pub fn virt_queue_size(&self) -> u16 {
        QUEUE_SIZE
    }
}

impl<H: Hal, T: Transport> Drop for VirtIOBlk<H, T> {
    fn drop(&mut self) {
        // Clear any pointers pointing to DMA regions, so the device doesn't try to access them
        // after they have been freed.
        self.transport.queue_unset(QUEUE);
    }
}

#[derive(FromBytes, Immutable, IntoBytes)]
#[repr(C)]
struct BlkConfig {
    /// Number of 512 Bytes sectors
    capacity_low: ReadOnly<u32>,
    capacity_high: ReadOnly<u32>,
    size_max: ReadOnly<u32>,
    seg_max: ReadOnly<u32>,
    cylinders: ReadOnly<u16>,
    heads: ReadOnly<u8>,
    sectors: ReadOnly<u8>,
    blk_size: ReadOnly<u32>,
    physical_block_exp: ReadOnly<u8>,
    alignment_offset: ReadOnly<u8>,
    min_io_size: ReadOnly<u16>,
    opt_io_size: ReadOnly<u32>,
    // ... ignored
}

/// A VirtIO block device request.
#[repr(C)]
#[derive(Debug, Immutable, IntoBytes, KnownLayout)]
pub struct BlkReq {
    type_: ReqType,
    reserved: u32,
    sector: u64,
}

impl Default for BlkReq {
    fn default() -> Self {
        Self {
            type_: ReqType::In,
            reserved: 0,
            sector: 0,
        }
    }
}

/// Response of a VirtIOBlk request.
#[repr(C)]
#[derive(Debug, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct BlkResp {
    status: RespStatus,
}

impl BlkResp {
    /// Return the status of a VirtIOBlk request.
    pub fn status(&self) -> RespStatus {
        self.status
    }
}

#[repr(u32)]
#[derive(Debug, Immutable, IntoBytes, KnownLayout)]
enum ReqType {
    In = 0,
    Out = 1,
    Flush = 4,
    GetId = 8,
    GetLifetime = 10,
    Discard = 11,
    WriteZeroes = 13,
    SecureErase = 14,
}

/// Status of a VirtIOBlk request.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, FromBytes, Immutable, IntoBytes, KnownLayout, PartialEq)]
pub struct RespStatus(u8);

impl RespStatus {
    /// Ok.
    pub const OK: RespStatus = RespStatus(0);
    /// IoErr.
    pub const IO_ERR: RespStatus = RespStatus(1);
    /// Unsupported yet.
    pub const UNSUPPORTED: RespStatus = RespStatus(2);
    /// Not ready.
    pub const NOT_READY: RespStatus = RespStatus(3);
}

impl From<RespStatus> for Result {
    fn from(status: RespStatus) -> Self {
        match status {
            RespStatus::OK => Ok(()),
            RespStatus::IO_ERR => Err(Error::IoError),
            RespStatus::UNSUPPORTED => Err(Error::Unsupported),
            RespStatus::NOT_READY => Err(Error::NotReady),
            _ => Err(Error::IoError),
        }
    }
}

impl Default for BlkResp {
    fn default() -> Self {
        BlkResp {
            status: RespStatus::NOT_READY,
        }
    }
}

/// The standard sector size of a VirtIO block device. Data is read and written in multiples of this
/// size.
pub const SECTOR_SIZE: usize = 512;

bitflags! {
    #[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
    struct BlkFeature: u64 {
        /// Device supports request barriers. (legacy)
        const BARRIER       = 1 << 0;
        /// Maximum size of any single segment is in `size_max`.
        const SIZE_MAX      = 1 << 1;
        /// Maximum number of segments in a request is in `seg_max`.
        const SEG_MAX       = 1 << 2;
        /// Disk-style geometry specified in geometry.
        const GEOMETRY      = 1 << 4;
        /// Device is read-only.
        const RO            = 1 << 5;
        /// Block size of disk is in `blk_size`.
        const BLK_SIZE      = 1 << 6;
        /// Device supports scsi packet commands. (legacy)
        const SCSI          = 1 << 7;
        /// Cache flush command support.
        const FLUSH         = 1 << 9;
        /// Device exports information on optimal I/O alignment.
        const TOPOLOGY      = 1 << 10;
        /// Device can toggle its cache between writeback and writethrough modes.
        const CONFIG_WCE    = 1 << 11;
        /// Device supports multiqueue.
        const MQ            = 1 << 12;
        /// Device can support discard command, maximum discard sectors size in
        /// `max_discard_sectors` and maximum discard segment number in
        /// `max_discard_seg`.
        const DISCARD       = 1 << 13;
        /// Device can support write zeroes command, maximum write zeroes sectors
        /// size in `max_write_zeroes_sectors` and maximum write zeroes segment
        /// number in `max_write_zeroes_seg`.
        const WRITE_ZEROES  = 1 << 14;
        /// Device supports providing storage lifetime information.
        const LIFETIME      = 1 << 15;
        /// Device can support the secure erase command.
        const SECURE_ERASE  = 1 << 16;

        // device independent
        const NOTIFY_ON_EMPTY       = 1 << 24; // legacy
        const ANY_LAYOUT            = 1 << 27; // legacy
        const RING_INDIRECT_DESC    = 1 << 28;
        const RING_EVENT_IDX        = 1 << 29;
        const UNUSED                = 1 << 30; // legacy
        const VERSION_1             = 1 << 32; // detect legacy

        // the following since virtio v1.1
        const ACCESS_PLATFORM       = 1 << 33;
        const RING_PACKED           = 1 << 34;
        const IN_ORDER              = 1 << 35;
        const ORDER_PLATFORM        = 1 << 36;
        const SR_IOV                = 1 << 37;
        const NOTIFICATION_DATA     = 1 << 38;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        hal::fake::FakeHal,
        transport::{
            DeviceType,
            fake::{FakeTransport, QueueStatus, State},
        },
    };
    use alloc::{sync::Arc, vec};
    use core::mem::size_of;
    use std::{sync::Mutex, thread};

    #[test]
    fn scatter_read_write_limits_and_error_retirement() {
        for indirect in [BlkFeature::empty(), BlkFeature::RING_INDIRECT_DESC] {
            let config = BlkConfig {
                capacity_low: ReadOnly::new(66),
                capacity_high: ReadOnly::new(0),
                size_max: ReadOnly::new(512),
                seg_max: ReadOnly::new(2),
                cylinders: ReadOnly::new(0),
                heads: ReadOnly::new(0),
                sectors: ReadOnly::new(0),
                blk_size: ReadOnly::new(0),
                physical_block_exp: ReadOnly::new(0),
                alignment_offset: ReadOnly::new(0),
                min_io_size: ReadOnly::new(0),
                opt_io_size: ReadOnly::new(0),
            };
            let state = Arc::new(Mutex::new(State::new(vec![QueueStatus::default()], config)));
            let transport = FakeTransport {
                device_type: DeviceType::Block,
                max_queue_size: QUEUE_SIZE.into(),
                device_features: (indirect | BlkFeature::SEG_MAX | BlkFeature::SIZE_MAX).bits(),
                state: state.clone(),
            };
            let mut blk = VirtIOBlk::<FakeHal, _>::new(transport).unwrap();
            assert_eq!(blk.max_segments(), 2);
            assert_eq!(blk.max_segment_size(), 512);
            let mut req = BlkReq::default();
            let mut resp = BlkResp::default();
            let mut oversized = [0; 1024];
            // SAFETY: validation fails before publication; all storage remains live.
            assert_eq!(
                // SAFETY: validation fails before publishing these live buffers.
                unsafe { blk.read_blocks_sg_nb(0, &mut req, &mut [&mut oversized], &mut resp) },
                Err(Error::InvalidParam)
            );
            assert_eq!(blk.peek_used(), None);
            for is_read in [true, false] {
                for status in [0, 1] {
                    let mut first = [0x37; 17];
                    let mut second = [0x37; 495];
                    let token = if is_read {
                        // SAFETY: disjoint stable local arrays survive until retirement below.
                        unsafe {
                            blk.read_blocks_sg_nb(
                                7,
                                &mut req,
                                &mut [&mut first, &mut second],
                                &mut resp,
                            )
                        }
                        .unwrap()
                    } else {
                        // SAFETY: stable initialized sources and protocol storage survive retirement.
                        unsafe {
                            blk.write_blocks_sg_nb(7, &mut req, &[&first, &second], &mut resp)
                        }
                        .unwrap()
                    };
                    assert!(
                        state
                            .lock()
                            .unwrap()
                            .read_write_queue::<{ QUEUE_SIZE as usize }>(QUEUE, |request| {
                                assert_eq!(
                                    &request[..16],
                                    BlkReq {
                                        type_: if is_read { ReqType::In } else { ReqType::Out },
                                        reserved: 0,
                                        sector: 7
                                    }
                                    .as_bytes()
                                );
                                let mut response = if is_read {
                                    vec![0xa5; 512]
                                } else {
                                    assert_eq!(&request[16..], &[0x37; 512]);
                                    Vec::new()
                                };
                                response.push(status);
                                response
                            })
                    );
                    assert_eq!(blk.peek_used(), Some(token));
                    let result = if is_read {
                        // SAFETY: the device reported token; these are its exact original buffers.
                        unsafe {
                            blk.complete_read_blocks_sg(
                                token,
                                &req,
                                &mut [&mut first, &mut second],
                                &mut resp,
                            )
                        }
                    } else {
                        // SAFETY: the device reported token; these are its exact original buffers.
                        unsafe {
                            blk.complete_write_blocks_sg(token, &req, &[&first, &second], &mut resp)
                        }
                    };
                    assert_eq!(
                        result,
                        if status == 0 {
                            Ok(())
                        } else {
                            Err(Error::IoError)
                        }
                    );
                    assert_eq!(blk.peek_used(), None);
                    if is_read {
                        assert_eq!(first, [0xa5; 17]);
                        assert_eq!(second, [0xa5; 495]);
                    }
                }
            }
        }
    }

    #[test]
    fn config() {
        let config_space = BlkConfig {
            capacity_low: ReadOnly::new(0x42),
            capacity_high: ReadOnly::new(0x02),
            size_max: ReadOnly::new(0),
            seg_max: ReadOnly::new(0),
            cylinders: ReadOnly::new(0),
            heads: ReadOnly::new(0),
            sectors: ReadOnly::new(0),
            blk_size: ReadOnly::new(0),
            physical_block_exp: ReadOnly::new(0),
            alignment_offset: ReadOnly::new(0),
            min_io_size: ReadOnly::new(0),
            opt_io_size: ReadOnly::new(0),
        };
        let state = Arc::new(Mutex::new(State::new(
            vec![QueueStatus::default()],
            config_space,
        )));
        let transport = FakeTransport {
            device_type: DeviceType::Block,
            max_queue_size: QUEUE_SIZE.into(),
            device_features: BlkFeature::RO.bits(),
            state: state.clone(),
        };
        let blk = VirtIOBlk::<FakeHal, FakeTransport<BlkConfig>>::new(transport).unwrap();

        assert_eq!(blk.capacity(), 0x02_0000_0042);
        assert_eq!(blk.readonly(), true);
    }

    #[test]
    fn read() {
        let config_space = BlkConfig {
            capacity_low: ReadOnly::new(66),
            capacity_high: ReadOnly::new(0),
            size_max: ReadOnly::new(0),
            seg_max: ReadOnly::new(0),
            cylinders: ReadOnly::new(0),
            heads: ReadOnly::new(0),
            sectors: ReadOnly::new(0),
            blk_size: ReadOnly::new(0),
            physical_block_exp: ReadOnly::new(0),
            alignment_offset: ReadOnly::new(0),
            min_io_size: ReadOnly::new(0),
            opt_io_size: ReadOnly::new(0),
        };
        let state = Arc::new(Mutex::new(State::new(
            vec![QueueStatus::default()],
            config_space,
        )));
        let transport = FakeTransport {
            device_type: DeviceType::Block,
            max_queue_size: QUEUE_SIZE.into(),
            device_features: BlkFeature::RING_INDIRECT_DESC.bits(),
            state: state.clone(),
        };
        let mut blk = VirtIOBlk::<FakeHal, FakeTransport<BlkConfig>>::new(transport).unwrap();

        // Start a thread to simulate the device waiting for a read request.
        let handle = thread::spawn(move || {
            println!("Device waiting for a request.");
            State::wait_until_queue_notified(&state, QUEUE);
            println!("Transmit queue was notified.");

            assert!(
                state
                    .lock()
                    .unwrap()
                    .read_write_queue::<{ QUEUE_SIZE as usize }>(QUEUE, |request| {
                        assert_eq!(
                            request,
                            BlkReq {
                                type_: ReqType::In,
                                reserved: 0,
                                sector: 42
                            }
                            .as_bytes()
                        );

                        let mut response = vec![0; SECTOR_SIZE];
                        response[0..9].copy_from_slice(b"Test data");
                        response.extend_from_slice(
                            BlkResp {
                                status: RespStatus::OK,
                            }
                            .as_bytes(),
                        );

                        response
                    })
            );
        });

        // Read a block from the device.
        let mut buffer = [0; 512];
        blk.read_blocks(42, &mut buffer).unwrap();
        assert_eq!(&buffer[0..9], b"Test data");

        handle.join().unwrap();
    }

    #[test]
    fn write() {
        let config_space = BlkConfig {
            capacity_low: ReadOnly::new(66),
            capacity_high: ReadOnly::new(0),
            size_max: ReadOnly::new(0),
            seg_max: ReadOnly::new(0),
            cylinders: ReadOnly::new(0),
            heads: ReadOnly::new(0),
            sectors: ReadOnly::new(0),
            blk_size: ReadOnly::new(0),
            physical_block_exp: ReadOnly::new(0),
            alignment_offset: ReadOnly::new(0),
            min_io_size: ReadOnly::new(0),
            opt_io_size: ReadOnly::new(0),
        };
        let state = Arc::new(Mutex::new(State::new(
            vec![QueueStatus::default()],
            config_space,
        )));
        let transport = FakeTransport {
            device_type: DeviceType::Block,
            max_queue_size: QUEUE_SIZE.into(),
            device_features: BlkFeature::RING_INDIRECT_DESC.bits(),
            state: state.clone(),
        };
        let mut blk = VirtIOBlk::<FakeHal, FakeTransport<BlkConfig>>::new(transport).unwrap();

        // Start a thread to simulate the device waiting for a write request.
        let handle = thread::spawn(move || {
            println!("Device waiting for a request.");
            State::wait_until_queue_notified(&state, QUEUE);
            println!("Transmit queue was notified.");

            assert!(
                state
                    .lock()
                    .unwrap()
                    .read_write_queue::<{ QUEUE_SIZE as usize }>(QUEUE, |request| {
                        assert_eq!(
                            &request[0..size_of::<BlkReq>()],
                            BlkReq {
                                type_: ReqType::Out,
                                reserved: 0,
                                sector: 42
                            }
                            .as_bytes()
                        );
                        let data = &request[size_of::<BlkReq>()..];
                        assert_eq!(data.len(), SECTOR_SIZE);
                        assert_eq!(&data[0..9], b"Test data");

                        let mut response = Vec::new();
                        response.extend_from_slice(
                            BlkResp {
                                status: RespStatus::OK,
                            }
                            .as_bytes(),
                        );

                        response
                    })
            );
        });

        // Write a block to the device.
        let mut buffer = [0; 512];
        buffer[0..9].copy_from_slice(b"Test data");
        blk.write_blocks(42, &mut buffer).unwrap();

        // Request to flush should be ignored as the device doesn't support it.
        blk.flush().unwrap();

        handle.join().unwrap();
    }

    #[test]
    fn flush() {
        let config_space = BlkConfig {
            capacity_low: ReadOnly::new(66),
            capacity_high: ReadOnly::new(0),
            size_max: ReadOnly::new(0),
            seg_max: ReadOnly::new(0),
            cylinders: ReadOnly::new(0),
            heads: ReadOnly::new(0),
            sectors: ReadOnly::new(0),
            blk_size: ReadOnly::new(0),
            physical_block_exp: ReadOnly::new(0),
            alignment_offset: ReadOnly::new(0),
            min_io_size: ReadOnly::new(0),
            opt_io_size: ReadOnly::new(0),
        };
        let state = Arc::new(Mutex::new(State::new(
            vec![QueueStatus::default()],
            config_space,
        )));
        let transport = FakeTransport {
            device_type: DeviceType::Block,
            max_queue_size: QUEUE_SIZE.into(),
            device_features: (BlkFeature::RING_INDIRECT_DESC | BlkFeature::FLUSH).bits(),
            state: state.clone(),
        };
        let mut blk = VirtIOBlk::<FakeHal, FakeTransport<BlkConfig>>::new(transport).unwrap();

        // Start a thread to simulate the device waiting for a flush request.
        let handle = thread::spawn(move || {
            println!("Device waiting for a request.");
            State::wait_until_queue_notified(&state, QUEUE);
            println!("Transmit queue was notified.");

            assert!(
                state
                    .lock()
                    .unwrap()
                    .read_write_queue::<{ QUEUE_SIZE as usize }>(QUEUE, |request| {
                        assert_eq!(
                            request,
                            BlkReq {
                                type_: ReqType::Flush,
                                reserved: 0,
                                sector: 0,
                            }
                            .as_bytes()
                        );

                        let mut response = Vec::new();
                        response.extend_from_slice(
                            BlkResp {
                                status: RespStatus::OK,
                            }
                            .as_bytes(),
                        );

                        response
                    })
            );
        });

        // Request to flush.
        blk.flush().unwrap();

        handle.join().unwrap();
    }

    #[test]
    fn device_id() {
        let config_space = BlkConfig {
            capacity_low: ReadOnly::new(66),
            capacity_high: ReadOnly::new(0),
            size_max: ReadOnly::new(0),
            seg_max: ReadOnly::new(0),
            cylinders: ReadOnly::new(0),
            heads: ReadOnly::new(0),
            sectors: ReadOnly::new(0),
            blk_size: ReadOnly::new(0),
            physical_block_exp: ReadOnly::new(0),
            alignment_offset: ReadOnly::new(0),
            min_io_size: ReadOnly::new(0),
            opt_io_size: ReadOnly::new(0),
        };
        let state = Arc::new(Mutex::new(State::new(
            vec![QueueStatus::default()],
            config_space,
        )));
        let transport = FakeTransport {
            device_type: DeviceType::Block,
            max_queue_size: QUEUE_SIZE.into(),
            device_features: BlkFeature::RING_INDIRECT_DESC.bits(),
            state: state.clone(),
        };
        let mut blk = VirtIOBlk::<FakeHal, FakeTransport<BlkConfig>>::new(transport).unwrap();

        // Start a thread to simulate the device waiting for a flush request.
        let handle = thread::spawn(move || {
            println!("Device waiting for a request.");
            State::wait_until_queue_notified(&state, QUEUE);
            println!("Transmit queue was notified.");

            assert!(
                state
                    .lock()
                    .unwrap()
                    .read_write_queue::<{ QUEUE_SIZE as usize }>(QUEUE, |request| {
                        assert_eq!(
                            request,
                            BlkReq {
                                type_: ReqType::GetId,
                                reserved: 0,
                                sector: 0,
                            }
                            .as_bytes()
                        );

                        let mut response = Vec::new();
                        response.extend_from_slice(b"device_id\0\0\0\0\0\0\0\0\0\0\0");
                        response.extend_from_slice(
                            BlkResp {
                                status: RespStatus::OK,
                            }
                            .as_bytes(),
                        );

                        response
                    })
            );
        });

        let mut id = [0; 20];
        let length = blk.device_id(&mut id).unwrap();
        assert_eq!(&id[0..length], b"device_id");

        handle.join().unwrap();
    }
}

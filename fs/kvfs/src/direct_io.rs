// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Types and filesystem hooks for the generic direct I/O engine.
//!
//! The engine owns locking, alignment, bounce buffers, synchronous device
//! submission and the read-side dirty-range flush; the write path will own
//! page-cache invalidation as well. Filesystems implement
//! [`DirectIoOperations`]. The contract mirrors Linux
//! `iomap_begin`/`iomap_end`.

use alloc::sync::Arc;

use iov_iter::IovIterDestRewind;
use kalloc_types::KBox;
use kerrno::LinuxError;
#[cfg(unittest)]
use ksync::{Mutex, static_lock};

use crate::{Kiocb, VfsError, VfsInode, VfsResult, node::map_block_error};

/// Classification of the storage backing one direct I/O mapping run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DioMappingKind {
    /// No allocated storage. Reads return zeroes; writes must allocate
    /// backing blocks.
    Hole,
    /// Allocated and initialized storage.
    Mapped,
    /// Allocated but uninitialized storage. Reads return zeroes until the run
    /// is converted; a write converts the written prefix in
    /// [`DirectIoOperations::dio_end_io`] and leaves the remainder unwritten.
    Unwritten,
}

bitflags::bitflags! {
    /// Attributes of one mapping returned by the filesystem.
    ///
    /// Mirrors the flags a filesystem reports from Linux `iomap_begin`; bit
    /// values match `struct iomap.flags`. Iteration flags owned by the iomap
    /// core (`FOLIO_BATCH`, `SIZE_CHANGED`, `STALE`) are engine-internal and
    /// intentionally absent. The read engine does not consume these flags; the
    /// write path will consume [`Self::NEW`].
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct DioMappingFlags: u32 {
        /// The run was newly allocated; bytes not covered by the write must
        /// remain zero rather than expose stale data.
        const NEW = 1 << 0;
        /// The inode has metadata that must be committed before the written
        /// data is durable.
        const DIRTY = 1 << 1;
        /// The run is shared and must be unshared before it is written.
        const SHARED = 1 << 2;
        /// The run merges several adjacent mappings.
        const MERGED = 1 << 3;
        /// The filesystem requires buffer heads for this mapping.
        const BUFFER_HEAD = 1 << 4;
        /// The mapping covers extended-attribute storage rather than file data.
        const XATTR = 1 << 5;
        /// I/O and completion for this run must not merge with the previous
        /// run.
        const BOUNDARY = 1 << 6;
        /// The write has no target block yet; the filesystem assigns it during
        /// bio submission.
        const ANON_WRITE = 1 << 7;
        /// The write is issued as one atomic bio.
        const ATOMIC_BIO = 1 << 8;
        /// The filesystem handles integrity metadata for this run.
        const INTEGRITY = 1 << 9;
        /// Bytes after the written range up to the end of the block must be
        /// zeroed.
        const ZERO_TAIL = 1 << 10;
        /// The mapping covers fsverity metadata stored beyond `i_size`.
        const FSVERITY = 1 << 11;
        /// Reserved for filesystem-private use.
        const PRIVATE = 1 << 12;
    }
}

/// Direction of one direct I/O request.
///
/// Linux carries this as the `IOMAP_WRITE` flag (no flag implies a read) and
/// as the generic `READ`/`WRITE` data direction. An enum states the same
/// intent without making "read" the implicit default.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DioDirection {
    /// Reading existing storage; a hole or unwritten run reads as zeroes.
    Read,
    /// Writing data; the filesystem chooses whether and how to allocate.
    Write,
}

/// One contiguous backing run returned by a filesystem for a direct I/O
/// request.
///
/// This is a temporary value produced for each map call and must not be
/// cached across iterations because the underlying extent layout can change.
/// All offsets and lengths are in bytes: `offset` is in the file's logical
/// address space, `device_offset` in the backing device's address space.
pub struct DioMapping {
    /// Kind of storage backing this run.
    pub kind: DioMappingKind,
    /// Allocation state of this run.
    pub flags: DioMappingFlags,
    /// File byte offset where this run begins, in the file's logical address
    /// space. It may be at or before the `pos` of the map call because the
    /// filesystem returns the whole extent containing the request; the engine
    /// trims the run to the range it processes. Meaningful for every kind,
    /// including [`DioMappingKind::Hole`].
    pub offset: u64,
    /// Full byte length of the run, not clamped to the request. The engine
    /// trims it to the range it is currently processing.
    pub length_bytes: u64,
    /// Device byte offset of the first backing byte, in the backing device's
    /// address space. It backs the same bytes as [`Self::offset`]: file byte
    /// `offset + k` lives at device byte `device_offset + k`, for
    /// `0 <= k < length_bytes`. Ignored for [`DioMappingKind::Hole`], which
    /// has no backing storage and issues no I/O.
    pub device_offset: u64,
    /// Block device for [`DioMappingKind::Mapped`] and
    /// [`DioMappingKind::Unwritten`] runs; unused for a hole, which issues no
    /// I/O.
    pub device: Arc<block::BlockDevice>,
}

/// Filesystem callbacks used by the generic direct I/O engine.
///
/// Implementations resolve backing storage, report alignment and perform
/// post-write settlement. The engine in `kvfs` owns locking, bounce buffers,
/// bio submission and page-cache invalidation.
pub trait DirectIoOperations {
    /// Resolves the backing run starting at `pos` for up to `len_bytes` bytes.
    ///
    /// - `inode`: generic inode paired with the implementing filesystem
    ///   inode.
    /// - `pos`: file byte offset where the run begins.
    /// - `len_bytes`: maximum number of bytes still needed; the engine never
    ///   passes zero.
    /// - `direction`: whether the run is being read or written; a write lets
    ///   the filesystem choose its allocation form.
    ///
    /// Returns: the next contiguous run. [`DioMapping::offset`] and
    /// [`DioMapping::length_bytes`] are the full extent bounds; the engine
    /// trims them to the current request.
    ///
    /// Errors: allocation, journal, device errors and "direct I/O is not
    /// possible here" (the caller falls back to buffered I/O) propagate
    /// unchanged.
    ///
    /// Locking: called while the engine holds the inode data lock for the
    /// whole request (shared for reads and overwrites, exclusive when the
    /// write may allocate or extend). A backend that allocates must stop its
    /// journal handle before returning; the engine submits data bios after
    /// this call.
    fn dio_map_blocks(
        &self,
        inode: &VfsInode,
        pos: u64,
        len_bytes: u64,
        direction: DioDirection,
    ) -> VfsResult<DioMapping>;

    /// Settles filesystem state after all data bios of one written run
    /// finish.
    ///
    /// Called exactly once for every mapping returned by a write
    /// [`Self::dio_map_blocks`] call, including when `written_bytes` is zero.
    /// The engine already reports any device error to its caller; this
    /// callback settles metadata only.
    ///
    /// - `inode`: generic inode paired with the implementing filesystem
    ///   inode.
    /// - `pos`: file byte offset where the processed range begins; this is
    ///   the `pos` passed to the matching map call, not
    ///   [`DioMapping::offset`].
    /// - `length_bytes`: byte length of the mapped range processed by this
    ///   call; it never exceeds the request or [`DioMapping::length_bytes`].
    /// - `written_bytes`: bytes actually written; zero when nothing was
    ///   written, less than `length_bytes` on a short or failed write.
    /// - `mapping`: the run returned by [`Self::dio_map_blocks`].
    ///
    /// Returns: `Ok(())` when required metadata is committed.
    ///
    /// Errors: metadata, journal and device errors propagate unchanged; the
    /// write error, if any, takes precedence over this result.
    ///
    /// Locking: called while the engine holds the inode data lock exclusively.
    /// Publish size with [`VfsInode::grow_size_after_backing_write_locked`] or
    /// [`VfsInode::update_size_after_backing_change_locked`]; the locking
    /// variants reacquire the same lock and would deadlock here. Any journal
    /// handle started here must be stopped before returning.
    fn dio_end_io(
        &self,
        inode: &VfsInode,
        pos: u64,
        length_bytes: u64,
        written_bytes: u64,
        mapping: &DioMapping,
    ) -> VfsResult<()>;

    /// Returns the direct I/O offset and length alignment for `inode`, in
    /// bytes.
    ///
    /// - `inode`: generic inode paired with the implementing filesystem
    ///   inode.
    ///
    /// Returns: the filesystem's required alignment; `0` means direct I/O is not
    /// supported and the caller falls back to buffered I/O. A non-zero value is
    /// a power of two and applies to the file offset and request length. The
    /// engine separately enforces the backing device's logical block alignment
    /// per run; memory alignment is guaranteed by the owned bounce buffer.
    ///
    /// Locking: called while the engine holds the inode data lock shared, before
    /// any mapping runs. Must not sleep or acquire a lock the data lock excludes,
    /// matching Linux `ext4_dio_alignment`.
    fn dio_alignment(&self, inode: &VfsInode) -> u64;
}

/// Bounce allocation stride for read runs, mirroring the Linux SZ_1M bounce
/// cap in `bio_iov_iter_bounce_read`; device-level splitting stays in the
/// block layer.
const DIO_READ_STRIDE: usize = 1024 * 1024;

/// Maximum number of tasks that may inject bounce failures concurrently.
#[cfg(unittest)]
const BOUNCE_INJECT_SLOTS: usize = 8;

#[cfg(unittest)]
static_lock! {
    /// Bounce-allocation failures injected per task by unit tests.
    ///
    /// Fixed size and keyed by task address so parallel test workers never consume
    /// or clear each other's injection; an empty slot has a zero task identity.
    static BOUNCE_ALLOC_FAIL: Mutex<[(usize, usize); BOUNCE_INJECT_SLOTS]> =
        Mutex::new([(0, 0); BOUNCE_INJECT_SLOTS]);
}

/// Returns the test identity of the current task, its object address.
#[cfg(unittest)]
fn current_task_identity() -> usize {
    let task = ktask::current().clone();
    ktask::KtaskRef::as_ptr(&task) as usize
}

/// Sets the number of bounce-allocation failures injected into the current task.
///
/// - `count`: failures the current task consumes before allocating succeeds;
///   zero clears this task's injection only.
///
/// Locking: takes the injection table mutex; test-only.
#[cfg(unittest)]
fn set_bounce_alloc_failures(count: usize) {
    let identity = current_task_identity();
    let mut slots = BOUNCE_ALLOC_FAIL.lock();
    if let Some(index) = slots.iter().position(|(task, _)| *task == identity) {
        slots[index] = if count == 0 {
            (0, 0)
        } else {
            (identity, count)
        };
        return;
    }
    if count == 0 {
        return;
    }
    // The table is sized well above the number of tests that inject
    // concurrently; exhausting it surfaces as a test assertion failure.
    let Some(index) = slots.iter().position(|(task, _)| *task == 0) else {
        return;
    };
    slots[index] = (identity, count);
}

/// Consumes one pending failure injected for the current task, if any.
#[cfg(unittest)]
fn bounce_alloc_fail_next() -> bool {
    let identity = current_task_identity();
    let mut slots = BOUNCE_ALLOC_FAIL.lock();
    let Some(index) = slots.iter().position(|(task, _)| *task == identity) else {
        return false;
    };
    if slots[index].1 == 0 {
        return false;
    }
    slots[index].1 -= 1;
    if slots[index].1 == 0 {
        slots[index] = (0, 0);
    }
    true
}

/// Allocates one zero-filled bounce buffer for a device read.
///
/// - `len`: buffer length in bytes; nonzero, bounded by the caller's stride.
///
/// Returns: the owned zero-filled buffer.
///
/// Errors: `NoMemory` when the allocation fails; the caller may retry with a
/// smaller length.
///
/// Locking: takes no locks. May sleep in reclaim.
fn dio_alloc_bounce(len: usize) -> VfsResult<KBox<[u8]>> {
    #[cfg(unittest)]
    if bounce_alloc_fail_next() {
        return Err(VfsError::NoMemory);
    }
    KBox::new_slice(0u8, len)
}

/// Reads one contiguous device range into a fresh bounce buffer and returns
/// it filled.
///
/// - `device`: block device backing `device_offset`; its logical block size
///   bounds the alignment and length accepted here.
/// - `device_offset`: device byte offset of the range start; must be a
///   multiple of the device logical block size.
/// - `len`: byte length of the range; nonzero and a multiple of the logical
///   block size. The engine passes bounded strides; the block layer splits
///   the bio by the device's segment limits.
///
/// Returns: the bounce buffer holding the `len` bytes read.
///
/// Errors: `EINVAL` for a zero or misaligned range, or a range beyond the
/// device; `EFBIG` when `device_offset + len` overflows; `NoMemory` when the
/// bounce allocation fails; other errors propagate unchanged.
///
/// Locking: takes no locks. May sleep on device completion; the caller must
/// be in sleepable task context and must not hold locks that forbid sleeping.
fn dio_read_chunk(
    device: &Arc<block::BlockDevice>,
    device_offset: u64,
    len: usize,
) -> VfsResult<KBox<[u8]>> {
    let block_size = device.block_size();
    if len == 0
        || !len.is_multiple_of(block_size)
        || !device_offset.is_multiple_of(block_size as u64)
    {
        return Err(VfsError::from(LinuxError::EINVAL));
    }
    let end = device_offset
        .checked_add(len as u64)
        .ok_or(VfsError::from(LinuxError::EFBIG))?;
    if end > device.size() {
        return Err(VfsError::from(LinuxError::EINVAL));
    }

    let block_id = device_offset / block_size as u64;
    let mut buffer = dio_alloc_bounce(len)?;
    device
        .read_block(block_id, &mut buffer)
        .map_err(map_block_error)?;
    Ok(buffer)
}

/// Zero source used by the read engine to fill hole and unwritten runs.
const ZERO_TAIL: [u8; 4096] = [0; 4096];

/// Copies `len` zero bytes into the destination until it stops accepting.
///
/// - `iter`: rewindable destination iterator; its cursor follows the copies.
/// - `len`: number of zero bytes to deliver; nonzero.
///
/// Returns: the number of zero bytes copied, at most `len`; less than `len`
/// only when the destination became full.
///
/// Errors: iterator faults propagate unchanged.
///
/// Locking: takes no locks.
fn dio_copy_zeros(iter: &mut IovIterDestRewind<'_>, mut len: usize) -> VfsResult<usize> {
    let mut done = 0usize;
    while len > 0 {
        let chunk = len.min(ZERO_TAIL.len());
        let copied = iter.copy_to_iter(&ZERO_TAIL[..chunk])?;
        done += copied;
        if copied < chunk {
            break;
        }
        len -= chunk;
    }
    Ok(done)
}

/// Maps and transfers runs until the destination budget is exhausted or a
/// copy comes up short.
///
/// - `inode`: the file being read; paired with `ops`.
/// - `iter`: rewindable destination iterator; its cursor follows successful
///   copies.
/// - `ops`: direct-I/O callbacks of the file's filesystem.
/// - `cur`: current file position; advanced by the bytes actually delivered.
///
/// Returns: `Ok(())` when the loop ends on an exhausted budget or a short
/// copy; `*cur` then holds the position after the last delivered byte.
///
/// Errors: `EINVAL` for a mapping that does not cover `*cur`, whose device
/// offset would overflow, or whose file position/length is not a multiple of
/// the device logical block size; `EFAULT` when the destination accepts
/// nothing while bytes remain; other errors propagate unchanged.
///
/// Locking: caller must hold the inode data lock shared. May sleep on
/// mapping, device completion and destination faults.
fn dio_read_runs(
    inode: &VfsInode,
    iter: &mut IovIterDestRewind<'_>,
    ops: &dyn DirectIoOperations,
    cur: &mut u64,
) -> VfsResult<()> {
    while iter.count() > 0 {
        let mapping = ops.dio_map_blocks(inode, *cur, iter.count() as u64, DioDirection::Read)?;
        let into = (*cur)
            .checked_sub(mapping.offset)
            .ok_or(VfsError::from(LinuxError::EINVAL))?;
        let run_len = mapping
            .length_bytes
            .saturating_sub(into)
            .min(iter.count() as u64) as usize;
        if run_len == 0 {
            return Err(VfsError::from(LinuxError::EINVAL));
        }
        let delivered = match mapping.kind {
            DioMappingKind::Hole | DioMappingKind::Unwritten => dio_copy_zeros(iter, run_len)?,
            DioMappingKind::Mapped => {
                let device_offset = mapping
                    .device_offset
                    .checked_add(into)
                    .ok_or(VfsError::from(LinuxError::EINVAL))?;
                let block_size = mapping.device.block_size();
                // The device logical block size is the device-alignment
                // authority, checked per run before any copy, as in Linux
                // iomap_dio_bio_iter's `(pos | length)` check.
                if !(*cur).is_multiple_of(block_size as u64) || !run_len.is_multiple_of(block_size)
                {
                    return Err(VfsError::from(LinuxError::EINVAL));
                }
                let mut stride = DIO_READ_STRIDE;
                let mut done = 0usize;
                while done < run_len {
                    let want = (run_len - done).min(stride);
                    let offset = device_offset
                        .checked_add(done as u64)
                        .ok_or(VfsError::from(LinuxError::EINVAL))?;
                    let buffer = match dio_read_chunk(&mapping.device, offset, want) {
                        Ok(buffer) => buffer,
                        Err(VfsError::NoMemory) if want > block_size => {
                            // Halve toward the device block size, staying a
                            // block multiple so the next request aligns.
                            stride = ((want / 2) & !(block_size - 1)).max(block_size);
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    let copied = iter.copy_to_iter(&buffer)?;
                    done += copied;
                    if copied < want {
                        break;
                    }
                }
                done
            }
        };
        if delivered == 0 {
            return Err(VfsError::from(LinuxError::EFAULT));
        }
        *cur += delivered as u64;
        if delivered < run_len {
            break;
        }
    }
    Ok(())
}

/// Performs one direct-I/O read over the given operations table.
///
/// - `iocb`: request control block; its position advances by the returned byte
///   count on success only.
/// - `iter`: rewindable destination iterator; its budget caps the transfer and
///   its cursor follows successful copies, including an end-of-file tail rewind
///   on both success and failure. The rewind capability is carried by the type.
/// - `ops`: direct-I/O callbacks of the file's filesystem; `dio_alignment` must
///   return zero or a power of two (zero means direct I/O is unsupported and
///   the caller routes to buffered I/O before entering here).
///
/// Returns: bytes copied into `iter`, shorter than requested on end-of-file or
/// a full destination.
///
/// Errors: `EINVAL` for a misaligned request or an unsupported alignment;
/// `EFAULT` when the destination accepts nothing while bytes remain; a failed
/// end-of-file rewind propagates on success, while on failure the original
/// error takes precedence; device, mapping and flush errors propagate
/// unchanged, without advancing `iocb`.
///
/// Locking: takes the inode data lock shared before the alignment hook and holds
/// it for the duration. May sleep flushing dirty pages and waiting for device
/// completion; the caller must run in a sleepable task context and must not hold
/// locks that forbid sleeping.
pub fn dio_read_iter(
    iocb: &mut Kiocb<'_>,
    iter: &mut IovIterDestRewind<'_>,
    ops: &dyn DirectIoOperations,
) -> VfsResult<usize> {
    let inode = iocb.file().inode();
    let _data = inode.lock_data_shared();
    let align = ops.dio_alignment(inode);
    if align == 0 || !align.is_power_of_two() {
        return Err(VfsError::from(LinuxError::EINVAL));
    }
    let pos = iocb.ki_pos();
    let size = inode.size();
    let budget0 = iter.count();
    if budget0 == 0 || pos >= size {
        return Ok(0);
    }
    if (pos | budget0 as u64) & (align - 1) != 0 {
        return Err(VfsError::from(LinuxError::EINVAL));
    }
    let flush_end = pos
        .checked_add(budget0 as u64)
        .ok_or(VfsError::from(LinuxError::EINVAL))?;
    iocb.file().mapping().write_and_wait_range(pos..flush_end)?;

    let mut cur = pos;
    let result = dio_read_runs(inode, iter, ops, &mut cur);

    if cur > size {
        let rewind = (cur - size) as usize;
        if result.is_ok() {
            iter.revert(rewind)?;
        } else {
            // The original I/O error takes precedence over a rewind failure.
            let _ = iter.revert(rewind);
        }
    }
    match result {
        Ok(()) => {
            let written = budget0 - iter.count();
            iocb.advance(written);
            Ok(written)
        }
        Err(error) => Err(error),
    }
}

unittest::path_mod_test!(tests, "../ut-tests/direct_io.rs");

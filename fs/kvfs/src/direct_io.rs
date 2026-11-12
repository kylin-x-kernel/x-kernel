// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Types and filesystem hooks for the generic direct I/O engine.
//!
//! The engine owns locking, alignment, bounce buffers, bio submission and
//! page-cache invalidation; filesystems implement [`DirectIoOperations`].
//! The contract mirrors Linux `iomap_begin`/`iomap_end`.

use alloc::sync::Arc;

use crate::{VfsInode, VfsResult};

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
    /// intentionally absent. The engine currently consumes [`Self::NEW`].
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
    /// Returns: the required alignment; `0` means direct I/O is not supported
    /// and the caller falls back to buffered I/O. A non-zero value is a power
    /// of two and applies to the file offset and request length only. Memory
    /// alignment is guaranteed by the engine's owned bounce buffer.
    fn dio_alignment(&self, inode: &VfsInode) -> u64;
}

unittest::path_mod_test!(tests, "../ut-tests/direct_io.rs");

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! File I/O syscalls.
//!
//! This module implements file input/output operations including:
//! - Reading and writing (read, write, pread, pwrite, etc.)
//! - Vectored I/O (readv, writev, preadv, pwritev, etc.)
//! - File seeking (lseek, etc.)
//! - Splice and transfer operations (splice, sendfile, etc.)
//! - File synchronization (fsync, fdatasync, etc.)

#[cfg(unittest)]
use alloc::sync::Arc;
use core::ffi::{c_char, c_int};

use iov_iter::{IovSink, IovSource, iov_iter_dest, iov_iter_source};
use kerrno::{KError, KResult, LinuxError};
use kio::prelude::*;
use kvfs::{
    FMode, FallocateFlags, Filename, LookupFlags, LookupIntent, NodeType, OpenFlags,
    TransferPosition, VfsFile, pipe::PipeObject,
};
use linux_raw_sys::general::__kernel_off_t;
use linux_sysno::Sysno;
use osvm::{VirtPtr, VmBytes, VmBytesMut};
use posix_types::{IoVec, IoVectorBuf, IoVectorBufIo, UserConstPtr, UserPtr};

trait RewindBytes {
    fn rewind_bytes(&mut self, count: usize) -> KResult<()>;
}

impl RewindBytes for VmBytes {
    fn rewind_bytes(&mut self, count: usize) -> KResult<()> {
        VmBytes::rewind_bytes(self, count)
    }
}

impl RewindBytes for VmBytesMut {
    fn rewind_bytes(&mut self, count: usize) -> KResult<()> {
        VmBytesMut::rewind_bytes(self, count)
    }
}

impl RewindBytes for IoVectorBufIo {
    fn rewind_bytes(&mut self, count: usize) -> KResult<()> {
        IoVectorBufIo::rewind_bytes(self, count)
    }
}

struct IoSourceAdapter<T>(T);

impl<T> IovSource for IoSourceAdapter<T>
where
    T: Read + IoBuf + RewindBytes,
{
    fn count(&self) -> usize {
        self.0.remaining()
    }

    fn copy_from_iter(&mut self, dst: &mut [u8]) -> KResult<usize> {
        self.0.read(dst)
    }

    fn revert(&mut self, count: usize) -> KResult<()> {
        self.0.rewind_bytes(count)
    }
}

struct IoSinkAdapter<T>(T);

impl<T> IovSink for IoSinkAdapter<T>
where
    T: Write + IoBufMut + RewindBytes,
{
    fn count(&self) -> usize {
        self.0.remaining_mut()
    }

    fn copy_to_iter(&mut self, src: &[u8]) -> KResult<usize> {
        self.0.write(src)
    }

    fn revert(&mut self, count: usize) -> KResult<()> {
        self.0.rewind_bytes(count)
    }
}

fn write_zeros_range(file: &VfsFile, start: u64, end: u64) -> KResult<()> {
    if end <= start {
        return Ok(());
    }

    let zeros = [0u8; 4096];
    let mut pos = start;
    let mut remaining = end - start;
    while remaining > 0 {
        let write_len = remaining.min(zeros.len() as u64) as usize;
        let written = file.write_from(&zeros[..write_len], &mut pos)?;
        if written == 0 {
            return Err(KError::WriteZero);
        }
        remaining -= written as u64;
    }
    Ok(())
}

// Finalize a complete request, not one step of an aggregate transfer.
fn read_request_to_io(file: &VfsFile, dst: impl Write + IoBufMut + RewindBytes) -> KResult<usize> {
    let mut dst = IoSinkAdapter(dst);
    let mut iter = iov_iter_dest(&mut dst);
    finish_restartable_io(file.read_iter(&mut iter))
}

fn read_file_at_to_io(
    file: &VfsFile,
    dst: impl Write + IoBufMut + RewindBytes,
    mut offset: u64,
) -> KResult<usize> {
    reject_positioned_stream_io(file)?;

    let mut dst = IoSinkAdapter(dst);
    let mut iter = iov_iter_dest(&mut dst);
    file.read_iter_from(&mut iter, &mut offset)
}

fn write_file_from_io_with_pos(
    file: &VfsFile,
    src: impl Read + IoBuf + RewindBytes,
    pos: &mut u64,
) -> KResult<usize> {
    let mut src = IoSourceAdapter(src);
    let mut iter = iov_iter_source(&mut src);
    file.write_iter_from(&mut iter, pos)
}

// Finalize a complete request, not one step of an aggregate transfer.
fn write_request_from_io(file: &VfsFile, src: impl Read + IoBuf + RewindBytes) -> KResult<usize> {
    let result = if file.is_stream() {
        let mut pos = 0;
        write_file_from_io_with_pos(file, src, &mut pos)
    } else {
        let mut pos = file.position_lock();
        write_file_from_io_with_pos(file, src, &mut pos)
    };
    finish_restartable_io(result)
}

fn write_file_at_from_io(
    file: &VfsFile,
    src: impl Read + IoBuf + RewindBytes,
    mut offset: u64,
) -> KResult<usize> {
    reject_positioned_stream_io(file)?;

    let mut src = IoSourceAdapter(src);
    let mut iter = iov_iter_source(&mut src);
    file.write_iter_from(&mut iter, &mut offset)
}

fn reject_positioned_stream_io(file: &VfsFile) -> KResult<()> {
    if file.is_stream() {
        Err(KError::from(LinuxError::ESPIPE))
    } else {
        Ok(())
    }
}

/// Returns `ENOSYS` for syscalls that are deliberately not implemented
/// (e.g. the new mount API: `fsopen`/`fspick`/`open_tree`, `io_uring_setup`,
/// `fanotify_init`). User space probes these and falls back to a supported
/// path on `ENOSYS`, so the failure is expected and not worth a warning per
/// call. Logged at `debug!` for diagnosability.
pub fn sys_dummy_fd(sysno: Sysno) -> KResult<isize> {
    debug!("Unsupported syscall requested dummy fd: {sysno}");
    Err(KError::Unsupported)
}

/// Read data from the file indicated by `fd`.
///
/// Return the read size if success.
pub fn sys_read(fd: i32, buf: UserPtr<u8>, len: usize) -> KResult<isize> {
    debug!("sys_read <= fd: {fd}, buf: {:p}, len: {len}", buf.as_ptr());
    // Get the file object and perform the read operation into the user buffer
    let file = kprocess::current_files().get_file(fd)?;
    read_request_to_io(&file, VmBytesMut::new(buf.as_ptr().cast_mut(), len))
        .map(|count| count as isize)
}

/// Vectored read into multiple buffers.
pub fn sys_readv(fd: i32, iov: UserConstPtr<IoVec>, iovcnt: usize) -> KResult<isize> {
    debug!("sys_readv <= fd: {fd}, iovcnt: {iovcnt}");
    // Vectored read - read data into multiple buffers in a single operation
    let f = kprocess::current_files().get_file(fd)?;
    let iov = IoVectorBuf::from_iovecs(IoVec::load_from_user(iov, iovcnt)?)?;
    read_request_to_io(&f, iov.into_io()).map(|count| count as isize)
}

/// Write data to the file indicated by `fd`.
///
/// Return the written size if success.
pub fn sys_write(fd: i32, buf: UserConstPtr<u8>, len: usize) -> KResult<isize> {
    debug!("sys_write <= fd: {fd}, buf: {:p}, len: {len}", buf.as_ptr());
    let file = kprocess::current_files().get_file(fd)?;
    write_request_from_io(&file, VmBytes::new(buf.as_ptr(), len)).map(|count| count as isize)
}

/// Vectored write from multiple buffers.
pub fn sys_writev(fd: i32, iov: UserConstPtr<IoVec>, iovcnt: usize) -> KResult<isize> {
    debug!("sys_writev <= fd: {fd}, iovcnt: {iovcnt}");
    // Vectored write - write data from multiple buffers in a single operation
    let f = kprocess::current_files().get_file(fd)?;
    let iov = IoVectorBuf::from_iovecs(IoVec::load_from_user(iov, iovcnt)?)?;
    write_request_from_io(&f, iov.into_io()).map(|count| count as isize)
}

// The non-positioned request helpers share this exit, including v2 offset=-1.
// Object/iterator progress has already been accounted for. Copy syscalls must
// use raw VFS operations and decide interruption at their aggregate boundary.
fn finish_restartable_io(result: KResult<usize>) -> KResult<usize> {
    match result {
        Err(KError::IoWaitInterrupted) => Err(LinuxError::ERESTARTSYS.into()),
        result => result,
    }
}

unittest::path_mod_test!(fallocate_mode_tests, "../ut-tests/fallocate_mode_tests.rs");

unittest::path_mod_test!(restart_tests, "../ut-tests/io_restart_tests.rs");

unittest::path_mod_test!(
    transfer_count_tests,
    "../ut-tests/io_transfer_count_tests.rs"
);

unittest::path_mod_test!(
    /// Boundary checks and settlement semantics of the `copy_file_range(2)`
    /// engine, mirroring Linux `generic_copy_file_checks` (`fs/read_write.c`).
    mod copy_file_range_checks_tests, "../ut-tests/io_copy_file_range_checks_tests.rs"
);

/// Repositions the read/write file offset.
pub fn sys_lseek(fd: c_int, offset: __kernel_off_t, whence: c_int) -> KResult<isize> {
    debug!("sys_lseek <= {fd} {offset} {whence}");
    let file = kprocess::current_files().get_file(fd)?;
    Ok(file.llseek(offset, whence)? as _)
}

/// Truncates a file to a specified length by path.
pub fn sys_truncate(path: UserConstPtr<c_char>, length: __kernel_off_t) -> KResult<isize> {
    let path = path.load_string()?;
    debug!("sys_truncate <= {path:?} {length}");
    // Truncate file to specified length - opens file by path
    if length < 0 {
        return Err(KError::InvalidInput);
    }
    let fs_struct = kprocess::current_user_process_fs_context();
    let fs = fs_struct.lock();
    let cred = kprocess::current_cred();
    let loc = Filename::new(path.as_str()).lookup_at(
        fs.root(),
        fs.pwd(),
        LookupIntent::Open,
        LookupFlags::follow(),
        &cred,
    )?;
    loc.truncate(length as _, &cred)?;
    Ok(0)
}

/// Truncates a file to a specified length by file descriptor.
pub fn sys_ftruncate(fd: c_int, length: __kernel_off_t) -> KResult<isize> {
    debug!("sys_ftruncate <= {fd} {length}");
    // Truncate file descriptor to specified length
    if length < 0 {
        return Err(KError::InvalidInput);
    }
    let f = kprocess::current_files().get_file(fd)?;
    f.truncate(length as _)?;
    Ok(0)
}

/// Decodes `fallocate` request flags and validates their legality, mirroring
/// `vfs_fallocate`'s checks. The errno split follows the reference exactly
/// (fs/open.c): punch-related violations are EOPNOTSUPP (lines 257-264),
/// while the exclusive-only range-shifting rules are EINVAL (lines 266-279).
fn parse_fallocate(mode: u32) -> KResult<FallocateFlags> {
    // Unknown bits are EOPNOTSUPP in vfs_fallocate (line 253); bitflags'
    // `from_bits` reports them by returning `None`.
    let Some(flags) = FallocateFlags::from_bits(mode) else {
        return Err(KError::OperationNotSupported);
    };
    // PUNCH_HOLE and ZERO_RANGE are mutually exclusive (lines 257-259), and
    // PUNCH_HOLE requires KEEP_SIZE (lines 262-264); both are EOPNOTSUPP.
    if flags.contains(FallocateFlags::PUNCH_HOLE | FallocateFlags::ZERO_RANGE) {
        return Err(KError::OperationNotSupported);
    }
    if flags.contains(FallocateFlags::PUNCH_HOLE) && !flags.contains(FallocateFlags::KEEP_SIZE) {
        return Err(KError::OperationNotSupported);
    }
    // Range-shifting modes must be used exclusively (COLLAPSE lines
    // 266-269, INSERT lines 271-274) and UNSHARE only accepts KEEP_SIZE
    // (lines 276-279); violations are EINVAL.
    let exclusive = |solo: FallocateFlags| {
        let rest = flags - solo;
        !rest.is_empty()
    };
    if flags.contains(FallocateFlags::COLLAPSE_RANGE) && exclusive(FallocateFlags::COLLAPSE_RANGE) {
        return Err(KError::InvalidInput);
    }
    if flags.contains(FallocateFlags::INSERT_RANGE) && exclusive(FallocateFlags::INSERT_RANGE) {
        return Err(KError::InvalidInput);
    }
    if flags.contains(FallocateFlags::UNSHARE_RANGE) {
        let rest = flags - FallocateFlags::UNSHARE_RANGE;
        if !rest.is_empty() && rest != FallocateFlags::KEEP_SIZE {
            return Err(KError::InvalidInput);
        }
    }
    Ok(flags)
}

pub fn sys_fallocate(
    fd: c_int,
    mode: u32,
    offset: __kernel_off_t,
    len: __kernel_off_t,
) -> KResult<isize> {
    debug!("sys_fallocate <= fd: {fd}, mode: {mode}, offset: {offset}, len: {len}");
    // Order mirrors the Linux call chain exactly: ksys_fallocate resolves the
    // descriptor first (EBADF), then vfs_fallocate checks the range
    // (EINVAL), mode legality (EOPNOTSUPP/EINVAL), write permission
    // (FMODE_WRITE, EBADF), node-type dispatch (ESPIPE/EISDIR/ENODEV), and
    // finally the file's size bound (EFBIG) — before any backend runs.
    let f = kprocess::current_files().get_file(fd)?;
    if offset < 0 || len <= 0 {
        return Err(KError::InvalidInput);
    }
    let flags = parse_fallocate(mode)?;
    let keep_size = flags.contains(FallocateFlags::KEEP_SIZE);
    let base_mode = flags - FallocateFlags::KEEP_SIZE;
    f.verify_mode(FMode::WRITE)?;

    // Node-type dispatch mirrors vfs_fallocate (fs/open.c): FIFOs are not
    // seekable, directories are their own error, and only regular files and
    // block devices have range semantics at all.
    match f.inode().node_type() {
        NodeType::Fifo => return Err(KError::from(LinuxError::ESPIPE)),
        NodeType::Directory => return Err(KError::from(LinuxError::EISDIR)),
        NodeType::RegularFile | NodeType::BlockDevice => {}
        _ => return Err(KError::from(LinuxError::ENODEV)),
    }

    let start = offset as u64;
    let len_u = len as u64;

    // Block devices validate mode/capacity/alignment in their own fallocate
    // (overflow past u64 is EFBIG there, capacity violations are EINVAL or
    // KEEP_SIZE-clamped). On Linux a block device's inode lives in the block
    // pseudo-superblock, so the hosting filesystem's size limit does not
    // apply; keep the hosting `max_file_size()` bound for regular files
    // only.
    if f.inode().node_type() == NodeType::BlockDevice {
        return f.fallocate(flags, start, len_u).map(|_| 0);
    }
    // Overflow past the regular file's maximum is EFBIG (fs/open.c:321),
    // not EINVAL.
    let end = start
        .checked_add(len_u)
        .ok_or_else(|| KError::from(LinuxError::EFBIG))?;
    if end > f.max_file_size() {
        return Err(KError::from(LinuxError::EFBIG));
    }
    let old_size = f.size();

    if base_mode == FallocateFlags::empty() {
        // Standard preallocation behavior in our current implementation.
        if !keep_size {
            let target_size = old_size.max(end);
            if target_size > old_size {
                // Some backends may delay i_size visibility on set_len-only growth.
                // Force EOF advancement with a single-byte write at target_size - 1.
                let z = [0u8; 1];
                let mut pos = target_size - 1;
                let written = f.write_from(&z[..], &mut pos)?;
                if written != 1 {
                    return Err(KError::WriteZero);
                }
            }
        }
    } else if base_mode == FallocateFlags::PUNCH_HOLE {
        // Emulate punch hole by zeroing existing file range.
        let target_end = end.min(old_size);
        if target_end > start {
            write_zeros_range(&f, start, target_end)?;
        }
    } else if base_mode == FallocateFlags::ZERO_RANGE {
        // Emulate zero-range by writing zeros to the range. KEEP_SIZE only
        // affects visible bytes in current file size; avoid temporary i_size
        // growth, which can perturb extent metadata on backends with
        // incomplete hole/truncate handling.
        let target_end = if keep_size { end.min(old_size) } else { end };
        if target_end > start {
            if !keep_size && target_end > old_size {
                f.truncate(target_end)?;
            }
            write_zeros_range(&f, start, target_end)?;
        }
    } else if base_mode == FallocateFlags::UNSHARE_RANGE {
        // On non-reflink filesystems, emulate unshare by preallocating range
        // semantics.
        if !keep_size {
            let target_size = old_size.max(end);
            if target_size > old_size {
                let z = [0u8; 1];
                let mut pos = target_size - 1;
                let written = f.write_from(&z[..], &mut pos)?;
                if written != 1 {
                    return Err(KError::WriteZero);
                }
            }
        }
    } else if base_mode == FallocateFlags::COLLAPSE_RANGE
        || base_mode == FallocateFlags::INSERT_RANGE
    {
        // Range-shifting modes: reject out-of-range requests with EINVAL
        // before forwarding, and answer a missing backend implementation
        // with EOPNOTSUPP (Linux: `!f_op->fallocate`), never ENOSYS.
        if base_mode == FallocateFlags::COLLAPSE_RANGE && (end > old_size || start >= old_size) {
            return Err(KError::InvalidInput);
        }
        if base_mode == FallocateFlags::INSERT_RANGE && start > old_size {
            return Err(KError::InvalidInput);
        }
        f.fallocate(base_mode, start, len_u)?;
    } else {
        // Other flag combinations are not supported by the emulation;
        // fallocate mode errors are EOPNOTSUPP on Linux (fs-op default),
        // never ENOSYS.
        return Err(KError::OperationNotSupported);
    }

    Ok(0)
}

/// Synchronizes a file's in-core state with storage.
pub fn sys_fsync(fd: c_int) -> KResult<isize> {
    debug!("sys_fsync <= {fd}");
    // Synchronize file to disk - syncs both data and metadata
    let file = kprocess::current_files().get_file(fd)?;
    file.fsync(false)?;
    Ok(0)
}

/// Synchronizes file data and the metadata needed to retrieve it with storage.
pub fn sys_fdatasync(fd: c_int) -> KResult<isize> {
    debug!("sys_fdatasync <= {fd}");
    let file = kprocess::current_files().get_file(fd)?;
    file.fsync(true)?;
    Ok(0)
}

/// Provides access pattern advice for a file region.
pub fn sys_fadvise64(
    fd: c_int,
    offset: __kernel_off_t,
    len: __kernel_off_t,
    advice: u32,
) -> KResult<isize> {
    debug!("sys_fadvise64 <= fd: {fd}, offset: {offset}, len: {len}, advice: {advice}");
    // Provide hints to kernel about how file will be accessed
    // Currently not fully implemented - pipes are not supported
    let file = kprocess::current_files().get_file(fd)?;
    if PipeObject::from_file(&file).is_ok() {
        return Err(KError::BrokenPipe);
    }
    if advice > 5 {
        return Err(KError::InvalidInput);
    }
    Ok(0)
}

/// Reads from a file at a given offset without changing the file position.
pub fn sys_pread64(
    fd: c_int,
    buf: UserPtr<u8>,
    len: usize,
    offset: __kernel_off_t,
) -> KResult<isize> {
    // Read from file at specific offset without changing file position
    if offset < 0 {
        return Err(KError::InvalidInput);
    }
    let f = kprocess::current_files().get_file(fd)?;
    reject_positioned_stream_io(&f)?;
    let read = read_file_at_to_io(
        &f,
        VmBytesMut::new(buf.as_ptr().cast_mut(), len),
        offset as _,
    )?;
    Ok(read as _)
}

/// Writes to a file at a given offset without changing the file position.
pub fn sys_pwrite64(
    fd: c_int,
    buf: UserConstPtr<u8>,
    len: usize,
    offset: __kernel_off_t,
) -> KResult<isize> {
    // Write to file at specific offset without changing file position
    if offset < 0 {
        return Err(KError::InvalidInput);
    }
    let f = kprocess::current_files().get_file(fd)?;
    reject_positioned_stream_io(&f)?;
    if len == 0 {
        return Ok(0);
    }
    let write = write_file_at_from_io(&f, VmBytes::new(buf.as_ptr(), len), offset as _)?;
    Ok(write as _)
}

/// Vectored read at a given offset.
pub fn sys_preadv(
    fd: c_int,
    iov: UserConstPtr<IoVec>,
    iovcnt: usize,
    offset: __kernel_off_t,
) -> KResult<isize> {
    // Vectored read at specific offset - delegates to preadv2 with flags=0
    if offset < 0 {
        return Err(KError::InvalidInput);
    }
    sys_preadv2(fd, iov, iovcnt, offset, 0)
}

/// Vectored write at a given offset.
pub fn sys_pwritev(
    fd: c_int,
    iov: UserConstPtr<IoVec>,
    iovcnt: usize,
    offset: __kernel_off_t,
) -> KResult<isize> {
    // Vectored write at specific offset - delegates to pwritev2 with flags=0
    if offset < 0 {
        return Err(KError::InvalidInput);
    }
    sys_pwritev2(fd, iov, iovcnt, offset, 0)
}

/// Vectored read at a given offset with flags.
pub fn sys_preadv2(
    fd: c_int,
    iov: UserConstPtr<IoVec>,
    iovcnt: usize,
    offset: __kernel_off_t,
    flags: u32,
) -> KResult<isize> {
    debug!("sys_preadv2 <= fd: {fd}, iovcnt: {iovcnt}, offset: {offset}, flags: {flags}");
    // Vectored read at specific offset with optional flags
    if offset < -1 {
        return Err(KError::InvalidInput);
    }
    if flags != 0 {
        return Err(KError::Unsupported);
    }
    let f = kprocess::current_files().get_file(fd)?;
    if offset != -1 {
        reject_positioned_stream_io(&f)?;
    }
    let iov = IoVectorBuf::from_iovecs(IoVec::load_from_user(iov, iovcnt)?)?;
    if offset == -1 {
        read_request_to_io(&f, iov.into_io()).map(|n| n as _)
    } else {
        read_file_at_to_io(&f, iov.into_io(), offset as _).map(|n| n as _)
    }
}

/// Vectored write at a given offset with flags.
pub fn sys_pwritev2(
    fd: c_int,
    iov: UserConstPtr<IoVec>,
    iovcnt: usize,
    offset: __kernel_off_t,
    flags: u32,
) -> KResult<isize> {
    debug!("sys_pwritev2 <= fd: {fd}, iovcnt: {iovcnt}, offset: {offset}, flags: {flags}");
    // Vectored write at specific offset with optional flags.
    if offset < -1 {
        return Err(KError::InvalidInput);
    }
    if flags != 0 {
        return Err(KError::Unsupported);
    }
    let f = kprocess::current_files().get_file(fd)?;
    if offset != -1 {
        reject_positioned_stream_io(&f)?;
    }
    let iov = IoVectorBuf::from_iovecs(IoVec::load_from_user(iov, iovcnt)?)?;
    if offset == -1 {
        write_request_from_io(&f, iov.into_io()).map(|n| n as _)
    } else {
        write_file_at_from_io(&f, iov.into_io(), offset as _).map(|n| n as _)
    }
}

/// Transfers data to the destination's current position.
pub fn sys_sendfile(
    out_fd: c_int,
    in_fd: c_int,
    offset: UserPtr<u64>,
    len: usize,
) -> KResult<isize> {
    let resources = kprocess::current_files();
    let input = resources.get_file(in_fd)?;
    let output = resources.get_file(out_fd)?;
    let mut input_offset = if offset.is_null() {
        None
    } else {
        let position = offset.read_vm()?;
        // Retain the existing legacy offset limit.
        if position > u32::MAX as u64 {
            return Err(KError::InvalidInput);
        }
        Some(position)
    };
    let input_position = match input_offset.as_mut() {
        Some(offset) => TransferPosition::At(offset),
        None => TransferPosition::Managed,
    };
    let result = kvfs::transfer_files(
        &input,
        input_position,
        &output,
        TransferPosition::Managed,
        len,
        false,
    );
    // Linux sendfile copies back an explicit offset even after EOF or error.
    if let Some(position) = input_offset {
        offset.write_vm(position)?;
    }
    finish_restartable_io(result).map(|count| count as _)
}

/// Copies a file range with optional explicit source and destination offsets.
///
/// The checks (`generic_file_rw_checks`/`generic_copy_file_checks` parity),
/// the position protocol and the buffered copy live in
/// [`kvfs::copy_file_range`]; this wrapper only parses the ABI: non-zero
/// flags are rejected, and negative explicit offsets are rejected with
/// `EINVAL` the way Linux `rw_verify_area` does.
pub fn sys_copy_file_range(
    fd_in: c_int,
    off_in: UserPtr<i64>,
    fd_out: c_int,
    off_out: UserPtr<i64>,
    len: usize,
    flags: u32,
) -> KResult<isize> {
    if flags != 0 {
        return Err(KError::InvalidInput);
    }
    let resources = kprocess::current_files();
    let input = resources.get_file(fd_in)?;
    let output = resources.get_file(fd_out)?;
    let mut input_offset = if off_in.is_null() {
        None
    } else {
        let value = off_in.read_vm()?;
        if value < 0 {
            return Err(KError::InvalidInput);
        }
        Some(value as u64)
    };
    let mut output_offset = if off_out.is_null() {
        None
    } else {
        let value = off_out.read_vm()?;
        if value < 0 {
            return Err(KError::InvalidInput);
        }
        Some(value as u64)
    };
    let input_position = match input_offset.as_mut() {
        Some(offset) => TransferPosition::At(offset),
        None => TransferPosition::Managed,
    };
    let output_position = match output_offset.as_mut() {
        Some(offset) => TransferPosition::At(offset),
        None => TransferPosition::Managed,
    };
    let result =
        kvfs::copy_file_range(&input, input_position, &output, output_position, len, false);
    // Linux copy_file_range copies offsets out only after progress; both
    // write-backs are attempted before the first fault is reported.
    if matches!(result, Ok(count) if count > 0) {
        let output_copyout = if let Some(position) = output_offset {
            off_out.write_vm(position as i64)
        } else {
            Ok(())
        };
        let input_copyout = if let Some(position) = input_offset {
            off_in.write_vm(position as i64)
        } else {
            Ok(())
        };
        input_copyout?;
        output_copyout?;
    }
    finish_restartable_io(result).map(|count| count as _)
}

/// Transfers data between file descriptors, at least one of which is a pipe.
pub fn sys_splice(
    fd_in: c_int,
    off_in: UserPtr<i64>,
    fd_out: c_int,
    off_out: UserPtr<i64>,
    len: usize,
    flags: u32,
) -> KResult<isize> {
    let resources = kprocess::current_files();
    let input = resources.get_file(fd_in)?;
    let output = resources.get_file(fd_out)?;
    let mut input_offset = if off_in.is_null() {
        if input.is_path() {
            return Err(KError::InvalidInput);
        }
        None
    } else {
        let position = off_in.read_vm()?;
        if position < 0 {
            return Err(KError::InvalidInput);
        }
        Some(position as u64)
    };
    let mut output_offset = if off_out.is_null() {
        if output.flags().contains(OpenFlags::APPEND) {
            return Err(KError::InvalidInput);
        }
        output.write(b"")?;
        None
    } else {
        let position = off_out.read_vm()?;
        if position < 0 {
            return Err(KError::InvalidInput);
        }
        Some(position as u64)
    };
    let has_input_pipe = PipeObject::from_file(&input).is_ok();
    let has_output_pipe = PipeObject::from_file(&output).is_ok();
    if !has_input_pipe && !has_output_pipe {
        return Err(KError::InvalidInput);
    }
    let input_position = match input_offset.as_mut() {
        Some(offset) => TransferPosition::At(offset),
        None => TransferPosition::Managed,
    };
    let output_position = match output_offset.as_mut() {
        Some(offset) => TransferPosition::At(offset),
        None => TransferPosition::Managed,
    };
    let result = kvfs::transfer_files(
        &input,
        input_position,
        &output,
        output_position,
        len,
        flags & linux_raw_sys::general::SPLICE_F_NONBLOCK != 0,
    );
    // Linux splice copies out after any successful result, including EOF.
    // Destination copyout precedes source copyout, and the first fault wins.
    if result.is_ok() {
        if let Some(position) = output_offset {
            off_out.write_vm(position as i64)?;
        }
        if let Some(position) = input_offset {
            off_in.write_vm(position as i64)?;
        }
    }
    finish_restartable_io(result).map(|count| count as _)
}

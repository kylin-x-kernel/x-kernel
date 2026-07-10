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

use alloc::{sync::Arc, vec};
use core::ffi::{c_char, c_int};

use iov_iter::{IovSink, IovSource, iov_iter_dest, iov_iter_source};
use kcred::AccessIdKind;
use kerrno::{KError, KResult, LinuxError};
use kfd_objects::pipe::current_pipe_endpoint;
use kio::prelude::*;
use kpoll::IoEvents;
use kvfs::{
    FMode, Filename, LookupFlags, LookupIntent, OpenFlags, Permission, VfsFile, check_permission,
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

fn read_file_to_io(file: &VfsFile, dst: impl Write + IoBufMut + RewindBytes) -> KResult<usize> {
    let mut dst = IoSinkAdapter(dst);
    let mut iter = iov_iter_dest(&mut dst);
    file.read_iter(&mut iter)
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

fn write_file_from_io(file: &VfsFile, src: impl Read + IoBuf + RewindBytes) -> KResult<usize> {
    if file.is_stream() {
        let mut pos = 0;
        write_file_from_io_with_pos(file, src, &mut pos)
    } else {
        let mut pos = file.position_lock();
        write_file_from_io_with_pos(file, src, &mut pos)
    }
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

/// Creates a dummy file descriptor for unsupported syscalls.
pub fn sys_dummy_fd(sysno: Sysno) -> KResult<isize> {
    warn!("Unsupported syscall requested dummy fd: {sysno}");
    Err(KError::Unsupported)
}

/// Read data from the file indicated by `fd`.
///
/// Return the read size if success.
pub fn sys_read(fd: i32, buf: UserPtr<u8>, len: usize) -> KResult<isize> {
    debug!("sys_read <= fd: {fd}, buf: {:p}, len: {len}", buf.as_ptr());
    // Get the file object and perform the read operation into the user buffer
    let file = kprocess::current_resources().get_file(fd)?;
    Ok(read_file_to_io(&file, VmBytesMut::new(buf.as_ptr().cast_mut(), len))? as _)
}

/// Vectored read into multiple buffers.
pub fn sys_readv(fd: i32, iov: UserConstPtr<IoVec>, iovcnt: usize) -> KResult<isize> {
    debug!("sys_readv <= fd: {fd}, iovcnt: {iovcnt}");
    // Vectored read - read data into multiple buffers in a single operation
    let f = kprocess::current_resources().get_file(fd)?;
    let iov = IoVectorBuf::from_iovecs(IoVec::load_from_user(iov, iovcnt)?)?;
    read_file_to_io(&f, iov.into_io()).map(|n| n as _)
}

/// Write data to the file indicated by `fd`.
///
/// Return the written size if success.
pub fn sys_write(fd: i32, buf: UserConstPtr<u8>, len: usize) -> KResult<isize> {
    debug!("sys_write <= fd: {fd}, buf: {:p}, len: {len}", buf.as_ptr());
    let file = kprocess::current_resources().get_file(fd)?;
    Ok(write_file_from_io(&file, VmBytes::new(buf.as_ptr(), len))? as _)
}

/// Vectored write from multiple buffers.
pub fn sys_writev(fd: i32, iov: UserConstPtr<IoVec>, iovcnt: usize) -> KResult<isize> {
    debug!("sys_writev <= fd: {fd}, iovcnt: {iovcnt}");
    // Vectored write - write data from multiple buffers in a single operation
    let f = kprocess::current_resources().get_file(fd)?;
    let iov = IoVectorBuf::from_iovecs(IoVec::load_from_user(iov, iovcnt)?)?;
    write_file_from_io(&f, iov.into_io()).map(|n| n as _)
}

/// Repositions the read/write file offset.
pub fn sys_lseek(fd: c_int, offset: __kernel_off_t, whence: c_int) -> KResult<isize> {
    debug!("sys_lseek <= {fd} {offset} {whence}");
    let file = kprocess::current_resources().get_file(fd)?;
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
    let loc = Filename::new(path.as_str()).lookup_at(
        fs.root(),
        fs.pwd(),
        LookupIntent::Open,
        LookupFlags::follow(),
    )?;
    let metadata = loc.getattr()?;
    let credentials =
        kprocess::with_current_credentials(|creds| creds.access_snapshot(AccessIdKind::Filesystem));
    check_permission(&metadata, Permission::MAY_WRITE, &credentials)?;
    loc.truncate(length as _)?;
    Ok(0)
}

/// Truncates a file to a specified length by file descriptor.
pub fn sys_ftruncate(fd: c_int, length: __kernel_off_t) -> KResult<isize> {
    debug!("sys_ftruncate <= {fd} {length}");
    // Truncate file descriptor to specified length
    if length < 0 {
        return Err(KError::InvalidInput);
    }
    let f = kprocess::current_resources().get_file(fd)?;
    f.verify_mode(FMode::WRITE)?;
    f.path().truncate(length as _)?;
    Ok(0)
}

/// Preallocates disk space for a file.
pub fn sys_fallocate(
    fd: c_int,
    mode: u32,
    offset: __kernel_off_t,
    len: __kernel_off_t,
) -> KResult<isize> {
    debug!("sys_fallocate <= fd: {fd}, mode: {mode}, offset: {offset}, len: {len}");
    // Allocate/deallocate disk space for a file.
    // Supported modes:
    // - preallocate (mode 0, optionally KEEP_SIZE)
    // - PUNCH_HOLE (requires KEEP_SIZE)
    // - ZERO_RANGE (with/without KEEP_SIZE)
    // - COLLAPSE_RANGE (byte-range remove)
    // - INSERT_RANGE (byte-range insert zeroes)
    // - UNSHARE_RANGE (no-op on non-reflink backend)
    const FALLOC_FL_KEEP_SIZE: u32 = 0x01;
    const FALLOC_FL_PUNCH_HOLE: u32 = 0x02;
    const FALLOC_FL_COLLAPSE_RANGE: u32 = 0x08;
    const FALLOC_FL_ZERO_RANGE: u32 = 0x10;
    const FALLOC_FL_INSERT_RANGE: u32 = 0x20;
    const FALLOC_FL_UNSHARE_RANGE: u32 = 0x40;

    if offset < 0 || len < 0 {
        return Err(KError::InvalidInput);
    }
    if len == 0 {
        return Ok(0);
    }

    let keep_size = (mode & FALLOC_FL_KEEP_SIZE) != 0;
    let base_mode = mode & !FALLOC_FL_KEEP_SIZE;

    let f = kprocess::current_resources().get_file(fd)?;
    f.verify_mode(FMode::WRITE)?;

    let start = offset as u64;
    let len_u = len as u64;
    let end = start.checked_add(len_u).ok_or(KError::InvalidInput)?;
    let old_size = f.size();

    match base_mode {
        // Standard preallocation behavior in our current implementation.
        0 => {
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
        }
        // Emulate punch hole by zeroing existing file range.
        FALLOC_FL_PUNCH_HOLE => {
            // PUNCH_HOLE requires KEEP_SIZE.
            if !keep_size {
                return Err(KError::InvalidInput);
            }
            let target_end = end.min(old_size);
            if target_end <= start {
                return Ok(0);
            }

            write_zeros_range(&f, start, target_end)?;
        }
        // Emulate zero-range by writing zeros to the range.
        FALLOC_FL_ZERO_RANGE => {
            // KEEP_SIZE: only affect visible bytes in current file size.
            // Avoid temporary i_size growth, which can perturb extent metadata
            // on backends with incomplete hole/truncate handling.
            let target_end = if keep_size { end.min(old_size) } else { end };
            if target_end <= start {
                return Ok(0);
            }

            if !keep_size && target_end > old_size {
                f.path().truncate(target_end)?;
            }

            write_zeros_range(&f, start, target_end)?;
        }
        // On non-reflink filesystems, emulate unshare by preallocating range semantics.
        FALLOC_FL_UNSHARE_RANGE => {
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
        }
        // Remove [start, end) and shift following bytes left.
        FALLOC_FL_COLLAPSE_RANGE => {
            // COLLAPSE_RANGE does not allow KEEP_SIZE.
            if keep_size {
                return Err(KError::InvalidInput);
            }
            if end > old_size {
                return Err(KError::InvalidInput);
            }
            if start >= old_size {
                return Err(KError::InvalidInput);
            }

            let removed = end - start;
            if removed == 0 {
                return Ok(0);
            }
            f.fallocate(base_mode, start, len_u)?;
        }
        // Insert zero-filled [start, start+len) and shift tail right.
        FALLOC_FL_INSERT_RANGE => {
            // INSERT_RANGE does not allow KEEP_SIZE.
            if keep_size {
                return Err(KError::InvalidInput);
            }
            if start > old_size {
                return Err(KError::InvalidInput);
            }

            let insert_len = len_u;
            if insert_len == 0 {
                return Ok(0);
            }
            f.fallocate(base_mode, start, len_u)?;
        }
        // Other mode combinations are not supported.
        _ => return Err(KError::Unsupported),
    }

    Ok(0)
}

/// Synchronizes a file's in-core state with storage.
pub fn sys_fsync(fd: c_int) -> KResult<isize> {
    debug!("sys_fsync <= {fd}");
    // Synchronize file to disk - syncs both data and metadata
    let file = kprocess::current_resources().get_file(fd)?;
    file.fsync(false)?;
    Ok(0)
}

/// Synchronizes a file's data (not metadata) with storage.
pub fn sys_fdatasync(fd: c_int) -> KResult<isize> {
    debug!("sys_fdatasync <= {fd}");
    // Synchronize file data to disk - only syncs data, not metadata
    let file = kprocess::current_resources().get_file(fd)?;
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
    if current_pipe_endpoint(fd).is_ok() {
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
    let f = kprocess::current_resources().get_file(fd)?;
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
    let f = kprocess::current_resources().get_file(fd)?;
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
    let f = kprocess::current_resources().get_file(fd)?;
    if offset != -1 {
        reject_positioned_stream_io(&f)?;
    }
    let iov = IoVectorBuf::from_iovecs(IoVec::load_from_user(iov, iovcnt)?)?;
    if offset == -1 {
        read_file_to_io(&f, iov.into_io()).map(|n| n as _)
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
    let f = kprocess::current_resources().get_file(fd)?;
    if offset != -1 {
        reject_positioned_stream_io(&f)?;
    }
    let iov = IoVectorBuf::from_iovecs(IoVec::load_from_user(iov, iovcnt)?)?;
    if offset == -1 {
        write_file_from_io(&f, iov.into_io()).map(|n| n as _)
    } else {
        write_file_at_from_io(&f, iov.into_io(), offset as _).map(|n| n as _)
    }
}

/// Helper for sendfile and copy_file_range operations
/// Abstracts both fixed position (via offset pointer) and current position reads/writes
enum SendFile {
    Direct(Arc<VfsFile>),
    Offset(Arc<VfsFile>, UserPtr<u64>),
}

impl SendFile {
    /// Check if data is available for reading
    fn has_data(&self) -> bool {
        match self {
            SendFile::Direct(file) => file.poll(),
            SendFile::Offset(file, ..) => file.poll(),
        }
        .contains(IoEvents::IN)
    }

    /// Read from this file, either at current position or from fixed offset
    fn read(&mut self, buf: &mut [u8]) -> KResult<usize> {
        match self {
            SendFile::Direct(file) => file.read(buf),
            SendFile::Offset(file, offset) => {
                reject_positioned_stream_io(file)?;
                let mut off = offset.read_vm()?;
                let bytes_read = file.read_from(buf, &mut off)?;
                offset.write_vm(off)?;
                Ok(bytes_read)
            }
        }
    }

    /// Write to this file, either at current position or to fixed offset
    fn write(&mut self, buf: &[u8]) -> KResult<usize> {
        match self {
            SendFile::Direct(file) => file.write(buf),
            SendFile::Offset(file, offset) => {
                reject_positioned_stream_io(file)?;
                let mut off = offset.read_vm()?;
                let bytes_written = file.write_from(buf, &mut off)?;
                offset.write_vm(off)?;
                Ok(bytes_written)
            }
        }
    }
}

/// Core implementation for sendfile/splice/copy_file_range
/// Copies data from source to destination with buffering
fn do_send(mut src: SendFile, mut dst: SendFile, len: usize) -> KResult<usize> {
    let mut buf = vec![0; 0x1000]; // 4KB intermediate buffer
    let mut total_written = 0;
    let mut remaining = len;

    while remaining > 0 {
        // After first successful write, stop if no more data available
        if total_written > 0 && !src.has_data() {
            break;
        }
        let to_read = buf.len().min(remaining);
        // Try to read - WouldBlock is acceptable if we've already written some data
        let bytes_read = match src.read(&mut buf[..to_read]) {
            Ok(n) => n,
            Err(KError::WouldBlock) if total_written > 0 => break,
            Err(e) => return Err(e),
        };
        if bytes_read == 0 {
            break; // EOF reached
        }

        // Write the data to destination
        let bytes_written = dst.write(&buf[..bytes_read])?;
        if bytes_written < bytes_read {
            break; // Destination full or error
        }

        total_written += bytes_written;
        remaining -= bytes_written;
    }

    Ok(total_written)
}

/// Efficiently transfer data from in_fd to out_fd without going through user space
/// Transfers data from one file descriptor to another.
pub fn sys_sendfile(
    out_fd: c_int,
    in_fd: c_int,
    offset: UserPtr<u64>,
    len: usize,
) -> KResult<isize> {
    debug!(
        "sys_sendfile <= out_fd: {}, in_fd: {}, offset: {}, len: {}",
        out_fd,
        in_fd,
        !offset.is_null(),
        len
    );

    // Source can use fixed offset or current file position
    let resources = kprocess::current_resources();
    let src = if !offset.is_null() {
        // Check offset fits in 32-bit range (legacy syscall limitation)
        if offset.read_vm()? > u32::MAX as u64 {
            return Err(KError::InvalidInput);
        }
        SendFile::Offset(resources.get_file(in_fd)?, offset)
    } else {
        SendFile::Direct(resources.get_file(in_fd)?)
    };

    // Destination always uses current file position
    let dst = SendFile::Direct(resources.get_file(out_fd)?);

    do_send(src, dst, len).map(|n| n as _)
}

/// Copy data from one file to another, both with optional fixed offsets
/// Copies a range of bytes between two file descriptors.
pub fn sys_copy_file_range(
    fd_in: c_int,
    off_in: UserPtr<u64>,
    fd_out: c_int,
    off_out: UserPtr<u64>,
    len: usize,
    flags: u32,
) -> KResult<isize> {
    debug!(
        "sys_copy_file_range <= fd_in: {}, off_in: {}, fd_out: {}, off_out: {}, len: {}, flags: {}",
        fd_in,
        !off_in.is_null(),
        fd_out,
        !off_out.is_null(),
        len,
        flags
    );

    if flags != 0 {
        return Err(KError::InvalidInput);
    }
    // TODO: check both regular files
    // TODO: check same file and overlap

    // Source can use fixed offset or current file position
    let resources = kprocess::current_resources();
    let src = if !off_in.is_null() {
        SendFile::Offset(resources.get_file(fd_in)?, off_in)
    } else {
        SendFile::Direct(resources.get_file(fd_in)?)
    };

    // Destination can also use fixed offset or current file position
    let dst = if !off_out.is_null() {
        SendFile::Offset(resources.get_file(fd_out)?, off_out)
    } else {
        SendFile::Direct(resources.get_file(fd_out)?)
    };

    do_send(src, dst, len).map(|n| n as _)
}

/// Move data between file descriptors, with at least one being a pipe
/// Splice can connect pipes to regular files or between pipes without user-space buffering
pub fn sys_splice(
    fd_in: c_int,
    off_in: UserPtr<i64>,
    fd_out: c_int,
    off_out: UserPtr<i64>,
    len: usize,
    _flags: u32,
) -> KResult<isize> {
    debug!(
        "sys_splice <= fd_in: {}, off_in: {}, fd_out: {}, off_out: {}, len: {}, flags: {}",
        fd_in,
        !off_in.is_null(),
        fd_out,
        !off_out.is_null(),
        len,
        _flags
    );

    // Track if we have a pipe - at least one must be present for splice
    let mut has_pipe = false;

    let resources = kprocess::current_resources();

    // Setup source: either with fixed offset or using current position
    let src = if !off_in.is_null() {
        // Fixed offset must be non-negative
        if off_in.read_vm()? < 0 {
            return Err(KError::InvalidInput);
        }
        SendFile::Offset(resources.get_file(fd_in)?, off_in.cast())
    } else {
        // Try to use as pipe first
        if let Ok(src) = current_pipe_endpoint(fd_in) {
            if !src.is_read() {
                return Err(KError::BadFileDescriptor);
            }
            has_pipe = true;
        }
        // Path-only files (opened without O_RDWR/O_WRONLY) cannot be spliced
        let file = resources.get_file(fd_in)?;
        if file.is_path() {
            return Err(KError::InvalidInput);
        }
        SendFile::Direct(file)
    };

    // Setup destination: either with fixed offset or using current position
    let dst = if !off_out.is_null() {
        // Fixed offset must be non-negative
        if off_out.read_vm()? < 0 {
            return Err(KError::InvalidInput);
        }
        SendFile::Offset(resources.get_file(fd_out)?, off_out.cast())
    } else {
        // Try to use as pipe first
        if let Ok(dst) = current_pipe_endpoint(fd_out) {
            if !dst.is_write() {
                return Err(KError::BadFileDescriptor);
            }
            has_pipe = true;
        }
        // APPEND mode files cannot be spliced (offset cannot be changed)
        let f = resources.get_file(fd_out)?;
        if f.flags().contains(OpenFlags::APPEND) {
            return Err(KError::InvalidInput);
        }
        // Verify destination is writable with a write probe
        f.write(b"")?;
        SendFile::Direct(f)
    };

    // At least one of source or destination must be a pipe
    if !has_pipe {
        return Err(KError::InvalidInput);
    }

    do_send(src, dst, len).map(|n| n as _)
}

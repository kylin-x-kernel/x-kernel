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
use kvfs::{FMode, Filename, LookupFlags, LookupIntent, OpenFlags, VfsFile, pipe::PipeObject};
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
    let file = kprocess::current_resources().get_file(fd)?;
    read_request_to_io(&file, VmBytesMut::new(buf.as_ptr().cast_mut(), len))
        .map(|count| count as isize)
}

/// Vectored read into multiple buffers.
pub fn sys_readv(fd: i32, iov: UserConstPtr<IoVec>, iovcnt: usize) -> KResult<isize> {
    debug!("sys_readv <= fd: {fd}, iovcnt: {iovcnt}");
    // Vectored read - read data into multiple buffers in a single operation
    let f = kprocess::current_resources().get_file(fd)?;
    let iov = IoVectorBuf::from_iovecs(IoVec::load_from_user(iov, iovcnt)?)?;
    read_request_to_io(&f, iov.into_io()).map(|count| count as isize)
}

/// Write data to the file indicated by `fd`.
///
/// Return the written size if success.
pub fn sys_write(fd: i32, buf: UserConstPtr<u8>, len: usize) -> KResult<isize> {
    debug!("sys_write <= fd: {fd}, buf: {:p}, len: {len}", buf.as_ptr());
    let file = kprocess::current_resources().get_file(fd)?;
    write_request_from_io(&file, VmBytes::new(buf.as_ptr(), len)).map(|count| count as isize)
}

/// Vectored write from multiple buffers.
pub fn sys_writev(fd: i32, iov: UserConstPtr<IoVec>, iovcnt: usize) -> KResult<isize> {
    debug!("sys_writev <= fd: {fd}, iovcnt: {iovcnt}");
    // Vectored write - write data from multiple buffers in a single operation
    let f = kprocess::current_resources().get_file(fd)?;
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

#[cfg(unittest)]
mod restart_tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use kvfs::{FMode, FileOperations, OpenFlags};

    use super::*;

    struct TransferOperations {
        fail_after: usize,
        max_write: usize,
        calls: AtomicUsize,
        transferred: AtomicUsize,
    }

    impl FileOperations for TransferOperations {
        fn supports_read(&self) -> bool {
            true
        }

        fn supports_write(&self) -> bool {
            true
        }

        fn read(&self, _: &VfsFile, bytes: &mut [u8], _: u64) -> KResult<usize> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            bytes.fill(0x45);
            Ok(bytes.len())
        }

        fn write(&self, _: &VfsFile, bytes: &[u8], _: u64) -> KResult<usize> {
            if self.calls.fetch_add(1, Ordering::Relaxed) == self.fail_after {
                return Err(KError::IoWaitInterrupted);
            }
            assert!(bytes.iter().all(|byte| *byte == 0x45));
            let written = bytes.len().min(self.max_write);
            self.transferred.fetch_add(written, Ordering::Relaxed);
            Ok(written)
        }
    }

    #[unittest::def_test]
    fn aggregate_transfer_does_not_promote_leaf_interruption_after_output() {
        let base = memfs::shmem::create_memfd_file("restart-test", false, kcred::initial_cred())
            .unwrap()
            .into_file(kcred::initial_cred())
            .unwrap();
        for completed_blocks in [0, 1] {
            let source = Arc::new(TransferOperations {
                fail_after: usize::MAX,
                max_write: usize::MAX,
                calls: AtomicUsize::new(0),
                transferred: AtomicUsize::new(0),
            });
            let destination = Arc::new(TransferOperations {
                fail_after: completed_blocks,
                max_write: usize::MAX,
                calls: AtomicUsize::new(0),
                transferred: AtomicUsize::new(0),
            });
            let input = base
                .alloc_clone(FMode::READ, OpenFlags::empty(), source.clone())
                .unwrap();
            let output = base
                .alloc_clone(FMode::WRITE, OpenFlags::empty(), destination.clone())
                .unwrap();
            let result = finish_restartable_io(kvfs::transfer_files(
                &input, None, &output, None, 8192, false,
            ));
            assert_eq!(
                result,
                if completed_blocks == 0 {
                    Err(LinuxError::ERESTARTSYS.into())
                } else {
                    Ok(completed_blocks * 4096)
                }
            );
            assert_eq!(source.calls.load(Ordering::Relaxed), completed_blocks + 1);
            assert_eq!(*input.position_lock(), completed_blocks as u64 * 4096);
            assert_eq!(*output.position_lock(), completed_blocks as u64 * 4096);
            assert_eq!(
                destination.calls.load(Ordering::Relaxed),
                completed_blocks + 1
            );
            assert_eq!(
                destination.transferred.load(Ordering::Relaxed),
                completed_blocks * 4096
            );
        }
    }

    #[unittest::def_test]
    fn aggregate_short_write_commits_only_accepted_bytes() {
        let base = memfs::shmem::create_memfd_file("short-copy", false, kcred::initial_cred())
            .unwrap()
            .into_file(kcred::initial_cred())
            .unwrap();
        let operations = Arc::new(TransferOperations {
            fail_after: usize::MAX,
            max_write: 123,
            calls: AtomicUsize::new(0),
            transferred: AtomicUsize::new(0),
        });
        let input = base
            .alloc_clone(FMode::READ, OpenFlags::empty(), operations.clone())
            .unwrap();
        let output = base
            .alloc_clone(FMode::WRITE, OpenFlags::empty(), operations.clone())
            .unwrap();
        assert_eq!(
            finish_restartable_io(kvfs::transfer_files(
                &input, None, &output, None, 8192, false,
            )),
            Ok(123)
        );
        assert_eq!(input.position(), 123);
        assert_eq!(output.position(), 123);
        assert_eq!(operations.transferred.load(Ordering::Relaxed), 123);
    }

    #[unittest::def_test]
    fn only_file_wait_marker_requests_restart() {
        assert_eq!(
            finish_restartable_io(Err(KError::IoWaitInterrupted)),
            Err(LinuxError::ERESTARTSYS.into())
        );
        for error in [
            KError::Interrupted,
            KError::WouldBlock,
            KError::NoMemory,
            KError::InvalidInput,
        ] {
            assert_eq!(finish_restartable_io(Err(error)), Err(error));
        }
        for count in [0, 1, 4096] {
            assert_eq!(finish_restartable_io(Ok(count)), Ok(count));
        }
    }
}

#[cfg(unittest)]
mod transfer_count_tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use iov_iter::{IovIterDest, IovIterSource};
    use kvfs::{FileOperations, Kiocb, transfer_files};

    use super::*;

    struct TransferBackend {
        invalid_call: usize,
        invalid_count: usize,
        calls: AtomicUsize,
    }

    impl TransferBackend {
        fn complete(&self, iocb: &mut Kiocb<'_>, count: usize) -> KResult<usize> {
            let reported = if self.calls.fetch_add(1, Ordering::Relaxed) == self.invalid_call {
                self.invalid_count
            } else {
                count
            };
            iocb.advance(reported);
            Ok(reported)
        }
    }

    impl FileOperations for TransferBackend {
        fn supports_read(&self) -> bool {
            true
        }

        fn supports_write(&self) -> bool {
            true
        }

        fn read_iter(&self, iocb: &mut Kiocb<'_>, iter: &mut IovIterDest<'_>) -> KResult<usize> {
            self.complete(iocb, iter.count())
        }

        fn write_iter(&self, iocb: &mut Kiocb<'_>, iter: &mut IovIterSource<'_>) -> KResult<usize> {
            self.complete(iocb, iter.count())
        }
    }

    fn transfer_file(backend: Arc<TransferBackend>, mode: FMode) -> Arc<VfsFile> {
        let base =
            memfs::shmem::create_memfd_file("transfer-count-test", false, kcred::initial_cred())
                .unwrap()
                .into_file(kcred::initial_cred())
                .unwrap();
        base.alloc_clone(mode, OpenFlags::empty(), backend).unwrap()
    }

    #[unittest::def_test]
    fn oversized_backend_counts_preserve_only_valid_transfer_progress() {
        for is_read_fault in [true, false] {
            for completed_blocks in [0, 1] {
                for invalid_count in [4097, usize::MAX] {
                    for has_explicit_offsets in [false, true] {
                        let source = Arc::new(TransferBackend {
                            invalid_call: if is_read_fault {
                                completed_blocks
                            } else {
                                usize::MAX
                            },
                            invalid_count,
                            calls: AtomicUsize::new(0),
                        });
                        let destination = Arc::new(TransferBackend {
                            invalid_call: if is_read_fault {
                                usize::MAX
                            } else {
                                completed_blocks
                            },
                            invalid_count,
                            calls: AtomicUsize::new(0),
                        });
                        let input = transfer_file(source.clone(), FMode::READ);
                        let output = transfer_file(destination.clone(), FMode::WRITE);
                        let mut input_offset = 0;
                        let mut output_offset = 0;
                        let result = transfer_files(
                            &input,
                            has_explicit_offsets.then_some(&mut input_offset),
                            &output,
                            has_explicit_offsets.then_some(&mut output_offset),
                            8192,
                            false,
                        );
                        let completed = completed_blocks * 4096;
                        assert_eq!(
                            result,
                            if completed == 0 {
                                Err(KError::Io)
                            } else {
                                Ok(completed)
                            }
                        );
                        let expected_position = if has_explicit_offsets {
                            0
                        } else {
                            completed as u64
                        };
                        assert_eq!(input.position(), expected_position);
                        assert_eq!(output.position(), expected_position);
                        let expected_offset = if has_explicit_offsets {
                            completed as u64
                        } else {
                            0
                        };
                        assert_eq!(input_offset, expected_offset);
                        assert_eq!(output_offset, expected_offset);
                        assert_eq!(source.calls.load(Ordering::Relaxed), completed_blocks + 1);
                        assert_eq!(
                            destination.calls.load(Ordering::Relaxed),
                            completed_blocks + usize::from(!is_read_fault)
                        );
                    }
                }
            }
        }
    }
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
    let f = kprocess::current_resources().get_file(fd)?;
    f.truncate(length as _)?;
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
                f.truncate(target_end)?;
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

/// Synchronizes file data and the metadata needed to retrieve it with storage.
pub fn sys_fdatasync(fd: c_int) -> KResult<isize> {
    debug!("sys_fdatasync <= {fd}");
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
    let file = kprocess::current_resources().get_file(fd)?;
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
    let f = kprocess::current_resources().get_file(fd)?;
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
    let resources = kprocess::current_resources();
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
    let result = kvfs::transfer_files(&input, input_offset.as_mut(), &output, None, len, false);
    // Linux sendfile copies back an explicit offset even after EOF or error.
    if let Some(position) = input_offset {
        offset.write_vm(position)?;
    }
    finish_restartable_io(result).map(|count| count as _)
}

/// Copies a file range with optional explicit source and destination offsets.
pub fn sys_copy_file_range(
    fd_in: c_int,
    off_in: UserPtr<u64>,
    fd_out: c_int,
    off_out: UserPtr<u64>,
    len: usize,
    flags: u32,
) -> KResult<isize> {
    if flags != 0 {
        return Err(KError::InvalidInput);
    }
    // TODO: check both regular files, same-file overlap and filesystem support.
    let resources = kprocess::current_resources();
    let input = resources.get_file(fd_in)?;
    let output = resources.get_file(fd_out)?;
    let mut input_offset = if off_in.is_null() {
        None
    } else {
        Some(off_in.read_vm()?)
    };
    let mut output_offset = if off_out.is_null() {
        None
    } else {
        Some(off_out.read_vm()?)
    };
    let result = kvfs::transfer_files(
        &input,
        input_offset.as_mut(),
        &output,
        output_offset.as_mut(),
        len,
        false,
    );
    // Unlike splice, copy_file_range copies offsets out only after progress.
    // Both offsets record committed output even if one copyout faults.
    if matches!(result, Ok(count) if count > 0) {
        let input_copyout = input_offset.map_or(Ok(()), |position| off_in.write_vm(position));
        let output_copyout = output_offset.map_or(Ok(()), |position| off_out.write_vm(position));
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
    let resources = kprocess::current_resources();
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
    let result = kvfs::transfer_files(
        &input,
        input_offset.as_mut(),
        &output,
        output_offset.as_mut(),
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

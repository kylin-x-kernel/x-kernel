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
    FMode, Filename, LookupFlags, LookupIntent, OpenFlags, TransferPosition, VfsFile,
    pipe::PipeObject,
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

#[unittest::mod_test]
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
                &input,
                TransferPosition::Managed,
                &output,
                TransferPosition::Managed,
                8192,
                false,
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
                &input,
                TransferPosition::Managed,
                &output,
                TransferPosition::Managed,
                8192,
                false,
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

#[unittest::mod_test]
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
                        let input_position = if has_explicit_offsets {
                            TransferPosition::At(&mut input_offset)
                        } else {
                            TransferPosition::Managed
                        };
                        let output_position = if has_explicit_offsets {
                            TransferPosition::At(&mut output_offset)
                        } else {
                            TransferPosition::Managed
                        };
                        let result = transfer_files(
                            &input,
                            input_position,
                            &output,
                            output_position,
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

    /// A managed output side takes its copy origin from `f_pos`, not from
    /// zero. Pinning the same-description mixed-direction fix: with `f_pos`
    /// at 4096 the ranges no longer overlap, whereas the earlier anchor-at-
    /// zero behavior rejected the request with `EINVAL`.
    #[unittest::def_test]
    fn same_file_managed_output_uses_f_pos_as_copy_origin() {
        let base = memfs::shmem::create_memfd_file("same-file-copy", false, kcred::initial_cred())
            .unwrap()
            .into_file(kcred::initial_cred())
            .unwrap();
        let pattern: [u8; 8192] = core::array::from_fn(|index| (index % 251) as u8);
        let mut position = 0;
        assert_eq!(base.write_from(&pattern, &mut position), Ok(8192));

        base.set_position(4096);
        let mut input_offset = 0;
        assert_eq!(
            kvfs::copy_file_range(
                &base,
                TransferPosition::At(&mut input_offset),
                &base,
                TransferPosition::Managed,
                4096,
                false,
            ),
            Ok(4096)
        );
        assert_eq!(input_offset, 4096);
        assert_eq!(base.position(), 8192);
        let mut copied = [0; 4096];
        let mut read_position = 4096;
        assert_eq!(base.read_from(&mut copied, &mut read_position), Ok(4096));
        assert_eq!(copied, pattern[..4096]);
    }
}

/// Boundary checks and settlement semantics of the `copy_file_range(2)`
/// engine, mirroring Linux `generic_copy_file_checks` (`fs/read_write.c`).
#[unittest::mod_test]
mod copy_file_range_checks_tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use iov_iter::{IovIterDest, IovIterSource};
    use kvfs::{FileOperations, Kiocb, MAX_LFS_FILESIZE, SuperBlockFlags};
    use linux_raw_sys::general::{O_APPEND, O_RDONLY, O_RDWR};

    use super::*;

    /// Opens a memfd-backed regular file with the memfs operation table.
    fn memfd_file(name: &str) -> Arc<VfsFile> {
        memfs::shmem::create_memfd_file(name, false, kcred::initial_cred())
            .unwrap()
            .into_file(kcred::initial_cred())
            .unwrap()
    }

    fn fill(file: &Arc<VfsFile>, len: usize) {
        let pattern: alloc::vec::Vec<u8> = (0..len).map(|index| (index % 251) as u8).collect();
        let mut position = 0;
        assert_eq!(file.write_from(&pattern, &mut position), Ok(len));
    }

    /// Instrumented backend whose `poisoned_call`-th write fails, modelling
    /// a destination that runs out of space mid-copy. The table carries a
    /// native chunk primitive (read the input, write through this table) so
    /// pairs ride the native path and the poison lands in it.
    struct CountingBackend {
        poisoned_call: usize,
        calls: AtomicUsize,
    }

    impl FileOperations for CountingBackend {
        fn supports_read(&self) -> bool {
            true
        }

        fn supports_write(&self) -> bool {
            true
        }

        fn copy_chunk_from(
            &self,
            input: &VfsFile,
            input_pos: u64,
            output: &VfsFile,
            output_pos: u64,
            len: usize,
        ) -> Option<KResult<usize>> {
            Some((|| {
                let mut buf = alloc::vec![0u8; len];
                let mut read_pos = input_pos;
                let read = input.read_from(&mut buf, &mut read_pos)?;
                if read == 0 {
                    return Ok(0);
                }
                let mut write_pos = output_pos;
                output.write_from(&buf[..read], &mut write_pos)
            })())
        }

        fn read_iter(&self, iocb: &mut Kiocb<'_>, iter: &mut IovIterDest<'_>) -> KResult<usize> {
            let count = iter.count();
            iocb.advance(count);
            Ok(count)
        }

        fn write_iter(&self, iocb: &mut Kiocb<'_>, iter: &mut IovIterSource<'_>) -> KResult<usize> {
            let count = iter.count();
            if self.calls.fetch_add(1, Ordering::Relaxed) == self.poisoned_call {
                return Err(KError::StorageFull);
            }
            iocb.advance(count);
            Ok(count)
        }
    }

    /// Linux `vfs_copy_file_range` allows partial success: an error after
    /// progress returns the completed prefix, and the published positions
    /// must describe the bytes the destination actually accepted.
    #[unittest::def_test]
    fn copy_error_after_progress_returns_completed_prefix() {
        let source = Arc::new(CountingBackend {
            poisoned_call: usize::MAX,
            calls: AtomicUsize::new(0),
        });
        let destination = Arc::new(CountingBackend {
            poisoned_call: 1,
            calls: AtomicUsize::new(0),
        });
        let source_base = memfd_file("copy-partial-src");
        fill(&source_base, 8192);
        let input = source_base
            .alloc_clone(FMode::READ, OpenFlags::empty(), source)
            .unwrap();
        let output = memfd_file("copy-partial-dst")
            .alloc_clone(FMode::WRITE, OpenFlags::empty(), destination)
            .unwrap();
        // Two 4 KiB chunks requested: the first lands, the second write
        // fails with ENOSPC.
        let mut input_offset = 0;
        let mut output_offset = 0;
        assert_eq!(
            kvfs::copy_file_range(
                &input,
                TransferPosition::At(&mut input_offset),
                &output,
                TransferPosition::At(&mut output_offset),
                8192,
                false,
            ),
            Ok(4096)
        );
        assert_eq!(input_offset, 4096);
        assert_eq!(output_offset, 4096);
        assert_eq!(input.position(), 0);
        assert_eq!(output.position(), 0);
    }

    /// A backend without a native chunk primitive: its pairs can only ride
    /// the generic buffered pump, and only within one super block.
    struct PrimitivelessBackend;

    impl FileOperations for PrimitivelessBackend {
        fn supports_read(&self) -> bool {
            true
        }

        fn supports_write(&self) -> bool {
            true
        }

        fn read_iter(&self, iocb: &mut Kiocb<'_>, iter: &mut IovIterDest<'_>) -> KResult<usize> {
            let count = iter.count();
            iocb.advance(count);
            Ok(count)
        }

        fn write_iter(&self, iocb: &mut Kiocb<'_>, iter: &mut IovIterSource<'_>) -> KResult<usize> {
            let count = iter.count();
            iocb.advance(count);
            Ok(count)
        }
    }

    /// A native table that refuses the input: its empty-range admission
    /// query answers `Some(Err(..))`, modelling memfs refusing non-memfs
    /// storage.
    struct RefusingBackend;

    impl FileOperations for RefusingBackend {
        fn copy_chunk_from(
            &self,
            input: &VfsFile,
            input_pos: u64,
            output: &VfsFile,
            output_pos: u64,
            len: usize,
        ) -> Option<KResult<usize>> {
            let _ = (input, input_pos, output, output_pos, len);
            Some(Err(KError::CrossesDevices))
        }
    }

    /// Linux shortens an oversized request at the source EOF; an unsigned
    /// range wrap is the only `EOVERFLOW`, so a request beyond `off_t`
    /// against a short source still copies what exists.
    #[unittest::def_test]
    fn oversized_request_shortens_at_source_eof() {
        let input = memfd_file("copy-shorten-src");
        fill(&input, 8192);
        let output = memfd_file("copy-shorten-dst");
        let mut input_offset = 0;
        let mut output_offset = 0;
        assert_eq!(
            kvfs::copy_file_range(
                &input,
                TransferPosition::At(&mut input_offset),
                &output,
                TransferPosition::At(&mut output_offset),
                (i64::MAX as u64 + 1) as usize,
                false,
            ),
            Ok(8192)
        );
        assert_eq!(input_offset, 8192);
        assert_eq!(output_offset, 8192);
    }

    /// The destination ceiling applies in the checks phase: a start at or
    /// past the maximum file size is `EFBIG` even for an empty source, and
    /// a copy crossing the ceiling is clamped to the tail room.
    #[unittest::def_test]
    fn destination_ceiling_checks_before_copying() {
        let source = memfd_file("copy-ceiling-src");
        let output = memfd_file("copy-ceiling-dst");
        let mut input_offset = 0;
        let mut output_offset = MAX_LFS_FILESIZE;
        assert_eq!(
            kvfs::copy_file_range(
                &source,
                TransferPosition::At(&mut input_offset),
                &output,
                TransferPosition::At(&mut output_offset),
                10,
                false,
            ),
            Err(KError::from(LinuxError::EFBIG))
        );

        let source = memfd_file("copy-clamp-src");
        fill(&source, 8192);
        let output = memfd_file("copy-clamp-dst");
        let mut input_offset = 0;
        let mut output_offset = MAX_LFS_FILESIZE - 4;
        assert_eq!(
            kvfs::copy_file_range(
                &source,
                TransferPosition::At(&mut input_offset),
                &output,
                TransferPosition::At(&mut output_offset),
                8192,
                false,
            ),
            Ok(4)
        );
        assert_eq!(input_offset, 4);
        assert_eq!(output_offset, MAX_LFS_FILESIZE);
    }

    /// Directory descriptors are rejected with `EISDIR` on either side.
    #[unittest::def_test]
    fn directory_sides_are_rejected_with_eisdir() {
        let fs = memfs::shmem::new_tmpfs(SuperBlockFlags::empty());
        let root = kvfs::Path::new(kvfs::Mount::new_root(&fs), fs.root_dir());
        let dir = kvfs::dentry_open(root, O_RDONLY, kcred::initial_cred()).unwrap();
        let input = memfd_file("copy-eisdir-src");
        fill(&input, 16);
        let mut offset = 0;
        assert_eq!(
            kvfs::copy_file_range(
                &dir,
                TransferPosition::At(&mut offset),
                &input,
                TransferPosition::Managed,
                16,
                false,
            ),
            Err(KError::from(LinuxError::EISDIR))
        );
        // A directory can only be opened for reading, so the output-side
        // directory case never reaches the type check: the access-mode
        // verification (Linux `rw_verify_area`) reports `EBADF` first.
        let mut offset = 0;
        assert_eq!(
            kvfs::copy_file_range(
                &input,
                TransferPosition::At(&mut offset),
                &dir,
                TransferPosition::Managed,
                16,
                false,
            ),
            Err(KError::BadFileDescriptor)
        );
    }

    /// A destination opened `O_APPEND` is rejected with `EBADF`: the copy
    /// would land at the wrong offset.
    #[unittest::def_test]
    fn append_destination_is_rejected_with_ebadf() {
        let source = memfd_file("copy-append-src");
        fill(&source, 16);
        let target =
            memfs::shmem::create_memfd_file("copy-append-dst", false, kcred::initial_cred())
                .unwrap();
        let output =
            kvfs::dentry_open(target.into_path(), O_RDWR | O_APPEND, kcred::initial_cred())
                .unwrap();
        let mut input_offset = 0;
        let mut output_offset = 0;
        assert_eq!(
            kvfs::copy_file_range(
                &source,
                TransferPosition::At(&mut input_offset),
                &output,
                TransferPosition::At(&mut output_offset),
                16,
                false,
            ),
            Err(KError::from(LinuxError::EBADF))
        );
    }

    /// Overlapping ranges within one inode are rejected so the chunked copy
    /// can never observe its own writes; a zero-length request never
    /// overlaps and stays a no-op success.
    #[unittest::def_test]
    fn overlapping_same_inode_ranges_are_rejected() {
        let file = memfd_file("copy-overlap");
        fill(&file, 8192);
        let mut input_offset = 0;
        let mut output_offset = 4096;
        assert_eq!(
            kvfs::copy_file_range(
                &file,
                TransferPosition::At(&mut input_offset),
                &file,
                TransferPosition::At(&mut output_offset),
                8192,
                false,
            ),
            Err(KError::from(LinuxError::EINVAL))
        );
        let mut input_offset = 4096;
        let mut output_offset = 0;
        assert_eq!(
            kvfs::copy_file_range(
                &file,
                TransferPosition::At(&mut input_offset),
                &file,
                TransferPosition::At(&mut output_offset),
                0,
                false,
            ),
            Ok(0)
        );
    }

    /// A start at or past EOF is a zero-length no-op that leaves the
    /// explicit offsets alone.
    #[unittest::def_test]
    fn request_past_eof_is_a_no_op() {
        let input = memfd_file("copy-past-eof-src");
        fill(&input, 8);
        let output = memfd_file("copy-past-eof-dst");
        let mut input_offset = 100;
        let mut output_offset = 0;
        assert_eq!(
            kvfs::copy_file_range(
                &input,
                TransferPosition::At(&mut input_offset),
                &output,
                TransferPosition::At(&mut output_offset),
                10,
                false,
            ),
            Ok(0)
        );
        assert_eq!(input_offset, 100);
        assert_eq!(output_offset, 0);
    }

    /// Pair admission follows the destination table's native primitive:
    /// plain memfs tables carry one and serve memfs storage across
    /// instances — including inputs fronted by a foreign table on memfs
    /// storage — while a destination without a primitive cannot leave its
    /// super block (`EXDEV`).
    #[unittest::def_test]
    fn cross_superblock_pairs_need_native_support() {
        let input = memfd_file("xdev-plain-src");
        fill(&input, 16);
        let output = memfd_file("xdev-plain-dst");
        let mut input_offset = 0;
        let mut output_offset = 0;
        assert_eq!(
            kvfs::copy_file_range(
                &input,
                TransferPosition::At(&mut input_offset),
                &output,
                TransferPosition::At(&mut output_offset),
                16,
                false,
            ),
            Ok(16)
        );

        // A foreign table fronting memfs storage stays servable: the
        // destination's primitive judges the storage, not the front table.
        let frontend = Arc::new(CountingBackend {
            poisoned_call: usize::MAX,
            calls: AtomicUsize::new(0),
        });
        let source_base = memfd_file("xdev-native-src");
        fill(&source_base, 16);
        let input = source_base
            .alloc_clone(FMode::READ, OpenFlags::empty(), frontend)
            .unwrap();
        let output = memfd_file("xdev-native-dst");
        let mut input_offset = 0;
        let mut output_offset = 0;
        assert_eq!(
            kvfs::copy_file_range(
                &input,
                TransferPosition::At(&mut input_offset),
                &output,
                TransferPosition::At(&mut output_offset),
                16,
                false,
            ),
            Ok(16)
        );

        // A primitive-less destination cannot copy across super blocks.
        let backend = Arc::new(PrimitivelessBackend);
        let input = memfd_file("xdev-anon-src");
        fill(&input, 16);
        let output_base = memfd_file("xdev-anon-dst");
        let output = output_base
            .alloc_clone(FMode::WRITE, OpenFlags::empty(), backend)
            .unwrap();
        let mut input_offset = 0;
        let mut output_offset = 0;
        assert_eq!(
            kvfs::copy_file_range(
                &input,
                TransferPosition::At(&mut input_offset),
                &output,
                TransferPosition::At(&mut output_offset),
                16,
                false,
            ),
            Err(KError::from(LinuxError::EXDEV))
        );
    }

    /// Pair admission is decided in the checks, before the request is
    /// shortened to the source EOF (Linux `generic_copy_file_checks`,
    /// `fs/read_write.c:1506`), so a pair the destination cannot serve
    /// reports `EXDEV` even for an empty request. `copy_file_range01`
    /// probes cross-device support with exactly this shape: answering it
    /// with a zero-byte success claims a capability the pair does not have.
    #[unittest::def_test]
    fn empty_request_keeps_the_pair_admission_verdict() {
        let backend = Arc::new(PrimitivelessBackend);
        let input = memfd_file("xdev-empty-src");
        fill(&input, 16);
        let output_base = memfd_file("xdev-empty-dst");
        let output = output_base
            .alloc_clone(FMode::WRITE, OpenFlags::empty(), backend)
            .unwrap();
        // An explicit zero length.
        let mut input_offset = 0;
        let mut output_offset = 0;
        assert_eq!(
            kvfs::copy_file_range(
                &input,
                TransferPosition::At(&mut input_offset),
                &output,
                TransferPosition::At(&mut output_offset),
                0,
                false,
            ),
            Err(KError::from(LinuxError::EXDEV))
        );
        // A request past the source EOF, which the checks shorten to zero.
        let mut input_offset = 16;
        let mut output_offset = 0;
        assert_eq!(
            kvfs::copy_file_range(
                &input,
                TransferPosition::At(&mut input_offset),
                &output,
                TransferPosition::At(&mut output_offset),
                16,
                false,
            ),
            Err(KError::from(LinuxError::EXDEV))
        );
        assert_eq!(input_offset, 16);
        assert_eq!(output_offset, 0);

        // A servable pair keeps Linux's empty-request success.
        let base = memfd_file("xdev-empty-same");
        fill(&base, 16);
        let output = base
            .alloc_clone(
                FMode::WRITE,
                OpenFlags::empty(),
                Arc::new(PrimitivelessBackend),
            )
            .unwrap();
        let mut input_offset = 0;
        let mut output_offset = 16;
        assert_eq!(
            kvfs::copy_file_range(
                &base,
                TransferPosition::At(&mut input_offset),
                &output,
                TransferPosition::At(&mut output_offset),
                0,
                false,
            ),
            Ok(0)
        );
    }

    /// Linux resolves pair admission ahead of the range checks
    /// (`generic_copy_file_checks`: `EXDEV` first, then `EOVERFLOW`, then the
    /// shortening and `generic_write_check_limits`), so an unservable pair
    /// reports `EXDEV` rather than the range verdict it would otherwise hit.
    #[unittest::def_test]
    fn admission_precedes_range_checks_for_unservable_pairs() {
        let backend = Arc::new(PrimitivelessBackend);
        let input = memfd_file("xdev-order-src");
        fill(&input, 16);
        let output_base = memfd_file("xdev-order-dst");
        let output = output_base
            .alloc_clone(FMode::WRITE, OpenFlags::empty(), backend)
            .unwrap();
        // A request whose range wraps reports `EXDEV`, not `EOVERFLOW`.
        let mut input_offset = 1;
        let mut output_offset = 0;
        assert_eq!(
            kvfs::copy_file_range(
                &input,
                TransferPosition::At(&mut input_offset),
                &output,
                TransferPosition::At(&mut output_offset),
                usize::MAX,
                false,
            ),
            Err(KError::from(LinuxError::EXDEV))
        );
        // A destination start at the file-size ceiling reports `EXDEV`,
        // not `EFBIG`.
        let mut input_offset = 0;
        let mut output_offset = MAX_LFS_FILESIZE;
        assert_eq!(
            kvfs::copy_file_range(
                &input,
                TransferPosition::At(&mut input_offset),
                &output,
                TransferPosition::At(&mut output_offset),
                16,
                false,
            ),
            Err(KError::from(LinuxError::EXDEV))
        );

        // A servable pair keeps the range verdicts: the same wrapping
        // request reports `EOVERFLOW`.
        let source = memfd_file("xdev-order-wrap-src");
        fill(&source, 16);
        let target = memfd_file("xdev-order-wrap-dst");
        let mut input_offset = 1;
        let mut output_offset = 0;
        assert_eq!(
            kvfs::copy_file_range(
                &source,
                TransferPosition::At(&mut input_offset),
                &target,
                TransferPosition::At(&mut output_offset),
                usize::MAX,
                false,
            ),
            Err(KError::from(LinuxError::EOVERFLOW))
        );
    }

    /// A native table that refuses the input rejects the pair at admission:
    /// the verdict reaches the caller for empty requests too, so it cannot
    /// depend on a copy step actually running. A refusal may arrive in
    /// either `KError` representation, so the assertions compare the
    /// syscall-visible errno both of them leave the kernel as.
    #[unittest::def_test]
    fn refused_input_keeps_the_admission_verdict_when_empty() {
        let input = memfd_file("xdev-refuse-src");
        fill(&input, 16);
        let output_base = memfd_file("xdev-refuse-dst");
        let output = output_base
            .alloc_clone(FMode::WRITE, OpenFlags::empty(), Arc::new(RefusingBackend))
            .unwrap();
        // An explicit zero length.
        let mut input_offset = 0;
        let mut output_offset = 0;
        let empty = kvfs::copy_file_range(
            &input,
            TransferPosition::At(&mut input_offset),
            &output,
            TransferPosition::At(&mut output_offset),
            0,
            false,
        );
        assert_eq!(empty.map_err(LinuxError::from), Err(LinuxError::EXDEV));
        // A request past the source EOF, which the checks shorten to zero.
        let mut input_offset = 16;
        let mut output_offset = 0;
        let past_eof = kvfs::copy_file_range(
            &input,
            TransferPosition::At(&mut input_offset),
            &output,
            TransferPosition::At(&mut output_offset),
            16,
            false,
        );
        assert_eq!(past_eof.map_err(LinuxError::from), Err(LinuxError::EXDEV));
    }

    /// A destination table without a native primitive falls back to the
    /// generic buffered pump under the same-superblock rule — same inode
    /// here, so the pair stays servable without any native support.
    #[unittest::def_test]
    fn handlerless_destination_falls_back_to_same_superblock() {
        let base = memfd_file("xdev-fallback");
        fill(&base, 16);
        let backend = Arc::new(PrimitivelessBackend);
        let output = base
            .alloc_clone(FMode::WRITE, OpenFlags::empty(), backend)
            .unwrap();
        // Disjoint ranges within the shared inode: [0,8) copies to [8,16).
        let mut input_offset = 0;
        let mut output_offset = 8;
        assert_eq!(
            kvfs::copy_file_range(
                &base,
                TransferPosition::At(&mut input_offset),
                &output,
                TransferPosition::At(&mut output_offset),
                8,
                false,
            ),
            Ok(8)
        );
        assert_eq!(input_offset, 8);
        assert_eq!(output_offset, 16);
    }

    /// Both sides `Managed` on one open file description take the shared
    /// single-lock path: one logical position advances by the transferred
    /// count, and the range rewritten onto itself keeps its content.
    #[unittest::def_test]
    fn same_description_managed_sides_share_one_position() {
        let file = memfd_file("shared-managed");
        fill(&file, 8192);
        assert_eq!(
            kvfs::transfer_files(
                &file,
                TransferPosition::Managed,
                &file,
                TransferPosition::Managed,
                4096,
                false,
            ),
            Ok(4096)
        );
        assert_eq!(file.position(), 4096);
        let mut check = alloc::vec![0u8; 8192];
        let mut position = 0;
        assert_eq!(file.read_from(&mut check, &mut position), Ok(8192));
        let pattern: alloc::vec::Vec<u8> = (0..8192).map(|index| (index % 251) as u8).collect();
        assert_eq!(check, pattern);
    }

    /// A `Managed`-on-both-sides `copy_file_range` starts both sides at
    /// `f_pos`, so the ranges overlap and are rejected with `EINVAL`
    /// without moving the position.
    #[unittest::def_test]
    fn same_description_managed_copy_overlap_is_rejected() {
        let file = memfd_file("shared-overlap");
        fill(&file, 8192);
        assert_eq!(
            kvfs::copy_file_range(
                &file,
                TransferPosition::Managed,
                &file,
                TransferPosition::Managed,
                4096,
                false,
            ),
            Err(KError::from(LinuxError::EINVAL))
        );
        assert_eq!(file.position(), 0);
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
    let resources = kprocess::current_resources();
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

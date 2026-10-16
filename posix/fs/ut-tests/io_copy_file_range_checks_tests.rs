// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

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
        memfs::shmem::create_memfd_file("copy-append-dst", false, kcred::initial_cred()).unwrap();
    let output =
        kvfs::dentry_open(target.into_path(), O_RDWR | O_APPEND, kcred::initial_cred()).unwrap();
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

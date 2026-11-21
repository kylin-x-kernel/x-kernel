// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, string::String, vec, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};

use block::{
    Bio, BioOperation, BioOperations, BlockDeviceOperations, DriverError, DriverResult, Gendisk,
    SECTOR_SIZE,
};
use iov_iter::{
    iov_iter_dest_rewind, iov_iter_kvec_dest_rewind, IovSink, RewindableSink,
};
use kerrno::{KError, KResult};
use ksync::Mutex;
use unittest::def_test;

use super::*;
use crate::{
    Dentry, FMode, FileOperations, InodeOperations, Mount, NodeFlags, NodePermission, NodeType,
    OpenFlags, Path, Umode, VfsFile, VfsFileBuilder, VfsInode, VfsInodeInit,
};

#[def_test]
fn dio_flags_start_empty() {
    assert!(DioMappingFlags::empty().is_empty());
    assert!(DioMappingFlags::NEW.contains(DioMappingFlags::NEW));
    assert_ne!(DioDirection::Read, DioDirection::Write);
}

#[def_test]
fn dio_mapping_flags_match_iomap_bits() {
    let all = DioMappingFlags::NEW
        | DioMappingFlags::DIRTY
        | DioMappingFlags::SHARED
        | DioMappingFlags::MERGED
        | DioMappingFlags::BUFFER_HEAD
        | DioMappingFlags::XATTR
        | DioMappingFlags::BOUNDARY
        | DioMappingFlags::ANON_WRITE
        | DioMappingFlags::ATOMIC_BIO
        | DioMappingFlags::INTEGRITY
        | DioMappingFlags::ZERO_TAIL
        | DioMappingFlags::FSVERITY
        | DioMappingFlags::PRIVATE;
    assert_eq!(all.bits().count_ones(), 13);
    assert_eq!(DioMappingFlags::PRIVATE.bits(), 1 << 12);
}

#[def_test]
fn dio_mapping_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<DioMapping>();
}

#[def_test]
fn direct_io_operations_is_object_safe() {
    fn accepts_dynamic(_: &dyn DirectIoOperations) {}
    let _ = accepts_dynamic;
}

const MIRROR_BLOCK_SIZE: usize = 512;
const MIRROR_BLOCKS: usize = 2048;

fn mirror_byte(index: usize) -> u8 {
    (index % 251) as u8
}

/// Serves reads from an immutable patterned byte image.
///
/// `reads` counts every submitted read; `fail_on_bio` (1-based, 0 disables)
/// fails that read and all later reads with `Io`.
struct MirrorBlockDevice {
    image: Arc<[u8]>,
    reads: Arc<AtomicUsize>,
    fail_on_bio: usize,
    block_size: usize,
}

impl BlockDeviceOperations for MirrorBlockDevice {
    fn num_blocks(&self) -> u64 {
        (self.image.len() / self.block_size) as u64
    }

    fn block_size(&self) -> usize {
        self.block_size
    }
}

impl BioOperations for MirrorBlockDevice {
    fn submit_bio(&self, mut bio: Bio) {
        let result = (|| -> DriverResult {
            if bio.operation() != BioOperation::Read {
                return Err(DriverError::Unsupported);
            }
            let nth = self.reads.fetch_add(1, Ordering::Relaxed) + 1;
            if self.fail_on_bio != 0 && nth >= self.fail_on_bio {
                return Err(DriverError::Io);
            }
            let mut offset = usize::try_from(bio.sector())
                .ok()
                .and_then(|sector| sector.checked_mul(SECTOR_SIZE as usize))
                .ok_or(DriverError::InvalidInput)?;
            for mut vector in bio.vectors_mut() {
                let end = offset
                    .checked_add(vector.bytes().len())
                    .ok_or(DriverError::InvalidInput)?;
                if end > self.image.len() {
                    return Err(DriverError::Io);
                }
                vector.bytes_mut()?.copy_from_slice(&self.image[offset..end]);
                offset = end;
            }
            Ok(())
        })();
        bio.end_io(result);
    }
}

fn mirror_disk_with(fail_on_bio: usize) -> (Arc<block::BlockDevice>, Arc<AtomicUsize>) {
    mirror_disk_sized(MIRROR_BLOCKS, fail_on_bio)
}

fn mirror_disk_sized(
    blocks: usize,
    fail_on_bio: usize,
) -> (Arc<block::BlockDevice>, Arc<AtomicUsize>) {
    mirror_disk_with_block_size(blocks, fail_on_bio, MIRROR_BLOCK_SIZE)
}

#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: Gendisk::alloc still requires a standard Box driver owner; keep this fixture on the production ownership contract"
)]
fn mirror_disk_with_block_size(
    blocks: usize,
    fail_on_bio: usize,
    block_size: usize,
) -> (Arc<block::BlockDevice>, Arc<AtomicUsize>) {
    let image: Box<[u8]> = (0..block_size * blocks).map(mirror_byte).collect();
    let reads = Arc::new(AtomicUsize::new(0));
    let operations: Box<dyn BioOperations> = Box::new(MirrorBlockDevice {
        image: Arc::from(image),
        reads: reads.clone(),
        fail_on_bio,
        block_size,
    });
    let device = Gendisk::alloc(String::from("kvfs-dio-mirror"), 0, 0, 1, operations)
        .expect("valid mirror test disk")
        .part0();
    (device, reads)
}

fn mirror_disk() -> (Arc<block::BlockDevice>, Arc<AtomicUsize>) {
    mirror_disk_with(0)
}

#[def_test]
fn read_chunk_returns_the_requested_device_bytes() {
    let (device, _reads) = mirror_disk();
    let buffer =
        dio_read_chunk(&device, MIRROR_BLOCK_SIZE as u64, 4096).expect("aligned device range reads");
    let expected: Vec<u8> = (MIRROR_BLOCK_SIZE..MIRROR_BLOCK_SIZE + 4096)
        .map(mirror_byte)
        .collect();
    assert_eq!(&buffer[..], &expected[..]);
}

#[def_test]
fn read_chunk_reads_a_large_run_in_one_bio() {
    let (device, _reads) = mirror_disk();
    let len = 256 * 1024 + MIRROR_BLOCK_SIZE;
    let buffer = dio_read_chunk(&device, 0, len).expect("one bio carries the contiguous range");
    let expected: Vec<u8> = (0..len).map(mirror_byte).collect();
    assert_eq!(&buffer[..], &expected[..]);
}

#[def_test]
fn read_chunk_rejects_misaligned_and_empty_requests() {
    let (device, _reads) = mirror_disk();
    for (offset, len) in [(0u64, 0usize), (0, 100), (100, MIRROR_BLOCK_SIZE)] {
        let error =
            dio_read_chunk(&device, offset, len).expect_err("degenerate request is rejected");
        assert_eq!(error, VfsError::from(LinuxError::EINVAL));
    }
}

#[def_test]
fn read_chunk_rejects_a_range_past_capacity() {
    let (device, _reads) = mirror_disk();
    let offset = (MIRROR_BLOCK_SIZE * MIRROR_BLOCKS - MIRROR_BLOCK_SIZE) as u64;
    let error = dio_read_chunk(&device, offset, 2 * MIRROR_BLOCK_SIZE)
        .expect_err("past-cap range is rejected");
    assert_eq!(error, VfsError::from(LinuxError::EINVAL));
}

#[def_test]
fn read_chunk_rejects_byte_offset_overflow() {
    let (device, _reads) = mirror_disk();
    let error = dio_read_chunk(
        &device,
        u64::MAX - (MIRROR_BLOCK_SIZE as u64 - 1),
        2 * MIRROR_BLOCK_SIZE,
    )
    .expect_err("overflowing range is rejected");
    assert_eq!(error, VfsError::from(LinuxError::EFBIG));
}

/// Destination adapter that copies at most `per_copy` bytes per request and
/// refuses everything once `refuse_after` bytes have been accepted.
///
/// `revert` rewinds `accepted` on success; `revert_fails` makes every nonzero
/// rewind fail so tests can pin the engine's end-of-file rewind precedence.
struct CappedSink {
    budget: usize,
    per_copy: usize,
    refuse_after: usize,
    accepted: usize,
    revert_fails: bool,
}

impl IovSink for CappedSink {
    fn count(&self) -> usize {
        self.budget
    }

    fn copy_to_iter(&mut self, src: &[u8]) -> KResult<usize> {
        let room = self.refuse_after.saturating_sub(self.accepted);
        let copied = src.len().min(self.per_copy).min(room);
        self.accepted += copied;
        Ok(copied)
    }

    fn copy_to_iter_nofault(&mut self, src: &[u8]) -> KResult<usize> {
        self.copy_to_iter(src)
    }

    fn revert(&mut self, count: usize) -> KResult<()> {
        if count == 0 {
            return Ok(());
        }
        if self.revert_fails {
            return Err(KError::InvalidInput);
        }
        self.accepted = self.accepted.saturating_sub(count);
        Ok(())
    }
}

impl RewindableSink for CappedSink {}

/// Arms this task's bounce-allocation failure injection until dropped.
///
/// A failed `assert!` returns from the test body, so the guard still clears the
/// injection table and a later task reusing this task object's address cannot
/// inherit a stale schedule.
struct BounceFailureInjection;

impl BounceFailureInjection {
    fn arm(count: usize) -> Self {
        set_bounce_alloc_failures(count);
        Self
    }
}

impl Drop for BounceFailureInjection {
    fn drop(&mut self) {
        set_bounce_alloc_failures(0);
    }
}

struct DioTestOperations;

impl InodeOperations for DioTestOperations {}
impl FileOperations for DioTestOperations {}

/// Builds a regular-file `VfsFile` on nullfs with a preset cached size.
fn dio_test_file(size: u64) -> Arc<VfsFile> {
    let super_block = crate::nullfs::new_superblock();
    let mount = Mount::new_root(&super_block);
    let inode = VfsInode::new_file_with_operations(
        Arc::new(()),
        Arc::new(DioTestOperations),
        Arc::new(DioTestOperations),
        NodeFlags::PRIVATE,
        VfsInodeInit::new(
            7,
            0,
            Umode::new(NodeType::RegularFile, NodePermission::default()),
        ),
    );
    inode.set_size(size);
    let dentry = Dentry::new_file_from_inode(inode, None, String::from("dio-test"));
    dentry.bind_super_block(&super_block);
    VfsFileBuilder::from_path_state(
        Path::new(mount, dentry),
        FMode::READ,
        OpenFlags::empty(),
        Arc::new(DioTestOperations),
        kcred::initial_cred(),
    )
    .finish()
    .expect("dio test file opens")
}

#[derive(Clone, Copy)]
struct MockRun {
    kind: DioMappingKind,
    offset: u64,
    length: u64,
    device_offset: u64,
}

fn hole(offset: u64, length: u64) -> MockRun {
    MockRun { kind: DioMappingKind::Hole, offset, length, device_offset: 0 }
}

fn mapped(offset: u64, length: u64, device_offset: u64) -> MockRun {
    MockRun { kind: DioMappingKind::Mapped, offset, length, device_offset }
}

fn unwritten(offset: u64, length: u64) -> MockRun {
    MockRun { kind: DioMappingKind::Unwritten, offset, length, device_offset: 0 }
}

/// Read-only `DirectIoOperations` mock serving a fixed run layout.
struct MockReadOps {
    runs: Vec<MockRun>,
    device: Arc<block::BlockDevice>,
    map_calls: Mutex<Vec<u64>>,
    end_io_calls: AtomicUsize,
    alignment: u64,
}

impl MockReadOps {
    fn new(runs: Vec<MockRun>, device: Arc<block::BlockDevice>) -> Self {
        Self {
            runs,
            device,
            map_calls: Mutex::new(Vec::new()),
            end_io_calls: AtomicUsize::new(0),
            alignment: MIRROR_BLOCK_SIZE as u64,
        }
    }

    fn positions(&self) -> Vec<u64> {
        self.map_calls.lock().clone()
    }

    fn with_alignment(mut self, alignment: u64) -> Self {
        self.alignment = alignment;
        self
    }
}

impl DirectIoOperations for MockReadOps {
    fn dio_map_blocks(
        &self,
        _inode: &VfsInode,
        pos: u64,
        _len_bytes: u64,
        direction: DioDirection,
    ) -> VfsResult<DioMapping> {
        assert_eq!(direction, DioDirection::Read);
        self.map_calls.lock().push(pos);
        let run = self
            .runs
            .iter()
            .find(|run| run.offset <= pos && pos < run.offset + run.length)
            .unwrap_or_else(|| panic!("mock layout has no run at pos {pos}"));
        Ok(DioMapping {
            kind: run.kind,
            flags: DioMappingFlags::empty(),
            offset: run.offset,
            length_bytes: run.length,
            device_offset: run.device_offset,
            device: self.device.clone(),
        })
    }

    fn dio_end_io(
        &self,
        _inode: &VfsInode,
        _pos: u64,
        _length_bytes: u64,
        _written_bytes: u64,
        _mapping: &DioMapping,
    ) -> VfsResult<()> {
        self.end_io_calls.fetch_add(1, Ordering::Relaxed);
        Err(VfsError::OperationNotSupported)
    }

    fn dio_alignment(&self, _inode: &VfsInode) -> u64 {
        self.alignment
    }
}

#[def_test]
fn read_iter_copies_mixed_layout() {
    let (device, reads) = mirror_disk();
    let file = dio_test_file(3 * MIRROR_BLOCK_SIZE as u64);
    let ops = MockReadOps::new(
        vec![
            hole(0, 512),
            mapped(512, 512, 4096),
            unwritten(1024, 512),
        ],
        device,
    );
    let mut bytes = vec![0xA5; 1536];
    let mut iter = iov_iter_kvec_dest_rewind(&mut bytes);
    let mut iocb = Kiocb::new(&file, 0);
    assert_eq!(dio_read_iter(&mut iocb, &mut iter, &ops), Ok(1536));
    assert!(bytes[..512].iter().all(|byte| *byte == 0));
    let expected: Vec<u8> = (4096..4608).map(mirror_byte).collect();
    assert_eq!(&bytes[512..1024], &expected[..]);
    assert!(bytes[1024..].iter().all(|byte| *byte == 0));
    assert_eq!(reads.load(Ordering::Relaxed), 1);
    assert_eq!(ops.positions(), vec![0, 512, 1024]);
    assert_eq!(ops.end_io_calls.load(Ordering::Relaxed), 0);
}

#[def_test]
fn read_iter_reverts_past_eof_tail() {
    let (device, reads) = mirror_disk();
    let size = 1024 + 256;
    let file = dio_test_file(size);
    let ops = MockReadOps::new(vec![mapped(0, 1536, 0)], device);
    let mut bytes = vec![0xA5; 1536];
    let mut iter = iov_iter_kvec_dest_rewind(&mut bytes);
    let mut iocb = Kiocb::new(&file, 0);
    assert_eq!(dio_read_iter(&mut iocb, &mut iter, &ops), Ok(1280));
    assert_eq!(iter.count(), 256);
    assert_eq!(iocb.ki_pos(), 1280);
    assert_eq!(reads.load(Ordering::Relaxed), 1);
    let expected: Vec<u8> = (0..1280).map(mirror_byte).collect();
    assert_eq!(&bytes[..1280], &expected[..]);
    // The past-EOF tail was physically copied before the rewind; the rewind
    // truncates the reported count and position, not the buffer contents.
    let tail: Vec<u8> = (1280..1536).map(mirror_byte).collect();
    assert_eq!(&bytes[1280..], &tail[..]);
}

#[def_test]
fn read_iter_handles_run_starting_before_request_pos() {
    let (device, reads) = mirror_disk();
    let file = dio_test_file(2048);
    let ops = MockReadOps::new(vec![mapped(0, 1024, 8192)], device);
    let mut bytes = vec![0; 512];
    let mut iter = iov_iter_kvec_dest_rewind(&mut bytes);
    let mut iocb = Kiocb::new(&file, 512);
    assert_eq!(dio_read_iter(&mut iocb, &mut iter, &ops), Ok(512));
    let expected: Vec<u8> = (8192 + 512..8192 + 1024).map(mirror_byte).collect();
    assert_eq!(&bytes[..], &expected[..]);
    assert_eq!(reads.load(Ordering::Relaxed), 1);
    assert_eq!(ops.positions(), vec![512]);
}

#[def_test]
fn read_iter_rejects_unaligned_request() {
    let (device, _reads) = mirror_disk();
    let file = dio_test_file(4096);
    let ops = MockReadOps::new(vec![mapped(0, 4096, 0)], device);
    let mut bytes = vec![0; 513];
    let mut iter = iov_iter_kvec_dest_rewind(&mut bytes);
    let mut iocb = Kiocb::new(&file, 0);
    assert_eq!(
        dio_read_iter(&mut iocb, &mut iter, &ops),
        Err(VfsError::from(LinuxError::EINVAL))
    );
    let mut bytes = vec![0; 512];
    let mut iter = iov_iter_kvec_dest_rewind(&mut bytes);
    let mut iocb = Kiocb::new(&file, 100);
    assert_eq!(
        dio_read_iter(&mut iocb, &mut iter, &ops),
        Err(VfsError::from(LinuxError::EINVAL))
    );
    assert!(ops.positions().is_empty());
}

#[def_test]
fn read_iter_rejects_unsupported_alignment() {
    let (device, _reads) = mirror_disk();
    let file = dio_test_file(4096);
    for alignment in [0, 3] {
        let ops = MockReadOps::new(vec![mapped(0, 4096, 0)], device.clone())
            .with_alignment(alignment as u64);
        let mut bytes = vec![0xA5; 512];
        let mut iter = iov_iter_kvec_dest_rewind(&mut bytes);
        let mut iocb = Kiocb::new(&file, 0);
        assert_eq!(
            dio_read_iter(&mut iocb, &mut iter, &ops),
            Err(VfsError::from(LinuxError::EINVAL))
        );
        assert!(ops.positions().is_empty());
        assert_eq!(iocb.ki_pos(), 0);
    }
}

#[def_test]
fn read_iter_returns_zero_at_or_past_eof() {
    let (device, _reads) = mirror_disk();
    let file = dio_test_file(1024);
    let ops = MockReadOps::new(vec![mapped(0, 1024, 0)], device);
    let mut bytes = vec![0xA5; 512];
    let mut iter = iov_iter_kvec_dest_rewind(&mut bytes);
    let mut iocb = Kiocb::new(&file, 1024);
    assert_eq!(dio_read_iter(&mut iocb, &mut iter, &ops), Ok(0));
    assert_eq!(iocb.ki_pos(), 1024);
    assert!(bytes.iter().all(|byte| *byte == 0xA5));
    assert!(ops.positions().is_empty());
}

#[def_test]
fn read_iter_zero_count_returns_zero() {
    let (device, _reads) = mirror_disk();
    let file = dio_test_file(4096);
    let ops = MockReadOps::new(vec![mapped(0, 4096, 0)], device);
    let mut bytes: Vec<u8> = Vec::new();
    let mut iter = iov_iter_kvec_dest_rewind(&mut bytes);
    let mut iocb = Kiocb::new(&file, 100);
    assert_eq!(dio_read_iter(&mut iocb, &mut iter, &ops), Ok(0));
    assert!(ops.positions().is_empty());
}

#[def_test]
fn read_iter_handles_short_user_iter() {
    let (device, _reads) = mirror_disk();
    let file = dio_test_file(4096);
    let ops = MockReadOps::new(vec![mapped(0, 2048, 0)], device);
    let mut sink = CappedSink {
        budget: 1024,
        per_copy: 512,
        refuse_after: usize::MAX,
        accepted: 0,
        revert_fails: false,
    };
    let mut iter = iov_iter_dest_rewind(&mut sink);
    let mut iocb = Kiocb::new(&file, 0);
    assert_eq!(dio_read_iter(&mut iocb, &mut iter, &ops), Ok(512));
    assert_eq!(iocb.ki_pos(), 512);
    assert_eq!(iter.count(), 512);
}

#[def_test]
fn read_iter_does_not_cache_mappings() {
    let (device, reads) = mirror_disk();
    let file = dio_test_file(4096);
    let ops = MockReadOps::new(vec![mapped(0, 512, 0), mapped(512, 512, 512)], device);
    let mut bytes = vec![0; 1024];
    let mut iter = iov_iter_kvec_dest_rewind(&mut bytes);
    let mut iocb = Kiocb::new(&file, 0);
    assert_eq!(dio_read_iter(&mut iocb, &mut iter, &ops), Ok(1024));
    assert_eq!(ops.positions(), vec![0, 512]);
    assert_eq!(reads.load(Ordering::Relaxed), 2);
}

#[def_test]
fn read_iter_propagates_chunk_error() {
    let (device, _reads) = mirror_disk_with(2);
    let file = dio_test_file(4096);
    let ops = MockReadOps::new(vec![mapped(0, 512, 0), mapped(512, 512, 512)], device);
    let mut bytes = vec![0xA5; 1024];
    let mut iter = iov_iter_kvec_dest_rewind(&mut bytes);
    let mut iocb = Kiocb::new(&file, 0);
    assert_eq!(dio_read_iter(&mut iocb, &mut iter, &ops), Err(VfsError::Io));
    assert_eq!(iocb.ki_pos(), 0);
    assert_eq!(ops.positions(), vec![0, 512]);
}

#[def_test]
fn read_iter_returns_efault_when_sink_refuses() {
    let (device, _reads) = mirror_disk();
    let file = dio_test_file(4096);
    let ops = MockReadOps::new(vec![mapped(0, 2048, 0)], device);
    let mut sink = CappedSink {
        budget: 1024,
        per_copy: 0,
        refuse_after: usize::MAX,
        accepted: 0,
        revert_fails: false,
    };
    let mut iter = iov_iter_dest_rewind(&mut sink);
    let mut iocb = Kiocb::new(&file, 0);
    assert_eq!(
        dio_read_iter(&mut iocb, &mut iter, &ops),
        Err(VfsError::from(LinuxError::EFAULT))
    );
    assert_eq!(iocb.ki_pos(), 0);
}

#[def_test]
fn read_chunk_returns_nomem_on_huge_request() {
    let (device, reads) = mirror_disk();
    device
        .set_capacity(1 << 30)
        .expect("capacity extension for the OOM request");
    let error = dio_read_chunk(&device, 0, 1usize << 36)
        .expect_err("64 GiB bounce allocation cannot succeed");
    assert_eq!(error, VfsError::NoMemory);
    assert_eq!(reads.load(Ordering::Relaxed), 0);
}

#[def_test]
fn read_iter_strides_large_run() {
    let (device, reads) = mirror_disk_sized(3 * MIRROR_BLOCKS, 0);
    let len = 2 * DIO_READ_STRIDE + MIRROR_BLOCK_SIZE;
    let file = dio_test_file(len as u64);
    let ops = MockReadOps::new(vec![mapped(0, len as u64, 0)], device);
    let mut bytes = vec![0xA5; len];
    let mut iter = iov_iter_kvec_dest_rewind(&mut bytes);
    let mut iocb = Kiocb::new(&file, 0);
    assert_eq!(dio_read_iter(&mut iocb, &mut iter, &ops), Ok(len));
    let expected: Vec<u8> = (0..len).map(mirror_byte).collect();
    assert_eq!(&bytes[..], &expected[..]);
    assert_eq!(reads.load(Ordering::Relaxed), 3);
    assert_eq!(ops.positions(), vec![0]);
}

#[def_test]
fn read_iter_stride_halving_recovers() {
    let (device, reads) = mirror_disk_sized(2 * MIRROR_BLOCKS + 1, 0);
    let len = DIO_READ_STRIDE + MIRROR_BLOCK_SIZE;
    let file = dio_test_file(len as u64);
    let ops = MockReadOps::new(vec![mapped(0, len as u64, 0)], device);
    let mut bytes = vec![0xA5; len];
    let mut iter = iov_iter_kvec_dest_rewind(&mut bytes);
    let mut iocb = Kiocb::new(&file, 0);
    let _injection = BounceFailureInjection::arm(1);
    let result = dio_read_iter(&mut iocb, &mut iter, &ops);
    assert_eq!(result, Ok(len));
    let expected: Vec<u8> = (0..len).map(mirror_byte).collect();
    assert_eq!(&bytes[..], &expected[..]);
    // The failed 1 MiB attempt retries as 512 KiB + 512 KiB + 512 B.
    assert_eq!(reads.load(Ordering::Relaxed), 3);
    assert_eq!(iocb.ki_pos(), len as u64);
}

#[def_test]
fn read_iter_stride_enom_exhausted() {
    let (device, reads) = mirror_disk();
    let file = dio_test_file(4096);
    let ops = MockReadOps::new(vec![mapped(0, 4096, 0)], device);
    let mut bytes = vec![0xA5; 1024];
    let mut iter = iov_iter_kvec_dest_rewind(&mut bytes);
    let mut iocb = Kiocb::new(&file, 0);
    let _injection = BounceFailureInjection::arm(64);
    let result = dio_read_iter(&mut iocb, &mut iter, &ops);
    assert_eq!(result, Err(VfsError::NoMemory));
    assert_eq!(iocb.ki_pos(), 0);
    assert_eq!(reads.load(Ordering::Relaxed), 0);
}

#[def_test]
fn read_iter_stride_error_after_prefix() {
    let (device, reads) = mirror_disk_sized(2 * MIRROR_BLOCKS, 2);
    let file = dio_test_file(2 * 1024 * 1024);
    let ops = MockReadOps::new(vec![mapped(0, 2 * 1024 * 1024, 0)], device);
    let mut bytes = vec![0xA5; 2 * 1024 * 1024];
    let mut iter = iov_iter_kvec_dest_rewind(&mut bytes);
    let mut iocb = Kiocb::new(&file, 0);
    assert_eq!(dio_read_iter(&mut iocb, &mut iter, &ops), Err(VfsError::Io));
    assert_eq!(iocb.ki_pos(), 0);
    assert_eq!(iter.count(), 1024 * 1024);
    assert_eq!(reads.load(Ordering::Relaxed), 2);
}

#[def_test]
fn read_iter_short_copy_mid_stride() {
    let (device, reads) = mirror_disk_sized(2 * MIRROR_BLOCKS, 0);
    let file = dio_test_file(2 * 1024 * 1024);
    let ops = MockReadOps::new(vec![mapped(0, 2 * 1024 * 1024, 0)], device);
    let mut sink = CappedSink {
        budget: 2 * 1024 * 1024,
        per_copy: 512 * 1024,
        refuse_after: usize::MAX,
        accepted: 0,
        revert_fails: false,
    };
    let mut iter = iov_iter_dest_rewind(&mut sink);
    let mut iocb = Kiocb::new(&file, 0);
    assert_eq!(
        dio_read_iter(&mut iocb, &mut iter, &ops),
        Ok(512 * 1024)
    );
    assert_eq!(reads.load(Ordering::Relaxed), 1);
    assert_eq!(iocb.ki_pos(), 512 * 1024);
}

#[def_test]
fn read_iter_reverts_eof_tail_on_error() {
    let (device, _reads) = mirror_disk_sized(2 * MIRROR_BLOCKS, 2);
    let file = dio_test_file(1280);
    let ops = MockReadOps::new(
        vec![mapped(0, 2048, 0), mapped(2048, 2048, 512)],
        device,
    );
    let mut bytes = vec![0xA5; 4096];
    let mut iter = iov_iter_kvec_dest_rewind(&mut bytes);
    let mut iocb = Kiocb::new(&file, 0);
    assert_eq!(dio_read_iter(&mut iocb, &mut iter, &ops), Err(VfsError::Io));
    assert_eq!(iter.count(), 2816);
    assert_eq!(iocb.ki_pos(), 0);
    let expected: Vec<u8> = (0..1280).map(mirror_byte).collect();
    assert_eq!(&bytes[..1280], &expected[..]);
}

#[def_test]
fn read_iter_revert_failure_preserves_io_error() {
    let (device, _reads) = mirror_disk_sized(2 * MIRROR_BLOCKS, 2);
    let file = dio_test_file(1280);
    let ops = MockReadOps::new(
        vec![mapped(0, 2048, 0), mapped(2048, 2048, 512)],
        device,
    );
    let mut sink = CappedSink {
        budget: 4096,
        per_copy: 4096,
        refuse_after: usize::MAX,
        accepted: 0,
        revert_fails: true,
    };
    let mut iter = iov_iter_dest_rewind(&mut sink);
    let mut iocb = Kiocb::new(&file, 0);
    assert_eq!(dio_read_iter(&mut iocb, &mut iter, &ops), Err(VfsError::Io));
    assert_eq!(iocb.ki_pos(), 0);
}

#[def_test]
fn read_iter_efault_after_eof_preserves_error() {
    let (device, _reads) = mirror_disk();
    let file = dio_test_file(1280);
    let ops = MockReadOps::new(
        vec![mapped(0, 2048, 0), mapped(2048, 2048, 0)],
        device,
    );
    let mut sink = CappedSink {
        budget: 4096,
        per_copy: 4096,
        refuse_after: 2048,
        accepted: 0,
        revert_fails: false,
    };
    let mut iter = iov_iter_dest_rewind(&mut sink);
    let mut iocb = Kiocb::new(&file, 0);
    assert_eq!(
        dio_read_iter(&mut iocb, &mut iter, &ops),
        Err(VfsError::from(LinuxError::EFAULT))
    );
    assert_eq!(iocb.ki_pos(), 0);
}

#[def_test]
fn read_iter_success_revert_error_propagates() {
    let (device, _reads) = mirror_disk();
    let file = dio_test_file(1280);
    let ops = MockReadOps::new(vec![mapped(0, 1536, 0)], device);
    let mut sink = CappedSink {
        budget: 1536,
        per_copy: 4096,
        refuse_after: usize::MAX,
        accepted: 0,
        revert_fails: true,
    };
    let mut iter = iov_iter_dest_rewind(&mut sink);
    let mut iocb = Kiocb::new(&file, 0);
    assert_eq!(
        dio_read_iter(&mut iocb, &mut iter, &ops),
        Err(VfsError::InvalidInput)
    );
    assert_eq!(iocb.ki_pos(), 0);
}

#[def_test]
fn read_iter_hole_crossing_eof_reverts() {
    let (device, reads) = mirror_disk();
    let file = dio_test_file(1280);
    let ops = MockReadOps::new(vec![hole(0, 2048)], device);
    let mut bytes = vec![0xA5; 2048];
    let mut iter = iov_iter_kvec_dest_rewind(&mut bytes);
    let mut iocb = Kiocb::new(&file, 0);
    assert_eq!(dio_read_iter(&mut iocb, &mut iter, &ops), Ok(1280));
    assert_eq!(iter.count(), 768);
    assert!(bytes[..1280].iter().all(|byte| *byte == 0));
    assert_eq!(reads.load(Ordering::Relaxed), 0);
}

#[def_test]
fn read_iter_rejects_run_misaligned_to_device_block() {
    let (device, reads) = mirror_disk_with_block_size(4, 0, 4096);
    let file = dio_test_file(4096);
    let ops =
        MockReadOps::new(vec![mapped(0, 4096, 0)], device).with_alignment(MIRROR_BLOCK_SIZE as u64);
    let mut bytes = vec![0xA5; MIRROR_BLOCK_SIZE];
    let mut iter = iov_iter_kvec_dest_rewind(&mut bytes);
    let mut iocb = Kiocb::new(&file, 0);
    assert_eq!(
        dio_read_iter(&mut iocb, &mut iter, &ops),
        Err(VfsError::from(LinuxError::EINVAL))
    );
    assert_eq!(reads.load(Ordering::Relaxed), 0);
    assert_eq!(iocb.ki_pos(), 0);
}

#[def_test]
fn read_iter_rejects_cur_misaligned_to_device_block() {
    let (device, reads) = mirror_disk_with_block_size(4, 0, 4096);
    let file = dio_test_file(4096);
    let ops =
        MockReadOps::new(vec![mapped(0, 4096, 0)], device).with_alignment(MIRROR_BLOCK_SIZE as u64);
    let mut bytes = vec![0xA5; MIRROR_BLOCK_SIZE];
    let mut iter = iov_iter_kvec_dest_rewind(&mut bytes);
    let mut iocb = Kiocb::new(&file, MIRROR_BLOCK_SIZE as u64);
    assert_eq!(
        dio_read_iter(&mut iocb, &mut iter, &ops),
        Err(VfsError::from(LinuxError::EINVAL))
    );
    assert_eq!(reads.load(Ordering::Relaxed), 0);
    assert_eq!(iocb.ki_pos(), MIRROR_BLOCK_SIZE as u64);
}

#[def_test]
fn read_iter_device_block_passes_aligned() {
    let (device, reads) = mirror_disk_with_block_size(4, 0, 4096);
    let file = dio_test_file(4096);
    let ops = MockReadOps::new(vec![mapped(0, 4096, 0)], device).with_alignment(4096);
    let mut bytes = vec![0xA5; 4096];
    let mut iter = iov_iter_kvec_dest_rewind(&mut bytes);
    let mut iocb = Kiocb::new(&file, 0);
    assert_eq!(dio_read_iter(&mut iocb, &mut iter, &ops), Ok(4096));
    let expected: Vec<u8> = (0..4096).map(mirror_byte).collect();
    assert_eq!(&bytes[..], &expected[..]);
    assert_eq!(iocb.ki_pos(), 4096);
    assert_eq!(reads.load(Ordering::Relaxed), 1);
}

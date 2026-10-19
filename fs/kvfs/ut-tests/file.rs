// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{vec, vec::Vec};

use unittest::{assert_eq, def_test};

use super::*;

struct TestInodeOperations;

impl crate::InodeOperations for TestInodeOperations {}

fn sync_test_file(operations: Arc<dyn FileOperations>, mode: FMode) -> Arc<VfsFile> {
    let super_block = crate::nullfs::new_superblock();
    let mount = crate::Mount::new_root(&super_block);
    let inode = VfsInode::new_file_with_operations(
        Arc::new(()),
        Arc::new(TestInodeOperations),
        operations.clone(),
        NodeFlags::PRIVATE,
        crate::VfsInodeInit::new(
            2,
            0,
            crate::Umode::new(NodeType::RegularFile, crate::NodePermission::default()),
        ),
    );
    let dentry = crate::Dentry::new_file_from_inode(inode, None, "sync-test".into());
    dentry.bind_super_block(&super_block);
    VfsFileBuilder::from_path_state(
        crate::Path::new(mount, dentry),
        mode,
        OpenFlags::empty(),
        operations,
        kcred::initial_cred(),
    )
    .finish()
    .unwrap()
}

struct PositionLockOperations {
    is_stream: bool,
    offsets: Mutex<Vec<u64>>,
}

impl PositionLockOperations {
    fn check_position_lock(&self, file: &VfsFile, offset: u64) {
        core::assert_eq!(file.position.try_lock().is_some(), self.is_stream);
        self.offsets.lock().push(offset);
    }
}

impl FileOperations for PositionLockOperations {
    fn read(&self, file: &VfsFile, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        self.check_position_lock(file, offset);
        buf.copy_from_slice(b"io!");
        Ok(buf.len())
    }

    fn write(&self, file: &VfsFile, buf: &[u8], offset: u64) -> VfsResult<usize> {
        self.check_position_lock(file, offset);
        core::assert_eq!(buf, b"io!");
        Ok(buf.len())
    }
}

fn check_sequential_io_position_contract(is_stream: bool) {
    let operations = Arc::new(PositionLockOperations {
        is_stream,
        offsets: Mutex::new(Vec::new()),
    });
    let mut mode = FMode::READ | FMode::WRITE | FMode::CAN_READ | FMode::CAN_WRITE;
    if is_stream {
        mode |= FMode::STREAM;
    }
    let file = sync_test_file(operations.clone(), mode);
    file.set_position(17);

    let mut bytes = [0; 3];
    core::assert_eq!(file.read(&mut bytes), Ok(3));
    core::assert_eq!(&bytes, b"io!");
    core::assert_eq!(file.position(), if is_stream { 17 } else { 20 });

    bytes.fill(0);
    let mut dest = iov_iter_kvec_dest(&mut bytes);
    core::assert_eq!(file.read_iter(&mut dest), Ok(3));
    core::assert_eq!(dest.count(), 0);
    core::assert_eq!(&bytes, b"io!");
    core::assert_eq!(file.position(), if is_stream { 17 } else { 23 });

    core::assert_eq!(file.write(b"io!"), Ok(3));
    core::assert_eq!(file.position(), if is_stream { 17 } else { 26 });

    let mut source = iov_iter_kvec_source(b"io!");
    core::assert_eq!(file.write_iter(&mut source), Ok(3));
    core::assert_eq!(source.count(), 0);
    core::assert_eq!(file.position(), if is_stream { 17 } else { 29 });
    core::assert_eq!(
        *operations.offsets.lock(),
        if is_stream {
            vec![0; 4]
        } else {
            vec![17, 20, 23, 26]
        }
    );
}

#[def_test]
fn stream_io_does_not_lock_or_advance_file_position() {
    check_sequential_io_position_contract(true);
}

#[def_test]
fn nonstream_io_serializes_and_advances_file_position() {
    check_sequential_io_position_contract(false);
}

struct PollCaptureOperations(kpoll::PollSet);

impl FileOperations for PollCaptureOperations {
    fn capture_poll_source(
        &self,
        _file: &VfsFile,
    ) -> Result<Option<FilePollSource>, PollRegisterError> {
        Ok(Some(FilePollSource::new(self.0.downgrade(), IoEvents::IN)))
    }
}

#[def_test]
fn captured_poll_source_filters_cancels_and_expires() {
    let default = sync_test_file(Arc::new(EmptyFileOperations), FMode::READ);
    assert!(default.capture_poll_source().unwrap().is_none());
    let ops = Arc::new(PollCaptureOperations(kpoll::PollSet::new()));
    let file = sync_test_file(ops.clone(), FMode::READ);
    let captured = file.capture_poll_source().unwrap().unwrap();
    let context = core::task::Context::from_waker(core::task::Waker::noop());
    let mut registrations = kpoll::PollRegistrations::new();
    captured
        .register(&mut registrations.context(&context), IoEvents::OUT)
        .unwrap();
    assert!(registrations.is_empty());
    captured
        .register(&mut registrations.context(&context), IoEvents::IN)
        .unwrap();
    assert_eq!(registrations.len(), 1);
    registrations.clear();
    assert_eq!(ops.0.wake(), 0);
    captured
        .register(&mut registrations.context(&context), IoEvents::IN)
        .unwrap();
    assert_eq!(ops.0.wake(), 1);
    drop(file);
    drop(ops);
    captured
        .register(&mut registrations.context(&context), IoEvents::IN)
        .unwrap();
    assert!(registrations.is_empty());
}

struct SyncOperations {
    intents: Mutex<Vec<bool>>,
}

struct ShortWriteOperations;

struct InterruptedAfterBlock(AtomicU32);

impl FileOperations for InterruptedAfterBlock {
    fn read(&self, _: &VfsFile, buf: &mut [u8], _: u64) -> VfsResult<usize> {
        if self.0.fetch_add(1, Ordering::Relaxed) != 0 {
            return Err(VfsError::IoWaitInterrupted);
        }
        buf.fill(0x37);
        Ok(buf.len())
    }

    fn write(&self, _: &VfsFile, buf: &[u8], _: u64) -> VfsResult<usize> {
        if self.0.fetch_add(1, Ordering::Relaxed) != 0 {
            return Err(VfsError::IoWaitInterrupted);
        }
        assert!(buf.iter().all(|byte| *byte == 0x37));
        Ok(buf.len())
    }
}

#[def_test]
fn iterator_progress_overrides_later_file_wait_interruption() {
    let length = memaddr::PAGE_SIZE_4K + 19;
    let file = sync_test_file(
        Arc::new(InterruptedAfterBlock(AtomicU32::new(0))),
        FMode::READ,
    );
    let mut bytes = vec![0; length];
    let mut iter = iov_iter_kvec_dest(&mut bytes);
    let mut iocb = Kiocb::new(&file, 3);
    assert_eq!(
        file.operations().read_iter(&mut iocb, &mut iter),
        Ok(memaddr::PAGE_SIZE_4K)
    );
    assert_eq!(iter.count(), 19);
    assert_eq!(iocb.ki_pos(), 3 + memaddr::PAGE_SIZE_4K as u64);
    assert!(
        bytes[..memaddr::PAGE_SIZE_4K]
            .iter()
            .all(|byte| *byte == 0x37)
    );
    assert!(bytes[memaddr::PAGE_SIZE_4K..].iter().all(|byte| *byte == 0));
    let file = sync_test_file(
        Arc::new(InterruptedAfterBlock(AtomicU32::new(0))),
        FMode::WRITE,
    );
    bytes.fill(0x37);
    let mut iter = iov_iter_kvec_source(&bytes);
    let mut iocb = Kiocb::new(&file, 3);
    assert_eq!(
        file.operations().write_iter(&mut iocb, &mut iter),
        Ok(memaddr::PAGE_SIZE_4K)
    );
    assert_eq!(iter.count(), 19);
    assert_eq!(iocb.ki_pos(), 3 + memaddr::PAGE_SIZE_4K as u64);
}

impl FileOperations for ShortWriteOperations {
    fn write(&self, _file: &VfsFile, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Ok(2)
    }
}

#[def_test]
fn scalar_write_iter_rewinds_unaccepted_bytes() {
    let file = sync_test_file(Arc::new(ShortWriteOperations), FMode::WRITE);
    let mut iocb = Kiocb::new(&file, 7);
    let mut iter = iov_iter_kvec_source(b"abcdef");
    assert_eq!(file.operations().write_iter(&mut iocb, &mut iter), Ok(2));
    assert_eq!(iter.count(), 4);
    assert_eq!(iocb.ki_pos(), 9);
}

impl FileOperations for SyncOperations {
    fn fsync(&self, _file: &VfsFile, data_only: bool) -> VfsResult<()> {
        self.intents.lock().push(data_only);
        if data_only {
            Err(VfsError::StorageFull)
        } else {
            Err(VfsError::Io)
        }
    }
}

#[def_test]
fn fsync_keeps_legacy_backend_default_success() {
    let file = sync_test_file(Arc::new(EmptyFileOperations), FMode::READ);
    for data_only in [false, true] {
        assert_eq!(file.fsync(data_only), Ok(()));
    }
}

#[def_test]
fn pipe_and_fifo_operation_tables_reject_sync() {
    for operations in [
        crate::pipe::pipe_file_operations(),
        crate::pipe::fifo_file_operations(),
    ] {
        let file = sync_test_file(operations, FMode::READ | FMode::WRITE);
        for data_only in [false, true] {
            assert_eq!(file.fsync(data_only), Err(VfsError::InvalidInput));
        }
    }
}

#[def_test]
fn fsync_preserves_sync_intent_and_backend_errors_on_readonly_file() {
    let operations = Arc::new(SyncOperations {
        intents: Mutex::new(Vec::new()),
    });
    let file = sync_test_file(operations.clone(), FMode::READ);
    assert_eq!(file.fsync(false), Err(VfsError::Io));
    assert_eq!(file.fsync(true), Err(VfsError::StorageFull));
    assert_eq!(*operations.intents.lock(), vec![false, true]);
}

#[def_test]
fn fsync_path_only_file_returns_ebadf_without_calling_backend() {
    let operations = Arc::new(SyncOperations {
        intents: Mutex::new(Vec::new()),
    });
    let file = sync_test_file(operations.clone(), FMode::PATH);
    for data_only in [false, true] {
        assert_eq!(file.fsync(data_only), Err(VfsError::BadFileDescriptor));
    }
    assert_eq!(operations.intents.lock().len(), 0);
}

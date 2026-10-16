// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

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

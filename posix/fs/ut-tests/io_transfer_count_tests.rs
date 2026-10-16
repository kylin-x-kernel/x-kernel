// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

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
    let base = memfs::shmem::create_memfd_file("transfer-count-test", false, kcred::initial_cred())
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

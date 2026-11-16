// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Partial writeback completion and retry tests.

use alloc::{sync::Arc, vec, vec::Vec};
use core::{
    ops::Range,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use kalloc_types::KBox;
use ksync::Mutex;
use memaddr::PAGE_SIZE_4K;
use pagecache::FolioRead;
use unittest::def_test;

use super::{
    super::{AddressSpace, AddressSpaceOperations, WritebackControl, WritebackSyncMode},
    TruncateOps,
};
use crate::{
    NodeFlags, NodePermission, NodeType, Umode, VfsError, VfsInode, VfsInodeInit, VfsResult,
};

struct PrefixRetryState {
    fail_once: bool,
    calls: usize,
    durable: Vec<u8>,
}

struct PrefixRetryWritebackOps {
    state: Mutex<PrefixRetryState>,
}

impl AddressSpaceOperations for PrefixRetryWritebackOps {
    fn read_folio(&self, _mapping: &AddressSpace, mut folio: FolioRead) -> VfsResult<()> {
        folio.data().fill(0);
        folio.complete(Ok(()))
    }

    fn writepages(&self, mapping: &AddressSpace, control: &mut WritebackControl) -> VfsResult<()> {
        mapping.writeback_cached_ranges(control, PAGE_SIZE_4K * 2, |offset, data| {
            let start = usize::try_from(offset).map_err(|_| (0, VfsError::InvalidInput))?;
            let end = start
                .checked_add(data.len())
                .ok_or((0, VfsError::InvalidInput))?;
            let mut state = self.state.lock();
            if state.durable.len() < end {
                state.durable.resize(end, 0);
            }
            state.calls += 1;
            if state.fail_once {
                let prefix = data.len() / 2;
                state.durable[start..start + prefix].copy_from_slice(&data[..prefix]);
                state.fail_once = false;
                return Err((prefix, VfsError::InvalidInput));
            }
            state.durable[start..end].copy_from_slice(data);
            Ok(data.len())
        })
    }
}

#[def_test]
fn failed_prefix_writeback_redirties_and_retry_converges() {
    let operations: &'static PrefixRetryWritebackOps = KBox::leak(
        KBox::new(PrefixRetryWritebackOps {
            state: Mutex::new(PrefixRetryState {
                fail_once: true,
                calls: 0,
                durable: Vec::new(),
            }),
        })
        .unwrap(),
    );
    let input = vec![0x6d; PAGE_SIZE_4K * 2];
    let inode = VfsInode::new_file_with_address_space_and_flags(
        Arc::new(TruncateOps),
        NodeFlags::ALWAYS_CACHE,
        operations,
        VfsInodeInit::new(
            3,
            u64::try_from(input.len()).unwrap(),
            Umode::new(NodeType::RegularFile, NodePermission::default()),
        ),
    );
    let address_space = inode.address_space();
    address_space
        .write_cached_bytes(0, &input)
        .expect("populate dirty page-cache batch");

    let mut control = WritebackControl::all().with_nr_to_write(2);
    assert_eq!(
        address_space.writepages_control(&mut control),
        Err(VfsError::InvalidInput)
    );
    assert_eq!(control.nr_to_write(), 0);
    for (index, expected_dirty) in [(0, false), (1, true)] {
        let state = address_space
            .i_pages
            .with_folio(index, |folio| {
                let folio = folio.expect("failed batch folio remains cached");
                (folio.is_dirty(), folio.is_under_writeback())
            })
            .unwrap();
        assert_eq!(state, (expected_dirty, false));
    }
    address_space.i_pages.verify_index_consistency();
    {
        let state = operations.state.lock();
        assert_eq!(state.calls, 1);
        assert_eq!(&state.durable[..PAGE_SIZE_4K], &input[..PAGE_SIZE_4K]);
        assert!(state.durable[PAGE_SIZE_4K..].iter().all(|byte| *byte == 0));
    }

    let mut control = WritebackControl::all().with_nr_to_write(1);
    address_space
        .writepages_control(&mut control)
        .expect("redirtied batch converges on retry");
    assert_eq!(control.nr_to_write(), 0);
    for index in 0..2 {
        let state = address_space
            .i_pages
            .with_folio(index, |folio| {
                let folio = folio.expect("retried batch folio remains cached");
                (folio.is_dirty(), folio.is_under_writeback())
            })
            .unwrap();
        assert_eq!(state, (false, false));
    }
    address_space.i_pages.verify_index_consistency();
    let state = operations.state.lock();
    assert_eq!(state.calls, 2);
    assert_eq!(state.durable, input);
}

#[def_test]
fn single_folio_failure_preserves_control_progress_for_retry() {
    let input = vec![0x6d; PAGE_SIZE_4K * 3];
    let inode = VfsInode::new_file_with_address_space_and_flags(
        Arc::new(TruncateOps),
        NodeFlags::ALWAYS_CACHE,
        &super::TRUNCATE_OPERATIONS,
        VfsInodeInit::new(
            4,
            input.len() as u64,
            Umode::new(NodeType::RegularFile, NodePermission::default()),
        ),
    );
    let mapping = inode.address_space();
    mapping.write_cached_bytes(0, &input).unwrap();
    let mut control = WritebackControl::all().with_nr_to_write(1);
    let _invalidate_guard = mapping.invalidate_lock.read();
    let result = mapping.writeback_cached_folios(&mut control, |index, _, _| {
        if index == 1 {
            Err(VfsError::InvalidInput)
        } else {
            Ok(())
        }
    });
    assert_eq!(result, Err(VfsError::InvalidInput));
    assert_eq!(control.nr_to_write(), 0);
    assert_eq!(control.pages_skipped(), 0);
    mapping.i_pages.verify_index_consistency();
    let mut control = WritebackControl::all().with_nr_to_write(1);
    let mut retried = Vec::new();
    mapping
        .writeback_cached_folios(&mut control, |index, _, _| {
            retried.push(index);
            Ok(())
        })
        .unwrap();
    assert_eq!(retried, vec![1]);
    assert_eq!(control.nr_to_write(), 0);
    mapping.i_pages.verify_index_consistency();
    for index in 0..3 {
        mapping
            .i_pages
            .with_folio(index, |folio| {
                let folio = folio.unwrap();
                assert!(!folio.is_dirty());
                assert!(!folio.is_under_writeback());
            })
            .unwrap();
    }
}

type PendingWriteback = (
    u64,
    Vec<u8>,
    pagecache::OwnedWritebackCompletion<AddressSpace>,
);

struct DeferredWritebackOps {
    calls: Mutex<Vec<Range<u64>>>,
    pending: Mutex<Vec<PendingWriteback>>,
    fail_on_call: Option<usize>,
    append_on_call: Option<usize>,
    ready: ktask::WaitQueue,
    submitted: AtomicUsize,
}

impl DeferredWritebackOps {
    fn submit(&self, mapping: &AddressSpace, control: &mut WritebackControl) -> VfsResult<()> {
        mapping.submit_cached_ranges(control, PAGE_SIZE_4K, |_, batch| {
            let offset = batch.offset();
            let (data, completion) = batch.into_parts();
            let completion = mapping.retain_writeback_completion(completion)?;
            self.pending.lock().push((offset, data, completion));
            Ok(())
        })
    }

    fn complete(&self, mapping: &AddressSpace, offset: u64, error: Option<VfsError>) {
        let mut pending = self.pending.lock();
        let position = pending
            .iter()
            .position(|(start, ..)| *start == offset)
            .unwrap();
        let (_, data, completion) = pending.remove(position);
        drop(pending);
        if let Some(error) = error {
            mapping.writeback_errors().record(error);
        }
        completion
            .complete(if error.is_some() { 0 } else { data.len() })
            .unwrap();
    }
}

impl AddressSpaceOperations for DeferredWritebackOps {
    fn read_folio(&self, _mapping: &AddressSpace, mut folio: FolioRead) -> VfsResult<()> {
        folio.data().fill(0);
        folio.complete(Ok(()))
    }

    fn writepages(&self, mapping: &AddressSpace, control: &mut WritebackControl) -> VfsResult<()> {
        let call = {
            let mut calls = self.calls.lock();
            calls.push(control.range_start()..control.range_end());
            calls.len()
        };
        if self.append_on_call == Some(call) {
            let append_offset = (PAGE_SIZE_4K * 2) as u64;
            mapping.write_cached_bytes(append_offset, &[0x7a; PAGE_SIZE_4K])?;
            mapping.inode().unwrap().set_size((PAGE_SIZE_4K * 3) as u64);
            let mut appended = WritebackControl::range(append_offset, PAGE_SIZE_4K)?;
            self.submit(mapping, &mut appended)?;
        }
        self.submit(mapping, control)?;
        self.submitted.store(call, Ordering::Release);
        self.ready.notify_all(false);
        if self.fail_on_call == Some(call) {
            Err(VfsError::StorageFull)
        } else {
            Ok(())
        }
    }
}

fn deferred_mapping(
    fail_on_call: Option<usize>,
    append_on_call: Option<usize>,
) -> VfsResult<(Arc<VfsInode>, &'static DeferredWritebackOps)> {
    let operations: &'static DeferredWritebackOps = KBox::leak(KBox::new(DeferredWritebackOps {
        calls: Mutex::new(Vec::new()),
        pending: Mutex::new(Vec::new()),
        fail_on_call,
        append_on_call,
        ready: ktask::WaitQueue::new(),
        submitted: AtomicUsize::new(0),
    })?);
    let inode = VfsInode::new_file_with_address_space_and_flags(
        Arc::new(TruncateOps),
        NodeFlags::ALWAYS_CACHE,
        operations,
        VfsInodeInit::new(
            30,
            PAGE_SIZE_4K as u64,
            Umode::new(NodeType::RegularFile, NodePermission::default()),
        ),
    );
    Ok((inode, operations))
}

// Observe the actual parking transition, never a fixed number of yields.
fn assert_waiting(task: &ktask::KtaskRef, finished: &AtomicBool) {
    while !task.state().is_blocked() && !finished.load(Ordering::Acquire) {
        ktask::yield_now();
    }
    assert!(!finished.load(Ordering::Acquire));
}

#[def_test]
fn raw_zero_budget_and_empty_range_bypass_filesystem_dispatch() {
    let (inode, operations) = deferred_mapping(None, None).unwrap();
    let mapping = inode.address_space();
    mapping.write_cached_bytes(0, b"dirty").unwrap();
    let mut zero = WritebackControl::all().with_nr_to_write(0);
    assert_eq!(mapping.writepages_control(&mut zero), Ok(()));
    assert_eq!(mapping.writepages_range(1, 0), Ok(()));
    assert!(operations.calls.lock().is_empty());
    assert!(operations.pending.lock().is_empty());
    assert_eq!(
        mapping.writepages_range(u64::MAX, 1),
        Err(VfsError::InvalidInput)
    );
    assert_eq!(
        mapping.write_and_wait_range(core::ops::Range { start: 5, end: 4 }),
        Err(VfsError::InvalidInput)
    );
    assert_eq!(mapping.write_and_wait_range(3..3), Ok(()));
    assert!(operations.calls.lock().is_empty());
}

#[def_test]
fn data_write_and_wait_covers_clean_old_writeback_and_new_errors_after_submission_failure() {
    for (fail_submission, mode) in [
        (false, WritebackSyncMode::None),
        (true, WritebackSyncMode::All),
    ] {
        let (inode, operations) = deferred_mapping(fail_submission.then_some(2), None).unwrap();
        let mapping = inode.address_space();
        mapping.write_cached_bytes(0, b"old").unwrap();
        let mut raw = WritebackControl::all().with_sync_mode(mode);
        mapping.writepages_control(&mut raw).unwrap();
        mapping
            .i_pages
            .with_folio(0, |folio| {
                let folio = folio.unwrap();
                assert!(!folio.is_dirty());
                assert!(folio.is_under_writeback());
            })
            .unwrap();
        let finished = Arc::new(AtomicBool::new(false));
        let result = Arc::new(Mutex::new(None));
        let waiting_mapping = mapping.clone();
        let waiting_finished = finished.clone();
        let waiting_result = result.clone();
        let task = ktask::spawn(move || {
            *waiting_result.lock() = Some(waiting_mapping.write_and_wait_range(0..u64::MAX));
            waiting_finished.store(true, Ordering::Release);
        });
        operations
            .ready
            .wait_until(|| operations.submitted.load(Ordering::Acquire) == 2);
        assert_waiting(&task, &finished);
        operations.complete(&mapping, 0, Some(VfsError::Io));
        assert_eq!(task.join(), 0);
        assert_eq!(
            *result.lock(),
            Some(Err(if fail_submission {
                VfsError::StorageFull
            } else {
                VfsError::Io
            }))
        );
        let cursor = mapping.writeback_errors().sample();
        assert_eq!(mapping.writeback_errors().check(cursor), Err(VfsError::Io));
        assert_eq!(mapping.write_and_wait_range(0..0), Ok(()));
    }
}

#[def_test]
fn live_invalidation_waits_for_owned_writeback_and_retains_failed_data() {
    for (fail_submission, completion_error) in [
        (false, None),
        (false, Some(VfsError::Io)),
        (true, Some(VfsError::Io)),
    ] {
        let (inode, operations) = deferred_mapping(fail_submission.then_some(1), None).unwrap();
        let mapping = inode.address_space();
        mapping.write_cached_bytes(0, b"retained").unwrap();
        let finished = Arc::new(AtomicBool::new(false));
        let result = Arc::new(Mutex::new(None));
        let invalidating_inode = inode.clone();
        let invalidating_finished = finished.clone();
        let invalidating_result = result.clone();
        let task = ktask::spawn(move || {
            *invalidating_result.lock() = Some(invalidating_inode.invalidate_cached_pages());
            invalidating_finished.store(true, Ordering::Release);
        });
        operations
            .ready
            .wait_until(|| operations.submitted.load(Ordering::Acquire) == 1);
        assert_waiting(&task, &finished);
        assert_eq!(mapping.nrpages(), 1);
        operations.complete(&mapping, 0, completion_error);
        assert_eq!(task.join(), 0);
        let expected_error = if fail_submission {
            Some(VfsError::StorageFull)
        } else {
            completion_error
        };
        assert_eq!(*result.lock(), Some(expected_error.map_or(Ok(()), Err)));
        if expected_error.is_some() {
            assert_eq!(mapping.nrpages(), 1);
            mapping
                .i_pages
                .with_folio(0, |folio| {
                    let folio = folio.unwrap();
                    assert!(folio.is_dirty());
                    assert!(!folio.is_under_writeback());
                    assert_eq!(&folio.data()[..8], b"retained");
                })
                .unwrap();
        } else {
            assert_eq!(mapping.nrpages(), 0);
        }
    }
}

#[def_test]
fn finite_data_target_submits_and_waits_the_same_bound_during_append() {
    let (inode, operations) = deferred_mapping(None, Some(1)).unwrap();
    let mapping = inode.address_space();
    mapping.write_cached_bytes(0, b"captured").unwrap();
    let finished = Arc::new(AtomicBool::new(false));
    let result = Arc::new(Mutex::new(None));
    let waiting_mapping = mapping.clone();
    let waiting_finished = finished.clone();
    let waiting_result = result.clone();
    let task = ktask::spawn(move || {
        *waiting_result.lock() = Some(waiting_mapping.write_and_wait_range(0..u64::MAX));
        waiting_finished.store(true, Ordering::Release);
    });
    operations
        .ready
        .wait_until(|| operations.submitted.load(Ordering::Acquire) == 1);
    assert_waiting(&task, &finished);
    assert_eq!(*operations.calls.lock(), vec![0..PAGE_SIZE_4K as u64]);
    operations.complete(&mapping, 0, None);
    assert_eq!(task.join(), 0);
    assert_eq!(*result.lock(), Some(Ok(())));
    assert!(finished.load(Ordering::Acquire));
    mapping
        .i_pages
        .with_folio(2, |folio| assert!(folio.unwrap().is_under_writeback()))
        .unwrap();
    operations.complete(&mapping, (PAGE_SIZE_4K * 2) as u64, None);
}

#[def_test]
fn data_range_sync_rounds_to_intersecting_folios() {
    let operations: &'static PrefixRetryWritebackOps = KBox::leak(
        KBox::new(PrefixRetryWritebackOps {
            state: Mutex::new(PrefixRetryState {
                fail_once: false,
                calls: 0,
                durable: Vec::new(),
            }),
        })
        .unwrap(),
    );
    let inode = VfsInode::new_file_with_address_space_and_flags(
        Arc::new(TruncateOps),
        NodeFlags::ALWAYS_CACHE,
        operations,
        VfsInodeInit::new(
            31,
            (PAGE_SIZE_4K * 3) as u64,
            Umode::new(NodeType::RegularFile, NodePermission::default()),
        ),
    );
    let mapping = inode.address_space();
    mapping
        .write_cached_bytes(0, &[0x61; PAGE_SIZE_4K * 3])
        .unwrap();
    mapping
        .write_and_wait_range(PAGE_SIZE_4K as u64 - 1..PAGE_SIZE_4K as u64 + 1)
        .unwrap();
    for (index, dirty) in [(0, false), (1, false), (2, true)] {
        mapping
            .i_pages
            .with_folio(index, |folio| {
                let folio = folio.unwrap();
                assert_eq!(folio.is_dirty(), dirty);
                assert!(!folio.is_under_writeback());
            })
            .unwrap();
    }
    assert_eq!(
        operations.state.lock().durable,
        vec![0x61; PAGE_SIZE_4K * 2]
    );
}

struct AcceptedWriteOps {
    accepted: usize,
    entered: AtomicBool,
    released: AtomicBool,
    events: ktask::WaitQueue,
    synced: AtomicUsize,
}

impl AddressSpaceOperations for AcceptedWriteOps {
    fn read_folio(&self, _mapping: &AddressSpace, mut folio: FolioRead) -> VfsResult<()> {
        folio.data().fill(0);
        folio.complete(Ok(()))
    }

    fn write_begin(
        &self,
        _mapping: &AddressSpace,
        _request: crate::WriteBeginRequest,
    ) -> VfsResult<crate::WriteReservation> {
        Ok(crate::WriteReservation::new())
    }

    fn write_end(
        &self,
        mapping: &AddressSpace,
        request: crate::WriteEndRequest,
        _reservation: &crate::WriteReservation,
    ) -> VfsResult<usize> {
        assert!(self.accepted <= request.copied());
        self.entered.store(true, Ordering::Release);
        self.events.notify_all(false);
        self.events
            .wait_until(|| self.released.load(Ordering::Acquire));
        mapping.write_end_set_size(request.pos() + self.accepted as u64)?;
        Ok(self.accepted)
    }

    fn writepages(&self, mapping: &AddressSpace, control: &mut WritebackControl) -> VfsResult<()> {
        mapping.writeback_cached_folios(control, |_, data, valid_len| {
            assert_eq!(valid_len, self.accepted + 1);
            assert_eq!(data, &b"accepted"[..self.accepted + 1]);
            self.synced.fetch_add(valid_len, Ordering::Relaxed);
            Ok(())
        })
    }
}

#[def_test]
fn writeback_waits_for_buffered_acceptance_and_samples_the_published_eof() {
    for accepted in [7, 2] {
        let operations: &'static AcceptedWriteOps = KBox::leak(
            KBox::new(AcceptedWriteOps {
                accepted,
                entered: AtomicBool::new(false),
                released: AtomicBool::new(false),
                events: ktask::WaitQueue::new(),
                synced: AtomicUsize::new(0),
            })
            .unwrap(),
        );
        let inode = VfsInode::new_file_with_address_space_and_flags(
            Arc::new(TruncateOps),
            NodeFlags::ALWAYS_CACHE,
            operations,
            VfsInodeInit::new(
                91,
                1,
                Umode::new(NodeType::RegularFile, NodePermission::default()),
            ),
        );
        let mapping = inode.address_space();
        mapping.write_cached_bytes(0, b"a").unwrap();
        let super_block = crate::nullfs::new_superblock();
        let dentry = crate::Dentry::new_file_from_inode(inode.clone(), None, "accept".into());
        dentry.bind_super_block(&super_block);
        // A process keeps its mount active after closing this temporary file.
        // Otherwise final unmount legitimately synchronizes short-write rollback.
        let mount = crate::Mount::new_root(&super_block);
        let file = crate::VfsFileBuilder::from_path_state(
            crate::Path::new(mount.clone(), dentry),
            crate::FMode::WRITE,
            crate::OpenFlags::empty(),
            Arc::new(TruncateOps),
            kcred::initial_cred(),
        )
        .finish()
        .unwrap();
        let writing = mapping.clone();
        let writer = ktask::spawn(move || {
            let mut source = iov_iter::iov_iter_kvec_source(b"ccepted");
            let mut iocb = crate::Kiocb::new(&file, 1);
            assert_eq!(
                writing.write_iter_with_prepare(&mut iocb, &mut source, |_, _| Ok(())),
                Ok(accepted)
            );
        });
        operations
            .events
            .wait_until(|| operations.entered.load(Ordering::Acquire));
        assert_eq!(inode.size(), 1);
        let started = Arc::new(AtomicBool::new(false));
        let signal = started.clone();
        let syncing = mapping.clone();
        let syncer = ktask::spawn(move || {
            signal.store(true, Ordering::Release);
            operations.events.notify_all(false);
            syncing
                .writepages_control(&mut WritebackControl::all())
                .unwrap();
        });
        operations
            .events
            .wait_until(|| started.load(Ordering::Acquire));
        while syncer.state() != ktask::TaskState::Blocked {
            assert_ne!(syncer.state(), ktask::TaskState::Exited);
            ktask::yield_now();
        }
        assert_eq!(operations.synced.load(Ordering::Relaxed), 0);
        operations.released.store(true, Ordering::Release);
        operations.events.notify_all(false);
        assert_eq!(writer.join(), 0);
        assert_eq!(syncer.join(), 0);
        assert_eq!(inode.size(), (accepted + 1) as u64);
        assert_eq!(operations.synced.load(Ordering::Relaxed), accepted + 1);
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Batched admission/settlement and per-folio buffered-write acceptance tests.

use alloc::{sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};

use iov_iter::{IovSource, iov_iter_source};
use kerrno::{KError, KResult};
use memaddr::PAGE_SIZE_4K;
use pagecache::FolioRead;
use unittest::def_test;

use super::{
    super::{AddressSpace, AddressSpaceOperations, BatchedWriteOperations, WritebackControl},
    TruncateOps,
};
use crate::{NodeFlags, NodePermission, NodeType, Umode, VfsError, VfsInode, VfsInodeInit, VfsResult};

/// Source that serves `limit` bytes and then faults, modelling a bad user buffer.
struct FailingSource {
    total: usize,
    limit: usize,
    written: usize,
}

impl IovSource for FailingSource {
    fn count(&self) -> usize {
        self.total.saturating_sub(self.written)
    }

    fn copy_from_iter(&mut self, dst: &mut [u8]) -> KResult<usize> {
        if self.written >= self.limit {
            return Err(KError::BadAddress);
        }
        let len = (self.limit - self.written).min(dst.len());
        for (offset, byte) in dst[..len].iter_mut().enumerate() {
            *byte = (self.written + offset) as u8;
        }
        self.written += len;
        Ok(len)
    }

    fn revert(&mut self, count: usize) -> KResult<()> {
        self.written = self.written.saturating_sub(count);
        Ok(())
    }
}

struct PrefixWriteOps;

static PREFIX_WRITE_OPERATIONS: PrefixWriteOps = PrefixWriteOps;

impl AddressSpaceOperations for PrefixWriteOps {
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
        mapping.write_end_set_size(request.pos() + request.copied() as u64)?;
        Ok(request.copied())
    }

    fn batched_write_ops(&self) -> Option<&'static dyn BatchedWriteOperations> {
        Some(&PREFIX_WRITE_OPERATIONS)
    }
}

/// Published count passed to the last `settle_write_run`, or `usize::MAX`.
static LAST_SETTLE_PUBLISHED: AtomicUsize = AtomicUsize::new(usize::MAX);

impl BatchedWriteOperations for PrefixWriteOps {
    fn batch_write_begin(
        &self,
        _mapping: &AddressSpace,
        _request: crate::WriteBeginRequest,
    ) -> VfsResult<crate::WriteReservation> {
        Ok(crate::WriteReservation::new())
    }

    fn settle_write_run(
        &self,
        _mapping: &AddressSpace,
        settlement: crate::WriteSettlement,
        _reservation: &crate::WriteReservation,
    ) -> VfsResult<()> {
        LAST_SETTLE_PUBLISHED.store(settlement.published(), Ordering::SeqCst);
        Ok(())
    }
}

#[def_test(serial)]
fn batched_write_failure_reports_the_committed_prefix() {
    let inode = VfsInode::new_file_with_address_space_and_flags(
        Arc::new(TruncateOps),
        NodeFlags::ALWAYS_CACHE,
        &PREFIX_WRITE_OPERATIONS,
        VfsInodeInit::new(
            7,
            1,
            Umode::new(NodeType::RegularFile, NodePermission::default()),
        ),
    );
    let mapping = inode.address_space();
    mapping
        .write_cached_bytes(0, &[0u8; 2 * PAGE_SIZE_4K])
        .unwrap();
    inode.set_size((2 * PAGE_SIZE_4K) as u64);

    let mut source = FailingSource {
        total: 2 * PAGE_SIZE_4K,
        limit: PAGE_SIZE_4K,
        written: 0,
    };
    let mut iter = iov_iter_source(&mut source);

    LAST_SETTLE_PUBLISHED.store(usize::MAX, Ordering::SeqCst);
    let _data_guard = inode.lock_data();
    let written = mapping
        .perform_write(0, &mut iter, 2 * PAGE_SIZE_4K)
        .unwrap();
    assert_eq!(
        written,
        PAGE_SIZE_4K,
        "a partially committed batch must report the written prefix, not the fault"
    );
    assert_eq!(
        LAST_SETTLE_PUBLISHED.load(Ordering::SeqCst),
        PAGE_SIZE_4K,
        "settlement must see exactly the published prefix"
    );
    mapping
        .with_folio(1, |folio| {
            assert_eq!(folio.unwrap().data()[0], 0, "the failed page stays untouched");
        })
        .unwrap();
}

/// Backend that accepts one byte less than the batch copied.
struct ShortAcceptOps;

static SHORT_ACCEPT_OPERATIONS: ShortAcceptOps = ShortAcceptOps;

/// Backend that accepts more bytes than the batch copied.
struct OverAcceptOps;

static OVER_ACCEPT_OPERATIONS: OverAcceptOps = OverAcceptOps;

impl AddressSpaceOperations for ShortAcceptOps {
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
        let accepted = request.copied().saturating_sub(1);
        mapping.write_end_set_size(request.pos() + accepted as u64)?;
        Ok(accepted)
    }
}

impl AddressSpaceOperations for OverAcceptOps {
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
        let accepted = request.copied() + 1;
        mapping.write_end_set_size(request.pos() + accepted as u64)?;
        Ok(accepted)
    }
}

/// Builds a two-page file whose pages are already resident.
///
/// - `operations`: address-space callbacks installed on the inode.
/// - `dirty`: whether the pages start dirty (`write_cached_bytes`) or clean
///   (plain materialization through the read path).
///
/// Returns: the inode paired with its address space.
fn resident_two_page_file(
    operations: &'static dyn AddressSpaceOperations,
    dirty: bool,
) -> (Arc<crate::VfsInode>, Arc<AddressSpace>) {
    let inode = VfsInode::new_file_with_address_space_and_flags(
        Arc::new(TruncateOps),
        NodeFlags::ALWAYS_CACHE,
        operations,
        VfsInodeInit::new(
            21,
            (2 * PAGE_SIZE_4K) as u64,
            Umode::new(NodeType::RegularFile, NodePermission::default()),
        ),
    );
    let mapping = inode.address_space();
    if dirty {
        mapping
            .write_cached_bytes(0, &[0u8; 2 * PAGE_SIZE_4K])
            .expect("populate dirty pages");
    } else {
        for index in 0..2 {
            mapping
                .with_folio_or_create(index, |_| Ok(()))
                .expect("populate a clean resident page");
        }
    }
    (inode, mapping)
}

/// Collects the folio indices one writeback pass charges.
///
/// - `mapping`: address space to scan.
///
/// Returns: ascending page indices submitted through the writeback callback.
fn writeback_visited(mapping: &AddressSpace) -> Vec<u64> {
    let mut visited = Vec::new();
    let mut control = WritebackControl::all().with_nr_to_write(2);
    let _invalidate_guard = mapping.invalidate_lock.read();
    mapping
        .writeback_cached_folios(&mut control, |index, _, _| {
            visited.push(index);
            Ok(())
        })
        .expect("writeback pass completes");
    visited
}

#[def_test]
fn batch_write_over_clean_resident_pages_reaches_the_writeback_index() {
    let (inode, mapping) = resident_two_page_file(&PREFIX_WRITE_OPERATIONS, false);
    assert!(
        writeback_visited(&mapping).is_empty(),
        "clean resident pages must not be dirty-indexed"
    );

    let mut source = FailingSource {
        total: 2 * PAGE_SIZE_4K,
        limit: 2 * PAGE_SIZE_4K,
        written: 0,
    };
    let mut iter = iov_iter_source(&mut source);
    {
        let _data_guard = inode.lock_data();
        let written = mapping
            .perform_write(0, &mut iter, 2 * PAGE_SIZE_4K)
            .expect("clean resident pages take the batch path");
        assert_eq!(written, 2 * PAGE_SIZE_4K);
    }

    assert_eq!(
        writeback_visited(&mapping),
        vec![0, 1],
        "a batched dirty must be visible to writeback and fsync"
    );
}

#[def_test]
fn per_folio_short_accept_reverts_the_iterator_to_the_reported_prefix() {
    let (inode, mapping) = resident_two_page_file(&SHORT_ACCEPT_OPERATIONS, true);
    let mut source = FailingSource {
        total: 2 * PAGE_SIZE_4K,
        limit: 2 * PAGE_SIZE_4K,
        written: 0,
    };
    let mut iter = iov_iter_source(&mut source);
    let written = {
        let _data_guard = inode.lock_data();
        mapping
            .perform_write(0, &mut iter, 2 * PAGE_SIZE_4K)
            .expect("a committed prefix is reported even with a fault")
    };
    assert_eq!(
        written,
        PAGE_SIZE_4K - 1,
        "the non-batched backend must stay on the per-folio path"
    );
    assert_eq!(
        written + iter.count(),
        2 * PAGE_SIZE_4K,
        "the iterator must sit exactly at the unreported tail"
    );
}

#[def_test]
fn per_folio_over_accept_is_rejected() {
    let (inode, mapping) = resident_two_page_file(&OVER_ACCEPT_OPERATIONS, true);
    let mut source = FailingSource {
        total: PAGE_SIZE_4K,
        limit: PAGE_SIZE_4K,
        written: 0,
    };
    let mut iter = iov_iter_source(&mut source);
    let result = {
        let _data_guard = inode.lock_data();
        mapping.perform_write(0, &mut iter, PAGE_SIZE_4K)
    };
    assert!(
        result.is_err(),
        "an over-accepting backend must not report phantom progress: {result:?}"
    );
}

/// Batched backend whose settlement hook reports a bookkeeping failure.
struct SettleFailureOps;

static SETTLE_FAILURE_OPERATIONS: SettleFailureOps = SettleFailureOps;

impl AddressSpaceOperations for SettleFailureOps {
    fn read_folio(&self, _mapping: &AddressSpace, mut folio: FolioRead) -> VfsResult<()> {
        folio.data().fill(0);
        folio.complete(Ok(()))
    }

    fn write_end(
        &self,
        mapping: &AddressSpace,
        request: crate::WriteEndRequest,
        _reservation: &crate::WriteReservation,
    ) -> VfsResult<usize> {
        mapping.write_end_set_size(request.pos() + request.copied() as u64)?;
        Ok(request.copied())
    }

    fn batched_write_ops(&self) -> Option<&'static dyn BatchedWriteOperations> {
        Some(&SETTLE_FAILURE_OPERATIONS)
    }
}

impl BatchedWriteOperations for SettleFailureOps {
    fn batch_write_begin(
        &self,
        _mapping: &AddressSpace,
        _request: crate::WriteBeginRequest,
    ) -> VfsResult<crate::WriteReservation> {
        Ok(crate::WriteReservation::new())
    }

    fn settle_write_run(
        &self,
        _mapping: &AddressSpace,
        _settlement: crate::WriteSettlement,
        _reservation: &crate::WriteReservation,
    ) -> VfsResult<()> {
        Err(VfsError::Io)
    }
}

#[def_test]
fn batched_settle_failure_keeps_the_published_prefix() {
    let (inode, mapping) = resident_two_page_file(&SETTLE_FAILURE_OPERATIONS, false);
    let mut source = FailingSource {
        total: 2 * PAGE_SIZE_4K,
        limit: 2 * PAGE_SIZE_4K,
        written: 0,
    };
    let mut iter = iov_iter_source(&mut source);
    let result = {
        let _data_guard = inode.lock_data();
        mapping.perform_write(0, &mut iter, 2 * PAGE_SIZE_4K)
    };
    assert_eq!(
        result.unwrap(),
        2 * PAGE_SIZE_4K,
        "published bytes stay reported progress even when settlement fails"
    );
    assert_eq!(
        iter.count(),
        0,
        "published bytes must not be handed back to the caller"
    );
    assert_eq!(
        writeback_visited(&mapping),
        vec![0, 1],
        "published data survives a settlement failure and stays writeback-visible"
    );
}

#[def_test]
fn batched_and_per_folio_writes_agree() {
    let (batch_inode, batch_mapping) = resident_two_page_file(&PREFIX_WRITE_OPERATIONS, true);
    let (folio_inode, folio_mapping) = resident_two_page_file(&SHORT_ACCEPT_OPERATIONS, true);

    let mut batch_source = FailingSource {
        total: 2 * PAGE_SIZE_4K,
        limit: PAGE_SIZE_4K,
        written: 0,
    };
    let mut batch_iter = iov_iter_source(&mut batch_source);
    let mut folio_source = FailingSource {
        total: 2 * PAGE_SIZE_4K,
        limit: PAGE_SIZE_4K,
        written: 0,
    };
    let mut folio_iter = iov_iter_source(&mut folio_source);

    // ShortAcceptOps is not batch-capable, so it stays on the per-folio path
    // and accepts one byte less than the batched run reports at the fault
    // boundary; the published prefixes themselves must match.
    let batch_written = {
        let _data_guard = batch_inode.lock_data();
        batch_mapping
            .perform_write(0, &mut batch_iter, 2 * PAGE_SIZE_4K)
            .unwrap()
    };
    let folio_written = {
        let _data_guard = folio_inode.lock_data();
        folio_mapping
            .perform_write(0, &mut folio_iter, 2 * PAGE_SIZE_4K)
            .unwrap()
    };
    assert_eq!(batch_written, PAGE_SIZE_4K);
    assert_eq!(folio_written, PAGE_SIZE_4K - 1);
    for index in 0..2 {
        batch_mapping
            .with_folio(index, |folio| {
                let batch_data = folio.unwrap().data()[..PAGE_SIZE_4K].to_vec();
                let mut same_on_prefix = true;
                folio_mapping
                    .with_folio(index, |other| {
                        let other_data = &other.unwrap().data()[..PAGE_SIZE_4K];
                        let common = batch_written.min(folio_written);
                        same_on_prefix = batch_data[..common] == other_data[..common];
                    })
                    .unwrap();
                assert!(same_on_prefix, "page {index} prefixes must agree");
            })
            .unwrap();
    }
    assert_eq!(
        writeback_visited(&folio_mapping),
        vec![0, 1],
        "per-folio dirty must reach the same writeback index as the batched one"
    );
}
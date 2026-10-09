// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Buffered read boundaries, synchronous readahead and publication.

use alloc::{sync::Arc, vec, vec::Vec};

use iov_iter::{IovSink, iov_iter_dest, iov_iter_kvec_dest};
use kerrno::{KError, KResult};
use memaddr::PAGE_SIZE_4K;
use pagecache::Folio;
use unittest::def_test;

use super::{AddressSpace, TruncateOps};
use crate::{
    AddressSpaceOperations, NodePermission, NodeType, Umode, VfsInode, VfsInodeInit, VfsResult,
};

fn read_inode(ops: &'static dyn AddressSpaceOperations, size: usize) -> Arc<VfsInode> {
    VfsInode::new_file_with_address_space(
        Arc::new(TruncateOps),
        ops,
        VfsInodeInit::new(
            1,
            size as u64,
            Umode::new(NodeType::RegularFile, NodePermission::default()),
        ),
    )
}

struct ReadOps;
static READ_OPS: ReadOps = ReadOps;

impl AddressSpaceOperations for ReadOps {
    fn read_folio(&self, mapping: &AddressSpace, folio: &mut Folio) -> VfsResult<()> {
        let index = folio.index();
        folio.data().fill((index % 251) as u8);
        let len = mapping
            .visible_len()?
            .saturating_sub(folio.index() * PAGE_SIZE_4K as u64)
            .min(PAGE_SIZE_4K as u64) as usize;
        folio.data()[len..].fill(0);
        Ok(())
    }
}

#[def_test]
fn buffered_read_crosses_windows_with_unaligned_eof_and_cache_holes() {
    let file_len = 70 * PAGE_SIZE_4K + 17;
    let inode = read_inode(&READ_OPS, file_len);
    let mapping = inode.address_space();
    for index in (0..70).step_by(2) {
        mapping
            .with_folio_or_create(index, |_| Ok(()))
            .expect("warm alternating pages");
    }
    let offset = 37;
    let mut output = vec![0xff; file_len];
    let mut iter = iov_iter_kvec_dest(&mut output);
    let copied = mapping
        .read_from_page_cache(&mut iter, offset)
        .expect("read across multiple windows");
    assert_eq!(copied, file_len - offset as usize);
    for (position, byte) in output[..copied].iter().enumerate() {
        assert_eq!(
            *byte,
            (((position + offset as usize) / PAGE_SIZE_4K) % 251) as u8
        );
    }
    assert!(output[copied..].iter().all(|byte| *byte == 0xff));
}

struct ShortSink {
    copied: usize,
    limit: usize,
}

impl IovSink for ShortSink {
    fn count(&self) -> usize {
        PAGE_SIZE_4K * 40 - self.copied
    }

    fn copy_to_iter(&mut self, src: &[u8]) -> KResult<usize> {
        let copied = src.len().min(self.limit);
        self.copied += copied;
        Ok(copied)
    }

    fn revert(&mut self, _count: usize) -> KResult<()> {
        Err(KError::InvalidInput)
    }
}

#[def_test]
fn buffered_read_stops_on_short_or_zero_copy() {
    for limit in [0, 23] {
        let inode = read_inode(&READ_OPS, PAGE_SIZE_4K * 40);
        let mapping = inode.address_space();
        let mut sink = ShortSink { copied: 0, limit };
        let mut iter = iov_iter_dest(&mut sink);
        assert_eq!(mapping.read_from_page_cache(&mut iter, 0), Ok(limit));
        assert_eq!(sink.copied, limit);
        // A short copy retains completed prefetch within the 128 KiB window.
        assert_eq!(mapping.nrpages(), 32);
    }
}

struct InvalidatingSink<'a> {
    mapping: &'a AddressSpace,
    output: Vec<u8>,
    truncate: bool,
}

impl IovSink for InvalidatingSink<'_> {
    fn count(&self) -> usize {
        PAGE_SIZE_4K * 3 - self.output.len()
    }

    fn copy_to_iter(&mut self, src: &[u8]) -> KResult<usize> {
        self.output.extend_from_slice(src);
        if self.output.len() == PAGE_SIZE_4K {
            // Remove only future pages: the current folio is still locked by
            // the copy callback. This models residency changing during a read.
            if self.truncate {
                #[expect(
                    clippy::unwrap_used,
                    reason = "CG-021-no-panic-on-reachable-failure: test scenario: the sink \
                              borrows the mapping of a live test inode"
                )]
                self.mapping.inode().unwrap().set_len(PAGE_SIZE_4K as u64)?;
            } else {
                self.mapping
                    .i_pages
                    .resize_cached_folios((PAGE_SIZE_4K * 3) as u64, PAGE_SIZE_4K as u64);
            }
        }
        Ok(src.len())
    }

    fn revert(&mut self, _count: usize) -> KResult<()> {
        Err(KError::InvalidInput)
    }
}

#[def_test]
fn buffered_read_resolves_pages_removed_during_copy() {
    let inode = read_inode(&READ_OPS, PAGE_SIZE_4K * 3);
    let mapping = inode.address_space();
    for index in 0..3 {
        mapping
            .with_folio_or_create(index, |folio| {
                folio.data().fill(0x99);
                Ok(())
            })
            .expect("populate resident pages");
    }
    let mut sink = InvalidatingSink {
        mapping: &mapping,
        output: Vec::new(),
        truncate: false,
    };
    let mut iter = iov_iter_dest(&mut sink);
    assert_eq!(
        mapping.read_from_page_cache(&mut iter, 0),
        Ok(PAGE_SIZE_4K * 3)
    );
    assert!(sink.output[..PAGE_SIZE_4K].iter().all(|byte| *byte == 0x99));
    assert!(
        sink.output[PAGE_SIZE_4K..PAGE_SIZE_4K * 2]
            .iter()
            .all(|byte| *byte == 1)
    );
    assert!(
        sink.output[PAGE_SIZE_4K * 2..]
            .iter()
            .all(|byte| *byte == 2)
    );
}

#[def_test]
fn buffered_read_rechecks_eof_between_folios() {
    let inode = read_inode(&READ_OPS, PAGE_SIZE_4K * 3);
    let mapping = inode.address_space();
    let mut sink = InvalidatingSink {
        mapping: &mapping,
        output: Vec::new(),
        truncate: true,
    };
    assert_eq!(
        mapping.read_from_page_cache(&mut iov_iter_dest(&mut sink), 0),
        Ok(PAGE_SIZE_4K)
    );
    assert_eq!(sink.output.len(), PAGE_SIZE_4K);
    assert_eq!(mapping.nrpages(), 1);
}

struct ReadaheadOps;
static READAHEAD_OPS: ReadaheadOps = ReadaheadOps;

impl AddressSpaceOperations for ReadaheadOps {
    #[expect(
        clippy::panic,
        reason = "CG-021-no-panic-on-reachable-failure: test scenario: rereading a completed \
                  readahead page is the regression this mock reports"
    )]
    fn read_folio(&self, mapping: &AddressSpace, folio: &mut Folio) -> VfsResult<()> {
        // Initialize the isolated miss or retry the consumed, incomplete EOF page.
        assert!(mapping.invalidate_lock.try_write().is_none());
        let byte = match folio.index() {
            0 => 0x11,
            3 => 0x33,
            _ => panic!("completed readahead pages must not be read again"),
        };
        folio.data().fill(byte);
        let len = mapping
            .visible_len()?
            .saturating_sub(folio.index() * PAGE_SIZE_4K as u64)
            .min(PAGE_SIZE_4K as u64) as usize;
        folio.data()[len..].fill(0);
        Ok(())
    }

    #[expect(
        clippy::panic,
        reason = "CG-021-no-panic-on-reachable-failure: test scenario: an unexpected readahead \
                  request is the regression this mock reports"
    )]
    fn readahead(&self, control: &mut crate::ReadaheadControl<'_, '_>) -> VfsResult<()> {
        assert!(control.mapping().invalidate_lock.try_write().is_none());
        match control.index() {
            0 => panic!("single-page misses must use read_folio"),
            2 => {
                assert_eq!(control.count(), 2);
                // The run is already resident and locked; completion fills
                // those folios instead of racing to insert new backing copies.
                assert_eq!(control.complete_folio(&vec![0x22; PAGE_SIZE_4K]), Ok(()));
                assert_eq!(control.index(), 3);
                assert_eq!(control.count(), 1);
                assert_eq!(control.complete_folio(&[0x33]), Err(KError::InvalidInput));
                assert_eq!(control.index(), 4);
                assert_eq!(control.count(), 0);
                assert_eq!(control.complete_folio(&[]), Err(KError::InvalidInput));
                Err(KError::Io)
            }
            _ => panic!("resident folios must not be submitted for readahead"),
        }
    }
}

#[def_test]
fn readahead_reads_singletons_preserves_residents_and_completes_eof() {
    let size = PAGE_SIZE_4K * 3 + 17;
    let small_inode = read_inode(&READAHEAD_OPS, size);
    let mut byte = [0];
    assert_eq!(
        small_inode
            .address_space()
            .read_from_page_cache(&mut iov_iter_kvec_dest(&mut byte), 0),
        Ok(1)
    );
    assert_eq!(byte[0], 0x11);
    let inode = read_inode(&READAHEAD_OPS, size);
    let mapping = inode.address_space();
    mapping
        .i_pages
        .filemap_add_folio(super::folio_from_bytes(1, &vec![0x99; PAGE_SIZE_4K]))
        .unwrap_or_else(|_| panic!("fixture index is absent"));
    let mut output = vec![0xff; size + 10];
    assert_eq!(
        mapping.read_from_page_cache(&mut iov_iter_kvec_dest(&mut output), 0),
        Ok(size)
    );
    assert!(output[..PAGE_SIZE_4K].iter().all(|&byte| byte == 0x11));
    assert!(
        output[PAGE_SIZE_4K..PAGE_SIZE_4K * 2]
            .iter()
            .all(|&byte| byte == 0x99)
    );
    assert!(
        output[PAGE_SIZE_4K * 2..PAGE_SIZE_4K * 3]
            .iter()
            .all(|&byte| byte == 0x22)
    );
    assert!(
        output[PAGE_SIZE_4K * 3..size]
            .iter()
            .all(|&byte| byte == 0x33)
    );
    assert!(output[size..].iter().all(|&byte| byte == 0xff));
    mapping.with_folio(3, |folio| {
        assert!(folio.unwrap().data()[17..].iter().all(|&byte| byte == 0))
    });
}

struct ReadGate {
    entered: core::sync::atomic::AtomicBool,
    release: core::sync::atomic::AtomicBool,
    events: ktask::WaitQueue,
}

impl crate::FileOperations for ReadGate {}
impl crate::InodeOperations for ReadGate {
    fn set_len(&self, inode: &VfsInode, len: u64) -> VfsResult<()> {
        inode.truncate_setsize(len)
    }
}

struct GatedReadOps;
static GATED_READ_OPS: GatedReadOps = GatedReadOps;

impl AddressSpaceOperations for GatedReadOps {
    fn readahead(&self, control: &mut crate::ReadaheadControl<'_, '_>) -> VfsResult<()> {
        use core::sync::atomic::Ordering;
        #[expect(
            clippy::unwrap_used,
            reason = "CG-021-no-panic-on-reachable-failure: test scenario: the gated mapping \
                      belongs to a live test inode"
        )]
        let inode = control.mapping().inode().unwrap();
        let gate = inode.private::<ReadGate>()?;
        gate.entered.store(true, Ordering::Release);
        gate.events.notify_all(false);
        gate.events
            .wait_until(|| gate.release.load(Ordering::Acquire));
        while control.count() != 0 {
            control.complete_folio(&vec![0x5a; PAGE_SIZE_4K])?;
        }
        Ok(())
    }
}

#[def_test]
fn synchronous_readahead_publication_excludes_truncate() {
    use core::sync::atomic::{AtomicBool, Ordering};
    let gate = Arc::new(ReadGate {
        entered: AtomicBool::new(false),
        release: AtomicBool::new(false),
        events: ktask::WaitQueue::new(),
    });
    let inode = VfsInode::new_file_with_address_space(
        gate.clone(),
        &GATED_READ_OPS,
        VfsInodeInit::new(
            2,
            (PAGE_SIZE_4K * 2) as u64,
            Umode::new(NodeType::RegularFile, NodePermission::default()),
        ),
    );
    let reader_inode = inode.clone();
    let reader = ktask::spawn(move || {
        let mut output = vec![0; PAGE_SIZE_4K * 2];
        reader_inode
            .address_space()
            .read_from_page_cache(&mut iov_iter_kvec_dest(&mut output), 0)
            .expect("read completes");
    });
    gate.events
        .wait_until(|| gate.entered.load(Ordering::Acquire));
    let mapping = inode.address_space();
    let publication_excludes_truncate = mapping.invalidate_lock.try_write().is_none();
    let reserved = mapping.nrpages() == 2;
    gate.release.store(true, Ordering::Release);
    gate.events.notify_all(false);
    // This may start before or after initialization. Either ordering must leave
    // no late completion that can repopulate the discarded range.
    inode.set_len(17).unwrap();
    reader.join();
    assert!(publication_excludes_truncate);
    assert!(reserved);
    assert_eq!(mapping.nrpages(), 1);
    mapping.with_folio(0, |folio| {
        let folio = folio.unwrap();
        assert!(folio.data()[..17].iter().all(|&byte| byte == 0x5a));
        assert!(folio.data()[17..].iter().all(|&byte| byte == 0));
    });
}

struct GrowingReadOps;
static GROWING_READ_OPS: GrowingReadOps = GrowingReadOps;

impl AddressSpaceOperations for GrowingReadOps {
    fn read_folio(&self, mapping: &AddressSpace, folio: &mut Folio) -> VfsResult<()> {
        // Model an old-EOF read finishing while a write publishes a larger
        // size. The filesystem has initialized both the data and zero tail.
        folio.data().fill(0);
        folio.data()[..17].fill(0x6b);
        mapping.write_end_set_size(PAGE_SIZE_4K as u64)?;
        Ok(())
    }
}

#[def_test]
fn completed_folio_read_survives_eof_growth() {
    let inode = read_inode(&GROWING_READ_OPS, 17);
    let mapping = inode.address_space();
    let mut output = [0xff; PAGE_SIZE_4K];
    let mut iter = iov_iter_kvec_dest(&mut output);
    assert_eq!(mapping.read_from_page_cache(&mut iter, 0), Ok(PAGE_SIZE_4K));
    assert!(output[..17].iter().all(|byte| *byte == 0x6b));
    assert!(output[17..].iter().all(|byte| *byte == 0));
}

struct FailingFallbackOps;
static FAILING_FALLBACK_OPS: FailingFallbackOps = FailingFallbackOps;

impl AddressSpaceOperations for FailingFallbackOps {
    fn read_folio(&self, mapping: &AddressSpace, folio: &mut Folio) -> VfsResult<()> {
        assert!(mapping.invalidate_lock.try_write().is_none());
        // Looking up the index must remain possible during backend I/O.
        assert_eq!(mapping.nrpages(), 3);
        let index = folio.index();
        folio.data().fill(index as u8);
        if folio.index() == 1 {
            return Err(KError::Io);
        }
        Ok(())
    }
}

#[def_test]
fn default_readahead_initializes_reservations_after_a_read_error() {
    let inode = read_inode(&FAILING_FALLBACK_OPS, 3 * PAGE_SIZE_4K);
    let mapping = inode.address_space();
    let _invalidate = mapping.invalidate_lock.read();
    assert_eq!(
        mapping.sync_readahead(0, 3 * PAGE_SIZE_4K, 3 * PAGE_SIZE_4K as u64),
        3 * PAGE_SIZE_4K as u64
    );
    assert_eq!(mapping.nrpages(), 3);
    for index in [0, 2] {
        mapping.with_folio(index, |folio| {
            assert!(
                folio
                    .unwrap()
                    .data()
                    .iter()
                    .all(|&byte| byte == index as u8)
            );
        });
    }
    mapping.with_folio(1, |folio| assert!(folio.is_none()));
    mapping
        .i_pages
        .with_folio_or_create(
            1,
            |folio| {
                folio.data().fill(0x55);
                Ok(())
            },
            |folio| {
                assert!(folio.is_uptodate());
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(mapping.nrpages(), 3);
}

// Owned by this test's inode: no global hook or scheduling assumptions.
struct ReadaheadConflict;

impl crate::FileOperations for ReadaheadConflict {}
impl crate::InodeOperations for ReadaheadConflict {}

pub(crate) fn observe_readahead_run(mapping: &AddressSpace, run: &core::ops::Range<u64>) {
    let Some(inode) = mapping.inode() else { return };
    let Ok(_) = inode.private::<ReadaheadConflict>() else {
        return;
    };
    if run.start == 4 {
        assert_eq!(run, &(4..8));
        // Model a competing publisher after the missing-run hint was sampled.
        #[expect(
            clippy::panic,
            reason = "CG-021-no-panic-on-reachable-failure: test scenario: a resident first \
                      hinted page is the regression this observer reports"
        )]
        mapping
            .i_pages
            .filemap_add_folio(super::folio_from_bytes(
                run.start,
                &vec![0x44; PAGE_SIZE_4K],
            ))
            .unwrap_or_else(|_| panic!("first hinted page must still be absent"));
    } else {
        assert_eq!(run, &(5..8));
    }
}

static CONFLICTED_READAHEAD_OPS: ReadaheadConflict = ReadaheadConflict;

impl AddressSpaceOperations for ReadaheadConflict {
    #[expect(
        clippy::panic,
        reason = "CG-021-no-panic-on-reachable-failure: test scenario: a demand read of the \
                  remaining run is the regression this mock reports"
    )]
    fn read_folio(&self, _mapping: &AddressSpace, _folio: &mut Folio) -> VfsResult<()> {
        panic!("the remaining run must be initialized by batch readahead");
    }

    fn readahead(&self, control: &mut crate::ReadaheadControl<'_, '_>) -> VfsResult<()> {
        assert_eq!(control.index(), 5);
        assert_eq!(control.count(), 3);
        while control.count() != 0 {
            control.complete_folio(&vec![0x55; PAGE_SIZE_4K])?;
        }
        Ok(())
    }
}

#[def_test]
fn readahead_preserves_remaining_run_after_first_page_publication_conflict() {
    let inode = VfsInode::new_file_with_address_space(
        Arc::new(ReadaheadConflict),
        &CONFLICTED_READAHEAD_OPS,
        VfsInodeInit::new(
            1,
            8 * PAGE_SIZE_4K as u64,
            Umode::new(NodeType::RegularFile, NodePermission::default()),
        ),
    );
    let mapping = inode.address_space();
    let _invalidate = mapping.invalidate_lock.read();
    assert_eq!(
        mapping.sync_readahead(
            4 * PAGE_SIZE_4K as u64,
            4 * PAGE_SIZE_4K,
            8 * PAGE_SIZE_4K as u64
        ),
        8 * PAGE_SIZE_4K as u64
    );
    assert_eq!(mapping.nrpages(), 4);
    for index in 4..8 {
        mapping.with_folio(index, |folio| {
            let expected = if index == 4 { 0x44 } else { 0x55 };
            assert!(
                folio
                    .expect("entire window must be initialized")
                    .data()
                    .iter()
                    .all(|&byte| byte == expected)
            );
        });
    }
}

struct WindowReadOps;
static WINDOW_READ_OPS: WindowReadOps = WindowReadOps;

impl AddressSpaceOperations for WindowReadOps {
    fn read_folio(&self, mapping: &AddressSpace, folio: &mut Folio) -> VfsResult<()> {
        assert!(mapping.invalidate_lock.try_write().is_none());
        assert_eq!(
            folio.index(),
            31,
            "only the aligned window has a one-page tail"
        );
        folio.data().fill(31);
        Ok(())
    }

    fn readahead(&self, control: &mut crate::ReadaheadControl<'_, '_>) -> VfsResult<()> {
        assert!(
            control.count() > 1,
            "single reservations must use read_folio"
        );
        while control.count() != 0 {
            let page = vec![control.index() as u8; PAGE_SIZE_4K];
            control.complete_folio(&page)?;
        }
        Ok(())
    }
}

#[def_test]
fn readahead_completes_full_windows_across_batch_boundaries() {
    let window_bytes = 128 * 1024;
    for offset in [0, 1] {
        let inode = read_inode(&WINDOW_READ_OPS, window_bytes + PAGE_SIZE_4K);
        let mapping = inode.address_space();
        let _invalidate = mapping.invalidate_lock.read();
        let end = offset + window_bytes as u64;
        assert_eq!(
            mapping.sync_readahead(offset, window_bytes, inode.size()),
            end
        );
        let pages = end.div_ceil(PAGE_SIZE_4K as u64);
        assert_eq!(mapping.nrpages(), pages);
        for index in 0..pages {
            mapping.with_folio(index, |folio| {
                assert!(
                    folio
                        .expect("window tail must be initialized before demand read")
                        .data()
                        .iter()
                        .all(|&byte| byte == index as u8)
                );
            });
        }
    }
}

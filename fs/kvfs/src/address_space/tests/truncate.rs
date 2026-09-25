// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Size publication, view invalidation and final teardown tests.

use alloc::{
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};

use ksync::Mutex;
use memaddr::PAGE_SIZE_4K;
use pagecache::Folio;
use unittest::def_test;
use vmobj::{
    MappingViewKind, MappingViewNotifier, MappingViewSpec, ObjectInvalidateWork, ObjectViewHit,
};

use super::{
    super::{AddressSpace, AddressSpaceOperations, WritebackControl},
    TRUNCATE_OPERATIONS, TruncateOps,
};
use crate::{NodeFlags, NodePermission, NodeType, Umode, VfsInode, VfsInodeInit, VfsResult};

struct RecordingNotifier {
    address_space: Weak<AddressSpace>,
    observed_sizes: Mutex<Vec<u64>>,
}

impl MappingViewNotifier for RecordingNotifier {
    fn invalidate(&self, _work: &ObjectInvalidateWork, _hit: &ObjectViewHit) {
        let size = self
            .address_space
            .upgrade()
            .and_then(|mapping| mapping.inode())
            .map(|inode| inode.size())
            .expect("registered address space remains attached to its inode");
        self.observed_sizes.lock().push(size);
    }
}

struct PanicWritebackOps;

static PANIC_WRITEBACK_OPERATIONS: PanicWritebackOps = PanicWritebackOps;

impl AddressSpaceOperations for PanicWritebackOps {
    fn read_folio(&self, _mapping: &AddressSpace, folio: &mut Folio) -> VfsResult<usize> {
        folio.data().fill(0);
        Ok(folio.data().len())
    }

    fn writepages(
        &self,
        _mapping: &AddressSpace,
        _control: &mut WritebackControl,
    ) -> VfsResult<()> {
        panic!("final eviction must not call ordinary writeback")
    }
}

/// Verifies that final address-space teardown discards cached folios
/// without forcing ordinary writeback.
#[def_test]
fn evict_discards_dirty_folios_without_writeback() {
    let address_space = AddressSpace::new(Weak::new(), &PANIC_WRITEBACK_OPERATIONS);
    address_space
        .write_cached_bytes(0, b"hello world")
        .expect("page-cache write should succeed");

    // Evict — this is the code path that was broken.
    address_space.evict().expect("evict should succeed");

    assert_eq!(address_space.nrpages(), 0);
}

/// Verifies that `evict()` on a *clean* address space is harmless and
/// does not spuriously write back clean folios.
#[def_test]
fn evict_on_clean_address_space_writes_back_zero_folios() {
    let address_space = AddressSpace::new(Weak::new(), &PANIC_WRITEBACK_OPERATIONS);

    // Evict on a clean cache should succeed.
    address_space.evict().expect("evict should succeed");
}

#[def_test]
fn truncate_publishes_i_size_before_two_mapping_invalidations() {
    let inode = VfsInode::new_file_with_address_space_and_flags(
        Arc::new(TruncateOps),
        NodeFlags::ALWAYS_CACHE,
        &TRUNCATE_OPERATIONS,
        VfsInodeInit::new(
            1,
            (PAGE_SIZE_4K * 2) as u64,
            Umode::new(NodeType::RegularFile, NodePermission::default()),
        ),
    );
    let address_space = inode.address_space();
    address_space
        .i_pages
        .filemap_add_folio(1, 0, b"cached tail")
        .expect("cache tail page");
    let notifier = Arc::new(RecordingNotifier {
        address_space: Arc::downgrade(&address_space),
        observed_sizes: Mutex::new(Vec::new()),
    });
    let _view = address_space.register_view(MappingViewSpec {
        mm_id: 1,
        vma_start: 0x4000,
        vma_len: PAGE_SIZE_4K * 2,
        object_start: 0,
        object_len: PAGE_SIZE_4K * 2,
        kind: MappingViewKind::Private,
        notifier: Some(notifier.clone()),
    });

    inode.set_len(0).expect("truncate succeeds");

    assert_eq!(inode.size(), 0);
    assert_eq!(address_space.nrpages(), 0);
    assert_eq!(*notifier.observed_sizes.lock(), [0, 0]);
}

#[def_test]
fn truncate_shrink_then_regrow_does_not_reveal_discarded_cache() {
    let old_len = (PAGE_SIZE_4K * 2) as u64;
    let inode = VfsInode::new_file_with_address_space_and_flags(
        Arc::new(TruncateOps),
        NodeFlags::ALWAYS_CACHE,
        &TRUNCATE_OPERATIONS,
        VfsInodeInit::new(
            2,
            old_len,
            Umode::new(NodeType::RegularFile, NodePermission::default()),
        ),
    );
    let address_space = inode.address_space();
    let stale = vec![0x5a; PAGE_SIZE_4K];
    address_space
        .i_pages
        .filemap_add_folio(0, 0, &stale)
        .expect("cache first page");
    address_space
        .i_pages
        .filemap_add_folio(1, 0, &stale)
        .expect("cache second page");

    inode.set_len(0).expect("shrink succeeds");
    assert_eq!(inode.size(), 0);
    assert_eq!(address_space.nrpages(), 0);

    inode.set_len(old_len).expect("regrow succeeds");
    assert_eq!(inode.size(), old_len);
    assert_eq!(address_space.nrpages(), 0);
    address_space
        .with_folio_or_create(0, |folio| {
            assert!(folio.data().iter().all(|byte| *byte == 0));
            Ok(())
        })
        .expect("read regrown sparse page");
}

#[def_test]
fn truncate_partial_page_growth_zeros_cached_old_eof() {
    let old_len = (PAGE_SIZE_4K / 4) as u64;
    let new_len = (PAGE_SIZE_4K / 2) as u64;
    let inode = VfsInode::new_file_with_address_space_and_flags(
        Arc::new(TruncateOps),
        NodeFlags::ALWAYS_CACHE,
        &TRUNCATE_OPERATIONS,
        VfsInodeInit::new(
            3,
            old_len,
            Umode::new(NodeType::RegularFile, NodePermission::default()),
        ),
    );
    let address_space = inode.address_space();
    let stale = vec![0x5a; PAGE_SIZE_4K];
    address_space
        .i_pages
        .filemap_add_folio(0, 0, &stale)
        .expect("cache partial EOF page");

    inode
        .set_len(new_len)
        .expect("partial-page growth succeeds");

    assert_eq!(inode.size(), new_len);
    address_space.with_folio(0, |folio| {
        let data = folio.expect("EOF folio remains cached").data();
        assert!(data[..old_len as usize].iter().all(|byte| *byte == 0x5a));
        assert!(
            data[old_len as usize..new_len as usize]
                .iter()
                .all(|byte| *byte == 0)
        );
    });
}

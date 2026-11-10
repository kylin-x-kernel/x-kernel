// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Size publication, view invalidation and final teardown tests.

use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};

use ksync::Mutex;
use memaddr::PAGE_SIZE_4K;
use pagecache::FolioRead;
use unittest::def_test;
use vmobj::{
    MappingViewKind, MappingViewNotifier, MappingViewSpec, ObjectInvalidateWork, ObjectViewHit,
};

use super::{
    super::{AddressSpace, AddressSpaceOperations, WritebackControl},
    TRUNCATE_OPERATIONS, TruncateOps,
};
use crate::{
    Dentry, FileOperations, InodeOperations, MetadataUpdate, NodeFlags, NodePermission, NodeType,
    Umode, VfsError, VfsInode, VfsInodeInit, VfsResult,
};

struct CacheOnlyFileOps;

impl InodeOperations for CacheOnlyFileOps {}
impl FileOperations for CacheOnlyFileOps {}

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
    fn read_folio(&self, _mapping: &AddressSpace, mut folio: FolioRead) -> VfsResult<()> {
        folio.data().fill(0);
        folio.complete(Ok(()))
    }

    // Ordinary writeback during final eviction is the regression this mock reports.
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
    let inode = VfsInode::new_file_with_address_space(
        Arc::new(CacheOnlyFileOps),
        &PANIC_WRITEBACK_OPERATIONS,
        VfsInodeInit::new(
            1,
            11,
            Umode::new(NodeType::RegularFile, NodePermission::default()),
        ),
    );
    let address_space = inode.address_space();
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
fn default_cached_file_inode_truncates_without_filesystem_set_len() {
    let old_len = (PAGE_SIZE_4K * 2) as u64;
    let inode = VfsInode::new_file_with_flags(
        Arc::new(CacheOnlyFileOps),
        NodeFlags::ALWAYS_CACHE,
        VfsInodeInit::new(
            4,
            old_len,
            Umode::new(NodeType::RegularFile, NodePermission::default()),
        ),
    );
    let address_space = inode.address_space();
    address_space
        .write_cached_bytes(PAGE_SIZE_4K as u64, b"stale")
        .expect("cache file data");

    inode.set_len(0).expect("shrink cache-only file");
    assert_eq!(inode.size(), 0);
    assert_eq!(address_space.nrpages(), 0);

    inode.set_len(old_len).expect("regrow cache-only file");
    address_space
        .with_folio_or_create(1, |folio| {
            assert!(folio.data().iter().all(|byte| *byte == 0));
            Ok(())
        })
        .expect("regrown bytes are zero");
}

/// Verifies that a size reported by `setattr` is published with the same
/// cache invalidation as a backing-store truncate, even for `NON_CACHEABLE`
/// inodes whose truncate bypasses `set_len`.
#[def_test]
fn setattr_size_invalidates_cached_folios() {
    let old_len = PAGE_SIZE_4K as u64;
    let inode = VfsInode::new_file_with_address_space_and_flags(
        Arc::new(TruncateOps),
        NodeFlags::NON_CACHEABLE,
        &TRUNCATE_OPERATIONS,
        VfsInodeInit::new(
            5,
            old_len,
            Umode::new(NodeType::RegularFile, NodePermission::default()),
        ),
    );
    let dentry = Dentry::new_file_from_inode(inode.clone(), None, String::from("cached"));
    let address_space = inode.address_space();
    address_space
        .i_pages
        .filemap_add_folio(super::folio_from_bytes(0, b"stale"))
        .unwrap_or_else(|_| panic!("fixture index is absent"));

    dentry
        .update_metadata(MetadataUpdate {
            size: Some(0),
            ..Default::default()
        })
        .expect("setattr size update succeeds");

    assert_eq!(inode.size(), 0);
    assert_eq!(address_space.nrpages(), 0);
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
        .filemap_add_folio(super::folio_from_bytes(1, b"cached tail"))
        .unwrap_or_else(|_| panic!("fixture index is absent"));
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
        .filemap_add_folio(super::folio_from_bytes(0, &stale))
        .unwrap_or_else(|_| panic!("fixture index is absent"));
    address_space
        .i_pages
        .filemap_add_folio(super::folio_from_bytes(1, &stale))
        .unwrap_or_else(|_| panic!("fixture index is absent"));

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
        .filemap_add_folio(super::folio_from_bytes(0, &stale))
        .unwrap_or_else(|_| panic!("fixture index is absent"));

    inode
        .set_len(new_len)
        .expect("partial-page growth succeeds");

    assert_eq!(inode.size(), new_len);
    address_space
        .with_folio(0, |folio| {
            let data = folio.expect("EOF folio remains cached").data();
            assert!(data[..old_len as usize].iter().all(|byte| *byte == 0x5a));
            assert!(
                data[old_len as usize..new_len as usize]
                    .iter()
                    .all(|byte| *byte == 0)
            );
        })
        .unwrap();
}

struct OnlineNotifier {
    mapping: Weak<AddressSpace>,
    attempts: Mutex<usize>,
}

impl MappingViewNotifier for OnlineNotifier {
    fn invalidate(&self, _work: &ObjectInvalidateWork, _hit: &ObjectViewHit) {
        panic!("live invalidation must wait for completed revocation");
    }

    fn try_invalidate(
        &self,
        _work: &ObjectInvalidateWork,
        _hit: &ObjectViewHit,
    ) -> VfsResult<bool> {
        assert_eq!(self.mapping.upgrade().unwrap().nrpages(), 1);
        let mut attempts = self.attempts.lock();
        *attempts += 1;
        Ok(*attempts > 1)
    }
}

#[def_test]
fn online_invalidation_retains_cache_until_revocation_completes() {
    let inode = VfsInode::new_file_with_address_space(
        Arc::new(TruncateOps),
        &TRUNCATE_OPERATIONS,
        VfsInodeInit::new(
            7,
            PAGE_SIZE_4K as u64,
            Umode::new(NodeType::RegularFile, NodePermission::default()),
        ),
    );
    let mapping = inode.address_space();
    mapping
        .i_pages
        .filemap_add_folio(super::folio_from_bytes(0, b"live"))
        .unwrap();
    let notifier = Arc::new(OnlineNotifier {
        mapping: Arc::downgrade(&mapping),
        attempts: Mutex::new(0),
    });
    let _view = mapping.register_view(MappingViewSpec {
        mm_id: 1,
        vma_start: 0x4000,
        vma_len: PAGE_SIZE_4K,
        object_start: 0,
        object_len: PAGE_SIZE_4K,
        kind: MappingViewKind::Shared,
        notifier: Some(notifier.clone()),
    });
    inode.invalidate_cached_pages().unwrap();
    assert_eq!(*notifier.attempts.lock(), 2);
    assert_eq!(mapping.nrpages(), 0);
    assert_eq!(inode.size(), PAGE_SIZE_4K as u64);
}

#[def_test]
fn live_invalidation_rejects_deferred_only_notifiers_without_freeing_pages() {
    let inode = VfsInode::new_file_with_address_space(
        Arc::new(TruncateOps),
        &TRUNCATE_OPERATIONS,
        VfsInodeInit::new(
            8,
            PAGE_SIZE_4K as u64,
            Umode::new(NodeType::RegularFile, NodePermission::default()),
        ),
    );
    let mapping = inode.address_space();
    mapping
        .i_pages
        .filemap_add_folio(super::folio_from_bytes(0, b"retained"))
        .unwrap();
    let _view = mapping.register_view(MappingViewSpec {
        mm_id: 1,
        vma_start: 0x4000,
        vma_len: PAGE_SIZE_4K,
        object_start: 0,
        object_len: PAGE_SIZE_4K,
        kind: MappingViewKind::Shared,
        notifier: Some(Arc::new(RecordingNotifier {
            address_space: Arc::downgrade(&mapping),
            observed_sizes: Mutex::new(Vec::new()),
        })),
    });
    assert_eq!(
        inode
            .invalidate_cached_pages()
            .map_err(kerrno::LinuxError::from),
        Err(kerrno::LinuxError::EOPNOTSUPP)
    );
    assert_eq!(mapping.nrpages(), 1);
}

struct RangeNotifier {
    result: bool,
    observed: Mutex<Vec<(u64, usize)>>,
}

impl MappingViewNotifier for RangeNotifier {
    fn invalidate(&self, _work: &ObjectInvalidateWork, _hit: &ObjectViewHit) {}

    fn try_invalidate(
        &self,
        _work: &ObjectInvalidateWork,
        hit: &ObjectViewHit,
    ) -> kerrno::KResult<bool> {
        self.observed
            .lock()
            .push((hit.object_start(), hit.object_len()));
        Ok(self.result)
    }
}

fn range_inode(id: u64) -> (Arc<VfsInode>, Arc<AddressSpace>) {
    let inode = VfsInode::new_file_with_address_space(
        Arc::new(TruncateOps),
        &TRUNCATE_OPERATIONS,
        VfsInodeInit::new(
            id,
            (PAGE_SIZE_4K * 2) as u64,
            Umode::new(NodeType::RegularFile, NodePermission::default()),
        ),
    );
    let mapping = inode.address_space();
    (inode, mapping)
}

fn register_range_view(
    mapping: &Arc<AddressSpace>,
    object_len: usize,
    result: bool,
) -> (Arc<RangeNotifier>, crate::AddressSpaceViewGuard) {
    let notifier = Arc::new(RangeNotifier {
        result,
        observed: Mutex::new(Vec::new()),
    });
    let view = mapping.register_view(MappingViewSpec {
        mm_id: 1,
        vma_start: 0x4000,
        vma_len: object_len,
        object_start: 0,
        object_len,
        kind: MappingViewKind::Shared,
        notifier: Some(notifier.clone()),
    });
    (notifier, view)
}

#[def_test]
fn invalidate_range_reports_busy_without_discarding() {
    let (_inode, mapping) = range_inode(20);
    mapping
        .i_pages
        .filemap_add_folio(super::folio_from_bytes(0, b"kept"))
        .unwrap();
    let (_notifier, _view) = register_range_view(&mapping, PAGE_SIZE_4K, false);

    assert!(!mapping.invalidate_range(0, PAGE_SIZE_4K as u64).unwrap());
    assert_eq!(mapping.nrpages(), 1);
}

#[def_test]
fn invalidate_range_discards_clean_folios_after_unmap_completes() {
    let (_inode, mapping) = range_inode(21);
    mapping
        .i_pages
        .filemap_add_folio(super::folio_from_bytes(0, b"stale"))
        .unwrap();
    let (_notifier, _view) = register_range_view(&mapping, PAGE_SIZE_4K, true);

    assert!(mapping.invalidate_range(0, PAGE_SIZE_4K as u64).unwrap());
    assert_eq!(mapping.nrpages(), 0);
}

#[def_test]
fn invalidate_range_keeps_dirty_folios_and_reports_busy() {
    let (_inode, mapping) = range_inode(22);
    mapping
        .write_cached_bytes(0, b"dirty")
        .expect("cache write should dirty the folio");
    let (_notifier, _view) = register_range_view(&mapping, PAGE_SIZE_4K, true);

    assert!(matches!(
        mapping.invalidate_range(0, PAGE_SIZE_4K as u64),
        Err(VfsError::ResourceBusy)
    ));
    assert_eq!(mapping.nrpages(), 1);
}

#[def_test]
fn invalidate_range_ignores_empty_and_inverted_ranges() {
    let (_inode, mapping) = range_inode(23);
    mapping
        .i_pages
        .filemap_add_folio(super::folio_from_bytes(0, b"kept"))
        .unwrap();

    assert!(mapping.invalidate_range(PAGE_SIZE_4K as u64, PAGE_SIZE_4K as u64).unwrap());
    assert!(mapping.invalidate_range(PAGE_SIZE_4K as u64, 0).unwrap());
    assert_eq!(mapping.nrpages(), 1);
}

#[def_test]
fn invalidate_range_clips_the_view_hit_to_the_requested_range() {
    let (_inode, mapping) = range_inode(24);
    mapping
        .i_pages
        .filemap_add_folio(super::folio_from_bytes(0, b"first"))
        .unwrap();
    mapping
        .i_pages
        .filemap_add_folio(super::folio_from_bytes(1, b"second"))
        .unwrap();
    let (notifier, _view) = register_range_view(&mapping, PAGE_SIZE_4K * 2, true);

    assert!(mapping.invalidate_range(0, PAGE_SIZE_4K as u64).unwrap());
    assert_eq!(*notifier.observed.lock(), [(0, PAGE_SIZE_4K)]);
    assert_eq!(mapping.nrpages(), 1);
}

#[def_test]
fn invalidate_range_rejects_views_without_notifier() {
    let (_inode, mapping) = range_inode(25);
    let _view = mapping.register_view(MappingViewSpec {
        mm_id: 1,
        vma_start: 0x4000,
        vma_len: PAGE_SIZE_4K,
        object_start: 0,
        object_len: PAGE_SIZE_4K,
        kind: MappingViewKind::Shared,
        notifier: None,
    });

    assert!(matches!(
        mapping.invalidate_range(0, PAGE_SIZE_4K as u64),
        Err(VfsError::OperationNotSupported)
    ));
}

#[def_test]
fn try_invalidate_mappings_range_treats_inverted_range_as_empty() {
    let (_inode, mapping) = range_inode(26);
    let (_notifier, _view) = register_range_view(&mapping, PAGE_SIZE_4K, false);

    assert!(
        mapping
            .try_invalidate_mappings_range(PAGE_SIZE_4K as u64, Some(0))
            .unwrap()
    );
}

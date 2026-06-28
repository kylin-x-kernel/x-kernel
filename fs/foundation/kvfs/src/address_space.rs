// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Inode address-space objects.
//!
//! Linux hangs the page-cache and backing-store operation boundary off
//! `inode->i_mapping`. This module provides the VFS-owned object that will
//! become that attachment point as filesystems migrate away from byte-level
//! `FileNodeOps` I/O.

use alloc::sync::Arc;

use kerrno::KResult;
use memaddr::PAGE_SIZE_4K;
use pagecache::{Folio, Mapping, MappingKind, MappingOps, PageIndex};

use crate::{
    AddressSpaceOperations, FileNode, Mutex, MutexGuard, NodeFlags, NodeType, TypeMap, VfsError,
    VfsResult, WeakVfsInode, WriteBeginRequest, WriteEndRequest, WritebackControl,
};

/// VFS address space for one inode.
pub struct AddressSpace {
    inode: WeakVfsInode,
    ops: Arc<dyn AddressSpaceOperations>,
    page_cache: Mutex<Option<Arc<Mapping>>>,
    data: Mutex<TypeMap>,
}

impl AddressSpace {
    /// Creates an address space for `inode`.
    pub fn new(inode: WeakVfsInode, ops: Arc<dyn AddressSpaceOperations>) -> Self {
        Self {
            inode,
            ops,
            page_cache: Mutex::default(),
            data: Mutex::default(),
        }
    }

    /// Returns the inode owning this address space.
    pub fn inode(&self) -> Option<alloc::sync::Arc<crate::VfsInode>> {
        self.inode.upgrade()
    }

    /// Returns the backing address-space operations.
    pub fn operations(&self) -> &Arc<dyn AddressSpaceOperations> {
        &self.ops
    }

    /// Returns the page-cache object owned by this address space, if present.
    pub fn page_cache(&self) -> Option<Arc<Mapping>> {
        self.page_cache.lock().clone()
    }

    /// Returns or creates the page-cache object owned by this address space.
    pub fn get_or_insert_page_cache(self: &Arc<Self>, kind: MappingKind, len: u64) -> Arc<Mapping> {
        let mut guard = self.page_cache.lock();
        if let Some(mapping) = guard.as_ref() {
            debug_assert_eq!(mapping.kind(), kind);
            return mapping.clone();
        }
        let ops: Arc<dyn MappingOps> = Arc::new(AddressSpacePageCacheOps {
            address_space: Arc::downgrade(self),
        });
        let mapping = Mapping::new(kind, len, ops);
        *guard = Some(mapping.clone());
        mapping
    }

    /// Reads backing storage into a newly materialized folio.
    pub fn read_folio(&self, folio: &mut Folio, index: PageIndex) -> VfsResult<usize> {
        self.ops.read_folio(folio, index)
    }

    /// Marks a folio dirty through this address-space operation set.
    pub fn dirty_folio(&self, folio: &mut Folio) -> VfsResult<bool> {
        self.ops.dirty_folio(self, folio)
    }

    /// Writes bytes into the page cache through the address-space operation set.
    pub fn write_from(&self, offset: u64, src: &[u8]) -> VfsResult<usize> {
        self.ops
            .write_begin(self, WriteBeginRequest::new(offset, src.len()))?;
        let mapping = self.page_cache().ok_or(VfsError::InvalidInput)?;
        let copied = mapping.write_from_with_dirty(offset, src, |folio| {
            self.dirty_folio(folio)?;
            Ok(())
        })?;
        self.ops
            .write_end(self, WriteEndRequest::new(offset, src.len(), copied))
    }

    /// Writes dirty pages belonging to this address space.
    pub fn writepages(&self, data_only: bool) -> VfsResult<()> {
        self.ops.writepages(self, WritebackControl::all(data_only))
    }

    /// Writes dirty pages intersecting `[start, start + len)`.
    pub fn writepages_range(&self, start: u64, len: usize, data_only: bool) -> VfsResult<()> {
        self.ops
            .writepages(self, WritebackControl::range(start, len, data_only)?)
    }

    /// Writes dirty pages from `start` through EOF.
    pub fn writepages_from(&self, start: u64, data_only: bool) -> VfsResult<()> {
        self.ops
            .writepages(self, WritebackControl::from(start, data_only))
    }

    /// Prepares this address space for inode teardown.
    ///
    /// Writes back dirty pages before dropping the page cache.  For
    /// file-backed files this persists data to disk; for in-memory files
    /// (memfs/tmpfs) the current `writepages` is a no-op — data lives in
    /// the page cache and eviction here is only reached on file deletion
    /// (unlink), where discarding is correct.
    ///
    /// When swap or background memory reclaim is added, `writepages` for
    /// in-memory files should either write dirty folios to swap or mark
    /// them unevictable (noswap).
    pub fn evict(&self) -> VfsResult<()> {
        if let Some(mapping) = self.page_cache() {
            self.writepages(false)?;
            mapping.invalidate_from_page(0)?;
        }
        Ok(())
    }

    /// Invalidates cached pages starting at `page_index`.
    pub fn invalidate_from(&self, page_index: u64) -> VfsResult<()> {
        if let Some(mapping) = self.page_cache() {
            mapping.invalidate_from_page(page_index)?;
        }
        Ok(())
    }

    /// Access address-space-private attachment storage.
    pub fn data(&self) -> MutexGuard<'_, TypeMap> {
        self.data.lock()
    }

    fn writeback_cached_folios(
        &self,
        control: WritebackControl,
        mut write_folio_fn: impl FnMut(PageIndex, &[u8], usize) -> VfsResult<()>,
    ) -> VfsResult<()> {
        let Some(mapping) = self.page_cache() else {
            return Ok(());
        };
        mapping.writeback_until(
            control.range_start(),
            control.range_end(),
            &mut write_folio_fn,
        )
    }
}

impl core::fmt::Debug for AddressSpace {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AddressSpace")
            .field("inode", &self.inode().map(|inode| inode.inode()))
            .field(
                "page_cache",
                &self.page_cache().map(|mapping| mapping.identity()),
            )
            .finish()
    }
}

struct AddressSpacePageCacheOps {
    address_space: alloc::sync::Weak<AddressSpace>,
}

impl AddressSpacePageCacheOps {
    fn address_space(&self) -> VfsResult<Arc<AddressSpace>> {
        self.address_space.upgrade().ok_or(VfsError::InvalidInput)
    }
}

impl MappingOps for AddressSpacePageCacheOps {
    fn instantiate_folio(&self, index: PageIndex) -> KResult<Folio> {
        let address_space = self.address_space()?;
        let mut folio = Folio::new_zeroed()?;
        address_space.read_folio(&mut folio, index)?;
        Ok(folio)
    }
}

pub(crate) fn default_address_space_operations(
    file: FileNode,
    node_type: NodeType,
) -> Arc<dyn AddressSpaceOperations> {
    if node_type == NodeType::RegularFile && !file.flags().contains(NodeFlags::NON_CACHEABLE) {
        file_address_space_operations(file)
    } else {
        empty_address_space_operations()
    }
}

pub(crate) fn file_address_space_operations(file: FileNode) -> Arc<dyn AddressSpaceOperations> {
    Arc::new(NodeBackedAddressSpaceOperations::new(file))
}

pub(crate) fn empty_address_space_operations() -> Arc<dyn AddressSpaceOperations> {
    Arc::new(EmptyAddressSpaceOperations)
}

struct NodeBackedAddressSpaceOperations {
    file: FileNode,
    in_memory: bool,
}

impl NodeBackedAddressSpaceOperations {
    fn new(file: FileNode) -> Self {
        let in_memory = file.flags().contains(NodeFlags::ALWAYS_CACHE);
        Self { file, in_memory }
    }

    fn page_start(page_index: u64) -> VfsResult<u64> {
        page_index
            .checked_mul(PAGE_SIZE_4K as u64)
            .ok_or(VfsError::InvalidInput)
    }

    fn write_folio_data(&self, page_index: PageIndex, data: &[u8]) -> VfsResult<usize> {
        let page_start = Self::page_start(page_index)?;
        let mut written = 0usize;
        while written < data.len() {
            let n = self
                .file
                .write_at(&data[written..], page_start + written as u64)?;
            if n == 0 {
                return Err(VfsError::WriteZero);
            }
            written += n;
        }
        Ok(written)
    }
}

impl AddressSpaceOperations for NodeBackedAddressSpaceOperations {
    fn read_folio(&self, folio: &mut Folio, page_index: PageIndex) -> VfsResult<usize> {
        let page = folio.data();
        if self.in_memory {
            page.fill(0);
            return Ok(page.len());
        }
        self.file.read_at(page, Self::page_start(page_index)?)
    }

    fn writepages(&self, mapping: &AddressSpace, control: WritebackControl) -> VfsResult<()> {
        if self.in_memory {
            // In-memory files (memfs/tmpfs) have no backing store — their
            // page cache IS the storage.  Eviction is only triggered by
            // file deletion (unlink), at which point discarding the pages
            // is correct behaviour.
            //
            // When swap or background memory reclaim is added, this is the
            // place to either write dirty folios to swap, or mark them as
            // unevictable when swap is unavailable (noswap).
            return Ok(());
        }

        mapping.writeback_cached_folios(control, |index, data, valid_len| {
            if valid_len == 0 {
                return Ok(());
            }
            self.write_folio_data(index, &data[..valid_len])?;
            Ok(())
        })?;

        self.file.sync(control.is_data_only())
    }
}

struct EmptyAddressSpaceOperations;

impl AddressSpaceOperations for EmptyAddressSpaceOperations {
    fn read_folio(&self, _folio: &mut Folio, _index: PageIndex) -> VfsResult<usize> {
        Err(VfsError::InvalidInput)
    }

    fn writepages(&self, _mapping: &AddressSpace, _control: WritebackControl) -> VfsResult<()> {
        Ok(())
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::sync::{Arc, Weak};

    use ksync::Mutex;
    use pagecache::{Folio, MappingKind, PageIndex};
    use unittest::def_test;

    use super::*;

    /// An `AddressSpaceOperations` that records whether `writepages` was
    /// called, and how many dirty folios were written back.
    struct TestOps {
        writepages_called: Mutex<bool>,
        writeback_count: Mutex<usize>,
    }

    impl TestOps {
        fn new_arc() -> Arc<Self> {
            Arc::new(Self {
                writepages_called: Mutex::new(false),
                writeback_count: Mutex::new(0),
            })
        }
    }

    impl AddressSpaceOperations for TestOps {
        fn read_folio(&self, folio: &mut Folio, _index: PageIndex) -> VfsResult<usize> {
            folio.data().fill(0);
            Ok(folio.data().len())
        }

        fn writepages(&self, mapping: &AddressSpace, control: WritebackControl) -> VfsResult<()> {
            *self.writepages_called.lock() = true;
            // Simulate writing dirty folios: count and clear them.
            mapping.writeback_cached_folios(control, |_index, _data, _valid_len| {
                *self.writeback_count.lock() += 1;
                Ok(())
            })
        }
    }

    /// Verifies that `AddressSpace::evict()` calls `writepages()` **before**
    /// `invalidate_from_page()`, so dirty page-cache folios are written back
    /// rather than silently dropped.
    ///
    /// This is a regression test for the bug where inode eviction dropped
    /// dirty file data without writing it to the backing store, causing
    /// APK cache corruption (APKINDEX.tar.gz "file format is invalid").
    #[def_test]
    fn evict_writes_back_dirty_folios_before_invalidation() {
        let ops = TestOps::new_arc();
        let address_space = Arc::new(AddressSpace::new(
            Weak::new(),
            ops.clone() as Arc<dyn AddressSpaceOperations>,
        ));

        // Create the page cache and write dirty data.
        let _mapping = address_space.get_or_insert_page_cache(MappingKind::InMemory, 4096);
        address_space
            .write_from(0, b"hello world")
            .expect("write_from should succeed");

        // Evict — this is the code path that was broken.
        address_space.evict().expect("evict should succeed");

        // writepages MUST have been called.
        assert!(
            *ops.writepages_called.lock(),
            "evict() must call writepages() before invalidating"
        );
        // At least one dirty folio must have been written back.
        assert!(
            *ops.writeback_count.lock() > 0,
            "evict() must write back dirty folios"
        );
    }

    /// Verifies that `evict()` on a *clean* address space is harmless and
    /// does not spuriously write back clean folios.
    #[def_test]
    fn evict_on_clean_address_space_writes_back_zero_folios() {
        let ops = TestOps::new_arc();
        let address_space = Arc::new(AddressSpace::new(
            Weak::new(),
            ops.clone() as Arc<dyn AddressSpaceOperations>,
        ));

        // Create the page cache but do NOT write any data.
        let _mapping = address_space.get_or_insert_page_cache(MappingKind::InMemory, 0);

        // Evict on a clean cache should succeed.
        address_space.evict().expect("evict should succeed");

        // writepages may be called but should find no dirty folios.
        assert_eq!(
            *ops.writeback_count.lock(),
            0,
            "evict on clean cache should not write back anything"
        );
    }
}

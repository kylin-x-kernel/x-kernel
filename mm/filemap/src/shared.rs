// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec, vec::Vec};
use core::ops::Range;

use kerrno::{KError, KResult};
use khal::paging::{MappingFlags, PageSize, PageTableMut, PagingError};
use ksync::Mutex;
use kvfs::{FMode, VfsFile};
use memaddr::{MemoryAddr, PAGE_SIZE_4K, VirtAddr, VirtAddrRange};
use memfs::shmem;
use memspace::{
    FaultContext, ForkCloneTarget, InvalidateHandle, MmSpace, VmBackingInfo, VmBackingKind,
    VmRuntimeOps, VmRuntimeRef,
    backend::{FaultCompletion, FaultCompletionResult, map_paging_err, pages_in},
};

use crate::runtime::{SharedFileSourceAdapter, SharedFileSourceSpec};

pub(crate) struct FileSharedRuntimeInner {
    source: SharedFileSourceAdapter,
    shared_ranges: Mutex<Vec<VirtAddrRange>>,
    writable_shared_ranges: Mutex<Vec<VirtAddrRange>>,
}

impl FileSharedRuntimeInner {
    fn object_id(&self) -> vmobj::VmObjectId {
        self.source.object()
    }

    fn writable_pages(range: VirtAddrRange) -> usize {
        range.size() / PAGE_SIZE_4K
    }

    fn subtract_range(range: VirtAddrRange, covered: VirtAddrRange) -> Vec<VirtAddrRange> {
        let overlap_start = range.start.max(covered.start);
        let overlap_end = range.end.min(covered.end);
        if overlap_start >= overlap_end {
            return vec![range];
        }

        let mut remaining = Vec::new();
        if range.start < overlap_start {
            remaining.push(VirtAddrRange::new(range.start, overlap_start));
        }
        if overlap_end < range.end {
            remaining.push(VirtAddrRange::new(overlap_end, range.end));
        }
        remaining
    }

    fn missing_writable_ranges(
        range: VirtAddrRange,
        existing: &[VirtAddrRange],
    ) -> Vec<VirtAddrRange> {
        let mut missing = vec![range];
        for covered in existing {
            let mut next = Vec::new();
            for candidate in missing {
                next.extend(Self::subtract_range(candidate, *covered));
            }
            missing = next;
            if missing.is_empty() {
                break;
            }
        }
        missing
    }

    fn register_writable_shared_range(&self, range: VirtAddrRange, is_new: bool) -> KResult {
        let mut ranges = self.writable_shared_ranges.lock();
        let missing = Self::missing_writable_ranges(range, &ranges);
        let pages = missing
            .iter()
            .map(|range| Self::writable_pages(*range))
            .sum();
        let file = self.source.file();
        file.verify_mode(FMode::WRITE)?;
        if is_new {
            shmem::register_writable_shared_pages(file.path(), pages)?;
        } else {
            shmem::duplicate_writable_shared_pages(file.path(), pages)?;
        }
        ranges.extend(missing);
        Ok(())
    }

    fn register_shared_range(&self, range: VirtAddrRange) -> KResult {
        let mut ranges = self.shared_ranges.lock();
        let missing = Self::missing_writable_ranges(range, &ranges);
        let pages = missing
            .iter()
            .map(|range| Self::writable_pages(*range))
            .sum();
        shmem::register_shared_pages(self.source.file().path(), pages)?;
        ranges.extend(missing);
        Ok(())
    }

    fn unregister_shared_range(&self, range: VirtAddrRange) {
        let mut ranges = self.shared_ranges.lock();
        let mut remaining = Vec::new();
        let mut removed_pages = 0;
        for registered in ranges.drain(..) {
            let overlap_start = registered.start.max(range.start);
            let overlap_end = registered.end.min(range.end);
            if overlap_start >= overlap_end {
                remaining.push(registered);
                continue;
            }

            removed_pages += Self::writable_pages(VirtAddrRange::new(overlap_start, overlap_end));
            if registered.start < overlap_start {
                remaining.push(VirtAddrRange::new(registered.start, overlap_start));
            }
            if overlap_end < registered.end {
                remaining.push(VirtAddrRange::new(overlap_end, registered.end));
            }
        }
        *ranges = remaining;
        drop(ranges);
        shmem::unregister_shared_pages(self.source.file().path(), removed_pages);
    }

    fn unregister_writable_shared_range(&self, range: VirtAddrRange) {
        let mut ranges = self.writable_shared_ranges.lock();
        let mut remaining = Vec::new();
        let mut removed_pages = 0;
        for registered in ranges.drain(..) {
            let overlap_start = registered.start.max(range.start);
            let overlap_end = registered.end.min(range.end);
            if overlap_start >= overlap_end {
                remaining.push(registered);
                continue;
            }

            removed_pages += Self::writable_pages(VirtAddrRange::new(overlap_start, overlap_end));
            if registered.start < overlap_start {
                remaining.push(VirtAddrRange::new(registered.start, overlap_start));
            }
            if overlap_end < registered.end {
                remaining.push(VirtAddrRange::new(overlap_end, registered.end));
            }
        }
        *ranges = remaining;
        drop(ranges);
        shmem::unregister_writable_shared_pages(self.source.file().path(), removed_pages);
    }
}

#[derive(Clone)]
pub(crate) struct FileSharedRuntime(pub(crate) Arc<FileSharedRuntimeInner>);

impl FileSharedRuntime {
    pub(crate) fn object_id(&self) -> vmobj::VmObjectId {
        self.0.object_id()
    }

    fn check_flags(&self, flags: MappingFlags) -> KResult {
        let mut required_flags = FMode::empty();
        if flags.contains(MappingFlags::READ) {
            required_flags |= FMode::READ;
        }
        if flags.contains(MappingFlags::WRITE) {
            required_flags |= FMode::WRITE;
        }

        if !self.0.source.file().mode().contains(required_flags) {
            return Err(KError::PermissionDenied);
        }
        Ok(())
    }

    fn check_shared_writable_mapping_allowed(&self, flags: MappingFlags) -> KResult {
        if flags.contains(MappingFlags::WRITE) {
            shmem::check_shared_writable_mapping_allowed(self.0.source.file().path())?;
        }
        Ok(())
    }

    fn check_shared_write_fault_allowed(&self) -> KResult {
        shmem::check_shared_write_fault_allowed(self.0.source.file().path())?;
        Ok(())
    }

    fn file_len(&self) -> KResult<u64> {
        Ok(self.0.source.file().size())
    }

    fn cloned_runtime(
        &self,
        new_start: VirtAddr,
        new_len: usize,
        offset_page: u64,
        new_mm_id: u64,
        new_aspace: &Arc<Mutex<MmSpace>>,
        invalidate: Option<InvalidateHandle>,
    ) -> Arc<Self> {
        let invalidate =
            invalidate.unwrap_or_else(|| new_aspace.lock().invalidate_handle(new_aspace));
        let inner = Arc::new(FileSharedRuntimeInner {
            source: SharedFileSourceAdapter::new(
                self.0.source.file_arc(),
                SharedFileSourceSpec {
                    mm_id: new_mm_id,
                    start: new_start,
                    len: new_len,
                    offset_page,
                    invalidate: invalidate.clone(),
                },
            ),
            shared_ranges: Mutex::new(Vec::new()),
            writable_shared_ranges: Mutex::new(Vec::new()),
        });
        Arc::new(Self(inner))
    }
}

impl FileSharedRuntime {
    fn page_size(&self) -> PageSize {
        PageSize::Size4K
    }

    fn backing_info_impl(&self) -> VmBackingInfo {
        VmBackingInfo::new(
            VmBackingKind::FileShared {
                object: self.object_id(),
            },
            self.page_size(),
        )
    }

    fn map_impl(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        _pt: &mut PageTableMut,
    ) -> KResult {
        self.check_flags(flags)?;
        self.check_shared_writable_mapping_allowed(flags)?;
        self.0.register_shared_range(range)?;
        if flags.contains(MappingFlags::WRITE)
            && let Err(err) = self.0.register_writable_shared_range(range, true)
        {
            self.0.unregister_shared_range(range);
            return Err(err);
        }
        Ok(())
    }

    fn unmap_impl(&self, range: VirtAddrRange, pt: &mut PageTableMut) -> KResult {
        for addr in pages_in(range, PageSize::Size4K)? {
            match pt.unmap(addr) {
                Ok(_) | Err(PagingError::NotMapped) => {}
                Err(err) => {
                    warn!("Failed to unmap page {:?}: {:?}", addr, err);
                    return Err(map_paging_err(err));
                }
            }
        }
        self.0.unregister_shared_range(range);
        self.0.unregister_writable_shared_range(range);
        Ok(())
    }

    fn on_protect_impl(
        &self,
        range: VirtAddrRange,
        new_flags: MappingFlags,
        _pgtbl: &mut PageTableMut,
    ) -> KResult<MappingFlags> {
        self.check_flags(new_flags)?;
        self.check_shared_writable_mapping_allowed(new_flags)?;
        if new_flags.contains(MappingFlags::WRITE) {
            self.0.register_writable_shared_range(range, true)?;
        } else {
            self.0.unregister_writable_shared_range(range);
        }
        Ok(new_flags - MappingFlags::WRITE)
    }

    fn handle_fault_impl(
        &self,
        ctx: FaultContext,
        flags: MappingFlags,
        pgtbl: &mut PageTableMut,
    ) -> FaultCompletionResult {
        let addr = ctx.address().align_down(self.page_size());
        let page_file_offset = ctx.page_file_offset(addr).ok_or(KError::BadAddress)?;
        // A fault only fails once the faulting page itself starts at or past
        // EOF. Bytes beyond EOF inside the last mapped file page stay
        // zero-filled and faultable.
        if page_file_offset >= self.file_len()? {
            return Err(KError::BadAddress);
        }
        let pn = ctx.page_index().ok_or(KError::BadAddress)?;
        let mut populated = 0;
        let address_space = self.0.source.address_space();

        match pgtbl.query(addr) {
            Ok((paddr, page_flags, _)) => {
                if ctx.access_flags().contains(MappingFlags::WRITE)
                    && !page_flags.contains(MappingFlags::WRITE)
                {
                    self.check_shared_write_fault_allowed()?;
                    self.0.source.file().page_mkwrite(pn, |folio| {
                        if folio.paddr() != paddr {
                            return Err(KError::BadAddress);
                        }
                        pgtbl
                            .remap_raw(addr, paddr, flags)
                            .map_err(map_paging_err)?;
                        populated = 1;
                        KResult::Ok(())
                    })?;
                } else if page_flags.contains(ctx.access_flags()) {
                    populated = 1;
                }
            }
            Err(PagingError::NotMapped) if ctx.access_flags().contains(MappingFlags::WRITE) => {
                self.check_shared_write_fault_allowed()?;
                self.0.source.file().page_mkwrite(pn, |folio| {
                    let reference = folio.try_clone_mapping_ref()?;
                    pgtbl.map(addr, reference, flags).map_err(map_paging_err)?;
                    populated = 1;
                    KResult::Ok(())
                })?;
            }
            Err(PagingError::NotMapped) => {
                address_space.with_invalidate_read(|| {
                    address_space.with_folio_or_create(pn, |folio| {
                        let reference = folio.try_clone_mapping_ref()?;
                        let read_flags = flags - MappingFlags::WRITE;
                        let has_access =
                            read_flags.intersects(MappingFlags::READ | MappingFlags::EXECUTE);
                        pgtbl
                            .map(
                                addr,
                                reference,
                                if has_access {
                                    read_flags
                                } else {
                                    MappingFlags::empty()
                                },
                            )
                            .map_err(map_paging_err)?;
                        populated = usize::from(has_access);
                        Ok(())
                    })
                })?;
            }
            Err(_) => return Err(KError::BadAddress),
        }

        Ok(FaultCompletion::from_populate((populated, None)))
    }
}

impl VmRuntimeOps for FileSharedRuntime {
    fn backing_info(&self) -> VmBackingInfo {
        self.backing_info_impl()
    }

    fn map(&self, range: VirtAddrRange, flags: MappingFlags, pgtbl: &mut PageTableMut) -> KResult {
        self.map_impl(range, flags, pgtbl)
    }

    fn open(&self, range: VirtAddrRange, flags: MappingFlags) -> KResult {
        self.0.register_shared_range(range)?;
        let result = (|| {
            if flags.contains(MappingFlags::WRITE) {
                self.0.register_writable_shared_range(range, false)?;
            }
            self.0.source.extend_view_to(range.end)
        })();
        if result.is_err() {
            self.0.unregister_shared_range(range);
            self.0.unregister_writable_shared_range(range);
        }
        result
    }

    fn unmap(&self, range: VirtAddrRange, pgtbl: &mut PageTableMut) -> KResult {
        self.unmap_impl(range, pgtbl)
    }

    fn on_mapping_retired(&self, range: VirtAddrRange) {
        self.0.unregister_shared_range(range);
        self.0.unregister_writable_shared_range(range);
    }

    fn on_protect(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        pgtbl: &mut PageTableMut,
    ) -> KResult<MappingFlags> {
        self.on_protect_impl(range, flags, pgtbl)
    }

    fn handle_fault(
        &self,
        ctx: FaultContext,
        flags: MappingFlags,
        pgtbl: &mut PageTableMut,
    ) -> FaultCompletionResult {
        self.handle_fault_impl(ctx, flags, pgtbl)
    }

    fn wait_for_fault(&self) -> KResult<()> {
        self.0.source.file().wait_for_shared_mapping_write();
        Ok(())
    }

    fn msync(&self, file_range: Range<u64>) -> KResult {
        self.0.source.file().fsync_range(file_range, true)?;
        Ok(())
    }

    fn relocate_for_mremap(
        &self,
        source_range: VirtAddrRange,
        new_start: VirtAddr,
        new_len: usize,
        new_mm_id: u64,
        aspace: &Arc<Mutex<MmSpace>>,
        invalidate: Option<InvalidateHandle>,
    ) -> KResult<Arc<dyn VmRuntimeOps>> {
        let offset_page = self.0.source.offset_page()
            + ((source_range.start - self.0.source.start()) / PAGE_SIZE_4K) as u64;
        Ok(self.cloned_runtime(
            new_start,
            new_len,
            offset_page,
            new_mm_id,
            aspace,
            invalidate,
        ))
    }

    fn clone_for_fork(
        &self,
        range: VirtAddrRange,
        _flags: MappingFlags,
        _old_pgtbl: &mut PageTableMut,
        _new_pgtbl: &mut PageTableMut,
        target: ForkCloneTarget<'_>,
    ) -> KResult<Arc<dyn VmRuntimeOps>> {
        Ok(self.cloned_runtime(
            range.start,
            range.size(),
            self.0.source.offset_page()
                + ((range.start - self.0.source.start()) / PAGE_SIZE_4K) as u64,
            target.new_mm_id,
            target.new_aspace,
            target.invalidate,
        ))
    }
}

pub(crate) struct FileSharedRuntimeSpec {
    pub start: VirtAddr,
    pub len: usize,
    pub file: Arc<VfsFile>,
    pub offset: usize,
    pub mm_id: u64,
    pub invalidate: InvalidateHandle,
}

pub(crate) fn new_shared_runtime(spec: FileSharedRuntimeSpec) -> VmRuntimeRef {
    let offset_page = (spec.offset / PAGE_SIZE_4K) as u64;
    let inner = Arc::new(FileSharedRuntimeInner {
        source: SharedFileSourceAdapter::new(
            spec.file,
            SharedFileSourceSpec {
                mm_id: spec.mm_id,
                start: spec.start,
                len: spec.len,
                offset_page,
                invalidate: spec.invalidate.clone(),
            },
        ),
        shared_ranges: Mutex::new(Vec::new()),
        writable_shared_ranges: Mutex::new(Vec::new()),
    });
    VmRuntimeRef::new_file_shared(Arc::new(FileSharedRuntime(inner)))
}

unittest::path_mod_test!(tests, "../ut-tests/shared.rs");

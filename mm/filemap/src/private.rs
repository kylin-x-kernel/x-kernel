// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::slice;

use anon::{AnonPrivateObject, AnonPrivateViewGuard};
use kerrno::{KError, KResult};
use khal::{
    mem::p2v,
    paging::{MappingFlags, PageSize, PageTableMut, PagingError},
};
use ksync::Mutex;
use kvfs::{AddressSpaceViewGuard, VfsFile};
use memaddr::{MemoryAddr, PAGE_SIZE_4K, PhysAddr, VirtAddr, VirtAddrRange};
use memspace::{
    FaultContext, ForkCloneTarget, InvalidateHandle, MmObserver, MmSpace, VmArea, VmBackingInfo,
    VmBackingKind, VmRuntimeOps, VmRuntimeRef,
    backend::{
        FaultCompletion, FaultCompletionResult, alloc_frame, dealloc_frame,
        private::{
            PrivateForkMaterialize, clone_private_object_pages_for_fork, cow_private_object_page,
            discard_private_object_range, map_existing_private_object_page,
            private_anon_invalidate_notifier, unmap_private_object_range,
        },
    },
};
use vmobj::{MappingViewId, MappingViewKind, MappingViewSpec};

use crate::runtime::FileRuntimeContext;

#[derive(Clone)]
pub(crate) struct FilePrivateRuntime {
    start: VirtAddr,
    len: usize,
    size: PageSize,
    anon: Arc<AnonPrivateObject>,
    file: Arc<VfsFile>,
    file_start: u64,
    file_end: Option<u64>,
    mapping_view: Option<AddressSpaceViewGuard>,
    anon_view: Option<AnonPrivateViewGuard>,
}

impl FilePrivateRuntime {
    fn registration_id(&self) -> Option<MappingViewId> {
        self.mapping_view.as_ref().map(|it| it.id())
    }

    fn anon_registration_id(&self) -> Option<MappingViewId> {
        self.anon_view.as_ref().map(AnonPrivateViewGuard::id)
    }

    fn file_len(&self) -> u64 {
        self.file_end.unwrap_or_else(|| self.file.size())
    }

    fn alloc_new_frame(&self, zeroed: bool) -> KResult<PhysAddr> {
        alloc_frame(zeroed, self.size)
    }

    fn object_offset_for(&self, addr: VirtAddr) -> KResult<u64> {
        addr.as_usize()
            .checked_sub(self.start.as_usize())
            .map(|it| it as u64)
            .ok_or(KError::BadAddress)
    }

    fn alloc_new_at(
        &self,
        va: VirtAddr,
        file_page_offset: Option<u64>,
        page_data_offset: Option<usize>,
        flags: MappingFlags,
        pgtbl: &mut PageTableMut,
    ) -> KResult {
        let object_start = self.object_offset_for(va)?;
        let prepared = self.anon.prepare_first_touch_page(object_start)?;
        let frame = self.alloc_new_frame(true)?;

        // SAFETY: `frame` was freshly allocated for this page and has at
        // least `self.size` bytes of kernel-mapped storage.
        let buf = unsafe { slice::from_raw_parts_mut(p2v(frame).as_mut_ptr(), self.size as _) };
        let start =
            page_data_offset.unwrap_or_else(|| self.start.as_usize().saturating_sub(va.as_usize()));
        assert!(start < self.size as _);

        let file_start = file_page_offset.unwrap_or_else(|| {
            self.file_start + va.as_usize().saturating_sub(self.start.as_usize()) as u64
        });
        let max_read = self
            .file_end
            .map_or(u64::MAX, |end| end.saturating_sub(file_start))
            .min((buf.len() - start) as u64) as usize;
        let address_space = self.file.mapping();

        let mut copied = 0usize;
        while copied < max_read {
            let object_offset = file_start + copied as u64;
            let index = object_offset / PAGE_SIZE_4K as u64;
            let page_off = (object_offset % PAGE_SIZE_4K as u64) as usize;
            let step = (max_read - copied).min(PAGE_SIZE_4K - page_off);
            if let Err(err) = address_space.with_folio_or_create(index, |folio| {
                buf[start + copied..start + copied + step]
                    .copy_from_slice(&folio.data()[page_off..page_off + step]);
                Ok(())
            }) {
                dealloc_frame(frame, self.size);
                return Err(err);
            }
            copied += step;
        }
        if let Err(err) = pgtbl.map(va, frame, self.size, flags) {
            dealloc_frame(frame, self.size);
            return Err(memspace::backend::map_paging_err(err));
        }
        if let Err(err) = prepared.commit(frame, self.size) {
            if let Ok((_frame, _flags, page_size)) = pgtbl.unmap(va) {
                assert_eq!(page_size, self.size);
                pgtbl.finish();
            }
            dealloc_frame(frame, self.size);
            return Err(err);
        }

        if flags.contains(MappingFlags::EXECUTE) {
            karch::flush_icache_range(p2v(frame), self.size.into());
        }
        Ok(())
    }

    fn prepare_new_file_backed_page(
        &self,
        ctx: &FaultContext,
        addr: VirtAddr,
    ) -> KResult<(Option<u64>, Option<usize>)> {
        let file_page_offset = ctx.page_file_offset(addr).ok_or(KError::BadAddress)?;
        let page_data_offset = ctx.page_data_offset().ok_or(KError::BadAddress)?;

        // Loader-created PT_LOAD mappings use `file_end: Some(...)` to
        // describe a file-backed prefix followed by a valid anonymous
        // zero-fill tail (`memsz > filesz`). Those tail pages must remain
        // faultable instead of being rejected as EOF.
        if self.file_end.is_none() && file_page_offset >= self.file_len() {
            return Err(KError::BadAddress);
        }

        Ok((Some(file_page_offset), Some(page_data_offset)))
    }

    fn handle_populated_fault(
        &self,
        addr: VirtAddr,
        flags: MappingFlags,
        page_flags: MappingFlags,
        page_size: PageSize,
        pgtbl: &mut PageTableMut,
        ctx: &FaultContext,
    ) -> FaultCompletionResult {
        assert_eq!(self.size, page_size);
        if ctx.access_flags().contains(MappingFlags::WRITE)
            && !page_flags.contains(MappingFlags::WRITE)
        {
            let object_start = self.object_offset_for(addr)?;
            if cow_private_object_page(&self.anon, object_start, addr, flags, self.size, pgtbl)?
                .is_retry()
            {
                return Ok(FaultCompletion::cow_conflict_retry());
            }
            return Ok(FaultCompletion::from_populate((1, None)));
        }
        if page_flags.contains(ctx.access_flags()) {
            return Ok(FaultCompletion::from_populate((1, None)));
        }
        Ok(FaultCompletion::from_populate((0, None)))
    }

    fn handle_unmapped_fault(
        &self,
        ctx: &FaultContext,
        flags: MappingFlags,
        pgtbl: &mut PageTableMut,
        addr: VirtAddr,
        object_start: u64,
    ) -> FaultCompletionResult {
        // Revalidate EOF before consulting retained private state. Truncate's
        // second unmap removes a racing COW PTE, but the Rust anon object may
        // still retain that page until a later reclaim pass.
        let file_source = self.prepare_new_file_backed_page(ctx, addr)?;
        let existing = map_existing_private_object_page(
            &self.anon,
            object_start,
            addr,
            ctx.access_flags(),
            flags,
            self.size,
            pgtbl,
        )?;
        if existing.is_retry() {
            return Ok(FaultCompletion::cow_conflict_retry());
        }
        if !existing.is_resolved() {
            let (file_page_offset, page_data_offset) = file_source;
            self.alloc_new_at(addr, file_page_offset, page_data_offset, flags, pgtbl)?;
        }
        Ok(FaultCompletion::from_populate((1, None)))
    }

    fn register_mapping_view(
        file: &Arc<VfsFile>,
        file_start: u64,
        file_end: Option<u64>,
        context: Option<&FileRuntimeContext<'_>>,
        start: VirtAddr,
        len: usize,
    ) -> Option<AddressSpaceViewGuard> {
        let context = context?;
        let address_space = file.mapping();
        let object_start = file_start / PageSize::Size4K as u64 * PageSize::Size4K as u64;
        let len = match file_end {
            Some(end) => end.saturating_sub(object_start).min(len as u64) as usize,
            None => len,
        };
        if len == 0 {
            return None;
        }
        Some(address_space.register_view(MappingViewSpec {
            mm_id: context.mm_id,
            vma_start: start.as_usize() as u64,
            vma_len: len,
            object_start,
            object_len: len,
            kind: MappingViewKind::Private,
            notifier: Some(crate::invalidate::MmSpaceInvalidate::new(
                context.invalidate.clone(),
            )),
        }))
    }

    fn register_anon_view(
        anon: &Arc<AnonPrivateObject>,
        mm_id: Option<u64>,
        invalidate: Option<InvalidateHandle>,
        observer: Option<&MmObserver>,
        start: VirtAddr,
        len: usize,
    ) -> Option<AnonPrivateViewGuard> {
        let observer = observer?;
        Some(anon.register_view(MappingViewSpec {
            mm_id: mm_id?,
            vma_start: start.as_usize() as u64,
            vma_len: len,
            object_start: 0,
            object_len: len,
            kind: MappingViewKind::Private,
            notifier: Some(private_anon_invalidate_notifier(
                invalidate.unwrap_or_else(|| observer.invalidate_handle()),
            )),
        }))
    }

    fn rebuilt_runtime(
        &self,
        new_start: VirtAddr,
        new_mm_id: Option<u64>,
        new_observer: Option<&MmObserver>,
        invalidate: Option<InvalidateHandle>,
        fork_child: bool,
    ) -> Arc<Self> {
        let invalidate = invalidate.or_else(|| new_observer.map(MmObserver::invalidate_handle));
        let anon = if fork_child {
            self.anon.fork_child()
        } else {
            self.anon.clone()
        };
        let file_context =
            new_mm_id
                .zip(new_observer)
                .map(|(mm_id, observer)| FileRuntimeContext {
                    mm_id,
                    observer,
                    invalidate: invalidate
                        .clone()
                        .unwrap_or_else(|| observer.invalidate_handle()),
                });
        Arc::new(Self {
            start: new_start,
            len: self.len,
            size: self.size,
            anon: anon.clone(),
            file: self.file.clone(),
            file_start: self.file_start,
            file_end: self.file_end,
            mapping_view: Self::register_mapping_view(
                &self.file,
                self.file_start,
                self.file_end,
                file_context.as_ref(),
                new_start,
                self.len,
            ),
            anon_view: Self::register_anon_view(
                &anon,
                new_mm_id,
                invalidate,
                new_observer,
                new_start,
                self.len,
            ),
        })
    }
}

impl FilePrivateRuntime {
    fn page_size(&self) -> PageSize {
        self.size
    }

    fn backing_info_impl(&self) -> VmBackingInfo {
        VmBackingInfo::new(
            VmBackingKind::FilePrivate {
                file_object: self.file.mapping().object_id(),
                anon_object: self.anon.id(),
                anon_lineage: self.anon.lineage(),
            },
            self.page_size(),
        )
    }

    fn map_impl(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        _pgtbl: &mut PageTableMut,
    ) -> KResult {
        debug!("FilePrivateRuntime::map: {range:?} {flags:?}");
        Ok(())
    }

    fn unmap_impl(&self, range: VirtAddrRange, pgtbl: &mut PageTableMut) -> KResult {
        debug!("FilePrivateRuntime::unmap: {range:?}");
        let object_start = self.object_offset_for(range.start)?;
        unmap_private_object_range(&self.anon, object_start, range, self.size, pgtbl)
    }

    fn on_protect_impl(
        &self,
        _range: VirtAddrRange,
        new_flags: MappingFlags,
        _pgtbl: &mut PageTableMut,
    ) -> KResult<MappingFlags> {
        Ok(new_flags)
    }

    fn handle_fault_impl(
        &self,
        ctx: FaultContext,
        flags: MappingFlags,
        pgtbl: &mut PageTableMut,
    ) -> FaultCompletionResult {
        let _ = self.registration_id();
        let _ = self.anon_registration_id();
        let addr = ctx.address().align_down(self.page_size());
        match pgtbl.query(addr) {
            Ok((_paddr, page_flags, page_size)) => {
                self.handle_populated_fault(addr, flags, page_flags, page_size, pgtbl, &ctx)
            }
            Err(PagingError::NotMapped) => {
                self.handle_unmapped_fault(&ctx, flags, pgtbl, addr, self.object_offset_for(addr)?)
            }
            Err(_) => Err(KError::BadAddress),
        }
    }
}

impl VmRuntimeOps for FilePrivateRuntime {
    fn backing_info(&self) -> VmBackingInfo {
        self.backing_info_impl()
    }

    fn map(&self, range: VirtAddrRange, flags: MappingFlags, pgtbl: &mut PageTableMut) -> KResult {
        self.map_impl(range, flags, pgtbl)
    }

    fn unmap(&self, range: VirtAddrRange, pgtbl: &mut PageTableMut) -> KResult {
        self.unmap_impl(range, pgtbl)
    }

    fn on_protect(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        pgtbl: &mut PageTableMut,
    ) -> KResult<MappingFlags> {
        self.on_protect_impl(range, flags, pgtbl)
    }

    fn madvise_dontneed(
        &self,
        vma: &VmArea,
        range: VirtAddrRange,
        pgtbl: &mut PageTableMut,
    ) -> KResult<bool> {
        let _ = vma;
        let object_start = self.object_offset_for(range.start)?;
        discard_private_object_range(&self.anon, object_start, range, self.size, pgtbl)?;
        Ok(true)
    }

    fn handle_fault(
        &self,
        ctx: FaultContext,
        flags: MappingFlags,
        pgtbl: &mut PageTableMut,
    ) -> FaultCompletionResult {
        self.handle_fault_impl(ctx, flags, pgtbl)
    }

    fn relocate_for_mremap(
        &self,
        new_start: VirtAddr,
        new_mm_id: u64,
        aspace: &Arc<Mutex<MmSpace>>,
        invalidate: Option<InvalidateHandle>,
    ) -> KResult<Arc<dyn VmRuntimeOps>> {
        let observer = MmObserver::new(aspace);
        Ok(self.rebuilt_runtime(
            new_start,
            Some(new_mm_id),
            Some(&observer),
            invalidate,
            false,
        ))
    }

    fn clone_for_fork(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        old_pgtbl: &mut PageTableMut,
        new_pgtbl: &mut PageTableMut,
        target: ForkCloneTarget<'_>,
    ) -> KResult<Arc<dyn VmRuntimeOps>> {
        let cow_flags = flags - MappingFlags::WRITE;
        let child = self.anon.fork_child();
        let object_start = self.object_offset_for(range.start)?;
        clone_private_object_pages_for_fork(
            &self.anon,
            &child,
            object_start,
            range.size(),
            PrivateForkMaterialize {
                runtime_start: self.start,
                cow_flags,
                page_size: self.size,
            },
            old_pgtbl,
            new_pgtbl,
        )?;

        let invalidate = target.invalidate;
        let observer = MmObserver::new(target.new_aspace);
        let file_context = FileRuntimeContext {
            mm_id: target.new_mm_id,
            observer: &observer,
            invalidate: invalidate
                .clone()
                .unwrap_or_else(|| observer.invalidate_handle()),
        };
        Ok(Arc::new(Self {
            start: self.start,
            len: self.len,
            size: self.size,
            anon: child.clone(),
            file: self.file.clone(),
            file_start: self.file_start,
            file_end: self.file_end,
            mapping_view: Self::register_mapping_view(
                &self.file,
                self.file_start,
                self.file_end,
                Some(&file_context),
                self.start,
                self.len,
            ),
            anon_view: Self::register_anon_view(
                &child,
                Some(target.new_mm_id),
                invalidate,
                Some(&observer),
                self.start,
                self.len,
            ),
        }))
    }
}

pub(crate) fn new_private_runtime(
    start: VirtAddr,
    len: usize,
    size: PageSize,
    file: Arc<VfsFile>,
    file_start: u64,
    file_end: Option<u64>,
    context: Option<FileRuntimeContext<'_>>,
) -> KResult<VmRuntimeRef> {
    let anon = AnonPrivateObject::new_root();
    let mm_id = context.as_ref().map(|context| context.mm_id);
    let observer = context.as_ref().map(|context| context.observer);
    let invalidate = context.as_ref().map(|context| context.invalidate.clone());
    Ok(VmRuntimeRef::new_file_private(Arc::new(
        FilePrivateRuntime {
            start,
            len,
            size,
            anon: anon.clone(),
            mapping_view: FilePrivateRuntime::register_mapping_view(
                &file,
                file_start,
                file_end,
                context.as_ref(),
                start,
                len,
            ),
            anon_view: FilePrivateRuntime::register_anon_view(
                &anon, mm_id, invalidate, observer, start, len,
            ),
            file,
            file_start,
            file_end,
        },
    )))
}

#[cfg(unittest)]
mod tests {
    use alloc::vec;
    use core::slice;

    use anon::AnonPrivateObject;
    use khal::{
        mem::p2v,
        paging::{MappingFlags, PageSize, PageTable},
        trap::PageFaultFlags,
    };
    use memaddr::{PAGE_SIZE_4K, VirtAddr, VirtAddrRange};
    use memspace::{
        FaultContext, VmArea, VmRuntimeOps,
        backend::private::{PrivateForkMaterialize, clone_private_object_pages_for_fork},
    };
    use unittest::def_test;

    use super::FilePrivateRuntime;
    use crate::test_support::page_cache_file;

    fn assert_frame_prefix(pa: memaddr::PhysAddr, value: u8, len: usize) {
        assert!(len <= PAGE_SIZE_4K);
        // SAFETY: Unit tests only read allocated frames that remain owned by an
        // AnonPrivateObject for at least `len` bytes; the assertion above
        // keeps the test prefix within the 4K frame.
        let bytes = unsafe { slice::from_raw_parts(p2v(pa).as_ptr(), len) };
        assert!(
            bytes.iter().all(|byte| *byte == value),
            "frame prefix should contain byte {value:#x}"
        );
    }

    #[def_test]
    fn file_private_fork_child_keeps_lineage_and_splits_anon_object() {
        let file = page_cache_file("cow-lineage");
        let parent = FilePrivateRuntime {
            start: VirtAddr::from_usize(0x4000),
            len: PAGE_SIZE_4K,
            size: PageSize::Size4K,
            anon: AnonPrivateObject::new_root(),
            file,
            file_start: 0,
            file_end: None,
            mapping_view: None,
            anon_view: None,
        };

        let parent_kind = parent.backing_info_impl().kind();
        let child = parent.rebuilt_runtime(VirtAddr::from_usize(0x8000), None, None, None, true);
        let child_kind = child.backing_info_impl().kind();

        let (
            super::VmBackingKind::FilePrivate {
                file_object: parent_file,
                anon_object: parent_anon,
                anon_lineage: parent_lineage,
            },
            super::VmBackingKind::FilePrivate {
                file_object: child_file,
                anon_object: child_anon,
                anon_lineage: child_lineage,
            },
        ) = (parent_kind, child_kind)
        else {
            panic!("expected file-private backing kinds");
        };

        assert_eq!(parent_file, child_file);
        assert_eq!(parent_lineage, child_lineage);
        assert_ne!(parent_anon, child_anon);
    }

    #[def_test]
    fn file_private_fork_madvise_then_write_fault_preserves_file_source_and_splits_anon_pages() {
        let file = page_cache_file("cow-fork-madvise-write");
        let payload = vec![0x41u8; PAGE_SIZE_4K];
        let mut pos = 0;
        let written = file
            .write_from(&payload[..], &mut pos)
            .expect("seed cached file with one page");
        assert_eq!(written, PAGE_SIZE_4K);

        let start = VirtAddr::from_usize(0x5000);
        let range = VirtAddrRange::from_start_size(start, PAGE_SIZE_4K);
        let flags = MappingFlags::READ | MappingFlags::WRITE;
        let parent = FilePrivateRuntime {
            start,
            len: PAGE_SIZE_4K,
            size: PageSize::Size4K,
            anon: AnonPrivateObject::new_root(),
            file,
            file_start: 0,
            file_end: Some(PAGE_SIZE_4K as u64),
            mapping_view: None,
            anon_view: None,
        };
        let mut parent_pt = PageTable::try_new().expect("allocate parent page table");

        {
            let mut pgtbl = parent_pt.modify();
            let fault = FaultContext::new(start, PageFaultFlags::WRITE).with_backing(
                Some(0),
                Some(0),
                Some(0),
            );
            let completion = VmRuntimeOps::handle_fault(&parent, fault, flags, &mut pgtbl)
                .expect("parent first fault");
            assert_eq!(
                completion.populated(),
                1,
                "file-private first fault should materialize one anon-backed page"
            );
        }

        let parent_kind = parent.backing_info_impl().kind();
        let child = parent.rebuilt_runtime(start, None, None, None, true);
        let child_kind = child.backing_info_impl().kind();
        let (
            super::VmBackingKind::FilePrivate {
                file_object: parent_file,
                anon_object: parent_anon,
                anon_lineage: parent_lineage,
            },
            super::VmBackingKind::FilePrivate {
                file_object: child_file,
                anon_object: child_anon,
                anon_lineage: child_lineage,
            },
        ) = (parent_kind, child_kind)
        else {
            panic!("expected file-private backing kinds");
        };
        assert_eq!(parent_file, child_file);
        assert_eq!(parent_lineage, child_lineage);
        assert_ne!(parent_anon, child_anon);

        let mut child_pt = PageTable::try_new().expect("allocate child page table");
        {
            let mut old_pgtbl = parent_pt.modify();
            let mut new_pgtbl = child_pt.modify();
            clone_private_object_pages_for_fork(
                &parent.anon,
                &child.anon,
                0,
                PAGE_SIZE_4K,
                PrivateForkMaterialize {
                    runtime_start: start,
                    cow_flags: flags - MappingFlags::WRITE,
                    page_size: PageSize::Size4K,
                },
                &mut old_pgtbl,
                &mut new_pgtbl,
            )
            .expect("fork file-private anon side");
        }
        let shared_child_page = child
            .anon
            .page_at(0)
            .expect("child should inherit one shared private page");
        let shared_pa = shared_child_page.phys_addr();

        let vma = VmArea::new(
            start,
            PAGE_SIZE_4K,
            flags,
            flags,
            parent.backing_info_impl(),
            0,
            None,
        );
        {
            let mut pgtbl = parent_pt.modify();
            let handled = VmRuntimeOps::madvise_dontneed(&parent, &vma, range, &mut pgtbl)
                .expect("parent MADV_DONTNEED");
            assert!(handled, "file-private runtime should handle MADV_DONTNEED");
        }
        assert!(
            parent.anon.page_at(0).is_none(),
            "parent MADV_DONTNEED should discard its private anon page"
        );

        {
            let mut pgtbl = child_pt.modify();
            let fault = FaultContext::new(start, PageFaultFlags::WRITE);
            let completion = VmRuntimeOps::handle_fault(&*child, fault, flags, &mut pgtbl)
                .expect("child write fault");
            assert_eq!(
                completion.populated(),
                1,
                "child write fault should complete via shared helper"
            );
        }
        let child_page = child
            .anon
            .page_at(0)
            .expect("child should still own a private page after write fault");
        assert!(
            shared_pa != child_page.phys_addr() || child_page.is_exclusive(),
            "child write fault should either split or retain the inherited page as the sole owner"
        );
        assert!(child_page.is_exclusive());

        {
            let mut pgtbl = parent_pt.modify();
            let fault = FaultContext::new(start, PageFaultFlags::READ).with_backing(
                Some(0),
                Some(0),
                Some(0),
            );
            let completion = VmRuntimeOps::handle_fault(&parent, fault, flags, &mut pgtbl)
                .expect("parent refault");
            assert_eq!(
                completion.populated(),
                1,
                "parent refault after MADV_DONTNEED should rematerialize from file source"
            );
        }
        let parent_page = parent
            .anon
            .page_at(0)
            .expect("parent should own a new private page after refault");
        assert_ne!(parent_page.phys_addr(), child_page.phys_addr());
        assert!(parent_page.is_exclusive());
        assert_frame_prefix(parent_page.phys_addr(), 0x41, 64);
        assert_eq!(parent.backing_info_impl().kind(), parent_kind);
        assert_eq!(child.backing_info_impl().kind(), child_kind);

        {
            let mut pgtbl = parent_pt.modify();
            parent
                .unmap_impl(range, &mut pgtbl)
                .expect("cleanup parent mapping");
        }
        {
            let mut pgtbl = child_pt.modify();
            child
                .unmap_impl(range, &mut pgtbl)
                .expect("cleanup child mapping");
        }
    }

    #[def_test]
    fn private_refault_rechecks_i_size_before_reusing_retained_cow_page() {
        let file = page_cache_file("truncate-private-refault");
        let payload = vec![0x5au8; PAGE_SIZE_4K * 2];
        let mut pos = 0;
        file.write_from(&payload, &mut pos)
            .expect("seed two cached pages");

        let start = VirtAddr::from_usize(0x20_000);
        let fault_addr = start + PAGE_SIZE_4K;
        let flags = MappingFlags::READ | MappingFlags::WRITE;
        let runtime = FilePrivateRuntime {
            start,
            len: PAGE_SIZE_4K * 2,
            size: PageSize::Size4K,
            anon: AnonPrivateObject::new_root(),
            file: file.clone(),
            file_start: 0,
            file_end: None,
            mapping_view: None,
            anon_view: None,
        };
        let mut page_table = PageTable::try_new().expect("allocate page table");
        let fault = FaultContext::new(fault_addr, PageFaultFlags::READ).with_backing(
            Some(1),
            Some(PAGE_SIZE_4K as u64),
            Some(0),
        );

        {
            let mut pgtbl = page_table.modify();
            VmRuntimeOps::handle_fault(&runtime, fault, flags, &mut pgtbl)
                .expect("initial private fault");
        }
        file.truncate(0).expect("truncate file to zero");
        {
            let mut pgtbl = page_table.modify();
            let (_, _, page_size) = pgtbl.unmap(fault_addr).expect("remove racing private PTE");
            assert_eq!(page_size, PageSize::Size4K);
            pgtbl.finish();
        }
        assert!(
            runtime.anon.page_at(PAGE_SIZE_4K as u64).is_some(),
            "PTE invalidation alone intentionally leaves retained Rust object state"
        );

        {
            let mut pgtbl = page_table.modify();
            assert!(
                VmRuntimeOps::handle_fault(&runtime, fault, flags, &mut pgtbl).is_err(),
                "refault after truncate must not reuse the retained private page"
            );
            runtime
                .unmap_impl(
                    VirtAddrRange::from_start_size(fault_addr, PAGE_SIZE_4K),
                    &mut pgtbl,
                )
                .expect("release retained private page");
        }
    }
}

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

unittest::path_mod_test!(tests, "../ut-tests/private.rs");

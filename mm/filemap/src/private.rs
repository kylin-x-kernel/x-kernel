// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{collections::BTreeMap, sync::Arc};
use core::{mem::MaybeUninit, slice};

use anon::{AnonPrivateObject, AnonPrivateViewGuard};
use kerrno::{KError, KResult};
use khal::{
    mem::p2v,
    paging::{MappingFlags, PageSize, PageTableMut, PagingError},
};
use ksync::Mutex;
use kvfs::{AddressSpaceViewGuard, MappedFolio, VfsFile};
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
    anon_start: u64,
    file_pages: Arc<Mutex<BTreeMap<u64, MappedFolio>>>,
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

    fn local_offset_for(&self, addr: VirtAddr) -> KResult<u64> {
        addr.as_usize()
            .checked_sub(self.start.as_usize())
            .map(|it| it as u64)
            .ok_or(KError::BadAddress)
    }

    fn anon_offset_for(&self, addr: VirtAddr) -> KResult<u64> {
        self.anon_start
            .checked_add(self.local_offset_for(addr)?)
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
        let object_start = self.anon_offset_for(va)?;
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

    fn release_file_pages(&self, range: VirtAddrRange) {
        let Ok(start) = self.local_offset_for(range.start) else {
            return;
        };
        let end = start.saturating_add(range.size() as u64);
        loop {
            let removed = {
                let mut pages = self.file_pages.lock();
                let Some((&offset, _)) = pages.range(start..end).next() else {
                    break;
                };
                pages.remove(&offset)
            };
            drop(removed);
        }
    }

    fn release_unmapped_file_pages(&self, range: VirtAddrRange, pgtbl: &mut PageTableMut) {
        let Ok(start) = self.local_offset_for(range.start) else {
            return;
        };
        // Failed sparse unmaps can leave cleared PTEs pending TLB invalidation.
        // finish() is infallible and completes those translations before any
        // file-page pin is released. NotMapped offsets below are the pins to
        // release; present PTEs retain their pins, including the failed leaf.
        pgtbl.finish();
        let mut unmapped_offsets = alloc::vec::Vec::new();
        let mut addr = range.start;
        while addr < range.end {
            if matches!(pgtbl.query(addr), Err(PagingError::NotMapped)) {
                unmapped_offsets.push(
                    start.saturating_add(
                        addr.as_usize().saturating_sub(range.start.as_usize()) as u64
                    ),
                );
            }
            addr += self.size as usize;
        }
        for offset in unmapped_offsets {
            let removed = self.file_pages.lock().remove(&offset);
            drop(removed);
        }
    }

    fn try_map_readonly_file_page(
        &self,
        va: VirtAddr,
        file_start: u64,
        data_offset: usize,
        flags: MappingFlags,
        pgtbl: &mut PageTableMut,
    ) -> KResult<bool> {
        let Some(page_end) = file_start.checked_add(PAGE_SIZE_4K as u64) else {
            return Ok(false);
        };
        if self.size != PageSize::Size4K
            || data_offset != 0
            || !file_start.is_multiple_of(PAGE_SIZE_4K as u64)
            || self.file_end.is_some_and(|end| page_end > end)
        {
            return Ok(false);
        }
        let object_start = self.local_offset_for(va)?;
        self.file
            .mapping()
            .with_pinned_file_page(file_start / PAGE_SIZE_4K as u64, |pin| {
                // The invalidate read guard covers EOF validation, PTE publication
                // and insertion of its file-owned reference.
                let file_len = self.file.size();
                if self.file_end.is_none() && file_start >= file_len {
                    return Err(KError::BadAddress);
                }
                if page_end > file_len {
                    return Ok(false);
                }
                let mut pages = self.file_pages.lock();
                if pages.contains_key(&object_start) {
                    // invalidate_present's zap_present_range and clear's
                    // runtime.unmap call `on_invalidate_complete` after TLB
                    // finalization; the runtime's release callback or its own
                    // post-finish cleanup removes this token. The sparse unmap
                    // path that retains a token retains its mapped PTE too, so
                    // seeing this key with a missing PTE would mean a caller
                    // violated the PTE/token ownership contract.
                    return Err(KError::AlreadyExists);
                }
                // SAFETY: The invalidation read guard is held by
                // `with_pinned_file_page`; `file_pages` retains the pin until
                // invalidation completion after PTE removal and TLB finish.
                let mapped = unsafe { pin.map_readonly(pgtbl, va, flags - MappingFlags::WRITE)? };
                if flags.contains(MappingFlags::EXECUTE) {
                    match pgtbl.query(va) {
                        Ok((paddr, ..)) => {
                            karch::flush_icache_range(p2v(paddr), self.size.into());
                        }
                        Err(err) => {
                            let rollback = pgtbl.unmap(va);
                            pgtbl.finish();
                            if !matches!(rollback, Ok(_) | Err(PagingError::NotMapped)) {
                                pages.insert(object_start, mapped);
                            }
                            return Err(memspace::backend::map_paging_err(err));
                        }
                    }
                }
                pages.insert(object_start, mapped);
                Ok(true)
            })
    }

    fn cow_file_page(
        &self,
        addr: VirtAddr,
        flags: MappingFlags,
        pgtbl: &mut PageTableMut,
    ) -> KResult<Option<memspace::backend::private::PrivateCowResult>> {
        use memspace::backend::private::PrivateCowResult;
        let local_offset = self.local_offset_for(addr)?;
        let anon_offset = self.anon_offset_for(addr)?;
        let Some(source) = self.file_pages.lock().get(&local_offset).cloned() else {
            return Ok(None);
        };
        let file_offset = self
            .file_start
            .checked_add(local_offset)
            .ok_or(KError::BadAddress)?;
        self.file.mapping().with_invalidate_read(|| {
            // COW publication, like first mapping, must exclude truncate.
            if file_offset >= self.file.size() {
                return Err(KError::BadAddress);
            }
            let expected = pgtbl
                .query_entry(addr)
                .map_err(memspace::backend::map_paging_err)?;
            if expected.page_size() != self.size {
                // `handle_fault` holds the address-space lock and has
                // already verified this runtime's page size. A mismatch
                // here therefore means a corrupt page-table entry, not a
                // transient replacement; report it instead of retrying.
                return Err(KError::BadAddress);
            }
            // The retained mapping owns the exact allocation visible through
            // this PTE. Cache replacement must not trigger a new lookup, I/O,
            // or resident-page quota admission before copying that allocation.
            if !source.matches_paddr(expected.paddr()) {
                // The address-space lock excludes concurrent PTE changes;
                // invalidation that cannot take it is deferred. Thus a
                // PTE belonging to a different pinned file-page version is a
                // non-recoverable consistency violation, not a race that
                // can make progress by retrying.
                return Err(KError::BadAddress);
            }
            let prepared = self.anon.prepare_first_touch_page(anon_offset)?;
            let frame = self.alloc_new_frame(false)?;
            // SAFETY: file tokens are installed only for Size4K mappings, so
            // frame is a fresh exclusive allocation of PAGE_SIZE_4K bytes.
            // MaybeUninit accepts its contents; copying initializes the whole
            // array under the source lock before any PTE exposes the frame.
            let destination = unsafe {
                &mut *p2v(frame)
                    .as_mut_ptr()
                    .cast::<[MaybeUninit<u8>; PAGE_SIZE_4K]>()
            };
            source.copy_to_uninit(destination);
            if let Err(err) = prepared.commit_after(frame, self.size, || {
                pgtbl
                    .replace_if_same(addr, expected, frame, flags)
                    .map(|_| ())
            }) {
                dealloc_frame(frame, self.size);
                return match err {
                    anon::AnonPrivatePageCommitError::Changed
                    | anon::AnonPrivatePageCommitError::Commit(
                        page_table::PteReplaceError::Changed { .. },
                    ) => Ok(Some(PrivateCowResult::Retry)),
                    anon::AnonPrivatePageCommitError::Commit(
                        page_table::PteReplaceError::PageTable(err),
                    ) => Err(memspace::backend::map_paging_err(err)),
                };
            }
            pgtbl.finish();
            let removed = self.file_pages.lock().remove(&local_offset);
            drop(removed);
            if flags.contains(MappingFlags::EXECUTE) {
                karch::flush_icache_range(p2v(frame), self.size.into());
            }
            Ok(Some(PrivateCowResult::Resolved))
        })
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
            let object_start = self.anon_offset_for(addr)?;
            if let Some(result) = self.cow_file_page(addr, flags, pgtbl)? {
                return Ok(if result.is_retry() {
                    FaultCompletion::cow_conflict_retry()
                } else {
                    FaultCompletion::from_populate((1, None))
                });
            }
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
            let reused = !ctx.access_flags().contains(MappingFlags::WRITE)
                && self.try_map_readonly_file_page(
                    addr,
                    file_page_offset.ok_or(KError::BadAddress)?,
                    page_data_offset.ok_or(KError::BadAddress)?,
                    flags,
                    pgtbl,
                )?;
            if !reused {
                self.alloc_new_at(addr, file_page_offset, page_data_offset, flags, pgtbl)?;
            }
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
        object_start: u64,
    ) -> Option<AnonPrivateViewGuard> {
        let observer = observer?;
        Some(anon.register_view(MappingViewSpec {
            mm_id: mm_id?,
            vma_start: start.as_usize() as u64,
            vma_len: len,
            object_start,
            object_len: len,
            kind: MappingViewKind::Private,
            notifier: Some(private_anon_invalidate_notifier(
                invalidate.unwrap_or_else(|| observer.invalidate_handle()),
            )),
        }))
    }

    fn rebuilt_runtime(
        &self,
        source_range: VirtAddrRange,
        new_range: VirtAddrRange,
        new_mm_id: Option<u64>,
        new_observer: Option<&MmObserver>,
        invalidate: Option<InvalidateHandle>,
        fork_child: bool,
    ) -> KResult<Arc<Self>> {
        let local_start = self.local_offset_for(source_range.start)?;
        let source_len = source_range.size() as u64;
        let retained_len = source_len.min(new_range.size() as u64);
        let retained_end = local_start
            .checked_add(retained_len)
            .ok_or(KError::BadAddress)?;
        let anon_start = self.anon_offset_for(source_range.start)?;
        let file_start = self
            .file_start
            .checked_add(local_start)
            .ok_or(KError::BadAddress)?;
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
        let file_pages = if fork_child {
            Arc::new(Mutex::new(BTreeMap::new()))
        } else {
            let mut relocated_pages = BTreeMap::new();
            for (&offset, pin) in self.file_pages.lock().range(local_start..retained_end) {
                relocated_pages.insert(offset - local_start, pin.clone());
            }
            Arc::new(Mutex::new(relocated_pages))
        };
        Ok(Arc::new(Self {
            start: new_range.start,
            len: new_range.size(),
            size: self.size,
            anon: anon.clone(),
            anon_start,
            file_pages,
            file: self.file.clone(),
            file_start,
            file_end: self.file_end,
            mapping_view: Self::register_mapping_view(
                &self.file,
                file_start,
                self.file_end,
                file_context.as_ref(),
                new_range.start,
                new_range.size(),
            ),
            anon_view: Self::register_anon_view(
                &anon,
                new_mm_id,
                invalidate,
                new_observer,
                new_range.start,
                new_range.size(),
                anon_start,
            ),
        }))
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
        let object_start = self.anon_offset_for(range.start)?;
        let result = unmap_private_object_range(&self.anon, object_start, range, self.size, pgtbl);
        if result.is_err() {
            self.release_unmapped_file_pages(range, pgtbl);
            return result;
        }
        // `unmap_private_object_range` finishes PTE/TLB removal before return.
        self.release_file_pages(range);
        Ok(())
    }

    fn on_protect_impl(
        &self,
        _range: VirtAddrRange,
        new_flags: MappingFlags,
        _pgtbl: &mut PageTableMut,
    ) -> KResult<MappingFlags> {
        // Cached file pages and fork-shared anonymous pages must fault for COW.
        Ok(new_flags - MappingFlags::WRITE)
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
                self.handle_unmapped_fault(&ctx, flags, pgtbl, addr, self.anon_offset_for(addr)?)
            }
            Err(_) => Err(KError::BadAddress),
        }
    }
}

impl VmRuntimeOps for FilePrivateRuntime {
    fn private_anon_offset_for(&self, addr: VirtAddr) -> Option<u64> {
        self.anon_offset_for(addr).ok()
    }

    fn on_invalidate_complete(&self, range: VirtAddrRange) {
        self.release_file_pages(range);
    }

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
        let object_start = self.anon_offset_for(range.start)?;
        let result =
            discard_private_object_range(&self.anon, object_start, range, self.size, pgtbl);
        if result.is_err() {
            self.release_unmapped_file_pages(range, pgtbl);
            return result.map(|()| true);
        }
        // `discard_private_object_range` finishes PTE/TLB removal before return.
        self.release_file_pages(range);
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
        source_range: VirtAddrRange,
        new_start: VirtAddr,
        new_len: usize,
        new_mm_id: u64,
        aspace: &Arc<Mutex<MmSpace>>,
        invalidate: Option<InvalidateHandle>,
    ) -> KResult<Arc<dyn VmRuntimeOps>> {
        let observer = MmObserver::new(aspace);
        self.rebuilt_runtime(
            source_range,
            VirtAddrRange::from_start_size(new_start, new_len),
            Some(new_mm_id),
            Some(&observer),
            invalidate,
            false,
        )
        .map(|runtime| runtime as Arc<dyn VmRuntimeOps>)
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
        let object_start = self.anon_offset_for(range.start)?;
        self.file.mapping().with_invalidate_read(|| {
            let file_pages = Arc::new(Mutex::new(BTreeMap::new()));
            let pins = self.file_pages.lock();
            let local_start = self.local_offset_for(range.start)?;
            let end = local_start.saturating_add(range.size() as u64);
            let file_len = self.file.size();
            let mut installed = alloc::vec::Vec::new();
            let materialized = (|| {
                for (&offset, pin) in pins.range(local_start..end) {
                    let addr = self.start + offset as usize;
                    let file_offset = self
                        .file_start
                        .checked_add(offset)
                        .ok_or(KError::BadAddress)?;
                    if old_pgtbl.query(addr).is_err() || file_offset >= file_len {
                        continue;
                    }
                    // SAFETY: The file invalidation read guard is held for this
                    // publication, and the child mapping retains a cloned pin until
                    // its invalidation callback runs after PTE removal and TLB finish.
                    let mapped = unsafe { pin.map_readonly(new_pgtbl, addr, cow_flags)? };
                    installed.push(addr);
                    // The child runtime keeps the parent's coordinate origin, so
                    // retain the original local key even when cloning a sub-VMA.
                    file_pages.lock().insert(offset, mapped);
                }
                drop(pins);
                // Anonymous child slots and their manual frame references commit
                // only after every file mapping has succeeded. This helper rolls
                // back its own PTE updates and references on any failure.
                clone_private_object_pages_for_fork(
                    &self.anon,
                    &child,
                    object_start,
                    range.size(),
                    PrivateForkMaterialize {
                        runtime_start: self.start,
                        object_base: self.anon_start,
                        cow_flags,
                        page_size: self.size,
                    },
                    old_pgtbl,
                    new_pgtbl,
                )
            })();
            if let Err(error) = materialized {
                for installed_addr in installed {
                    new_pgtbl
                        .unmap(installed_addr)
                        .map_err(memspace::backend::map_paging_err)?;
                }
                new_pgtbl.finish();
                return Err(error);
            }

            let invalidate = target.invalidate;
            let observer = MmObserver::new(target.new_aspace);
            let file_context = FileRuntimeContext {
                mm_id: target.new_mm_id,
                observer: &observer,
                invalidate: invalidate
                    .clone()
                    .unwrap_or_else(|| observer.invalidate_handle()),
            };
            let runtime: Arc<dyn VmRuntimeOps> = Arc::new(Self {
                start: self.start,
                len: self.len,
                size: self.size,
                anon: child.clone(),
                anon_start: self.anon_start,
                file_pages,
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
                    self.anon_start,
                ),
            });
            Ok(runtime)
        })
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
            anon_start: 0,
            file_pages: Arc::new(Mutex::new(BTreeMap::new())),
            mapping_view: FilePrivateRuntime::register_mapping_view(
                &file,
                file_start,
                file_end,
                context.as_ref(),
                start,
                len,
            ),
            anon_view: FilePrivateRuntime::register_anon_view(
                &anon, mm_id, invalidate, observer, start, len, 0,
            ),
            file,
            file_start,
            file_end,
        },
    )))
}

unittest::path_mod_test!(tests, "../ut-tests/private.rs");

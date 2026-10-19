// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Anonymous private/COW mapping backend.

use alloc::{sync::Arc, vec::Vec};

use anon::{
    AnonPrivateObject, AnonPrivatePageCommitError, AnonPrivateReleasedPage, AnonPrivateViewGuard,
};
use kerrno::{KError, KResult};
use khal::{
    mem::p2v,
    paging::{MappingFlags, PageSize, PageTableMut, PagingError},
};
use ksync::Mutex;
use memaddr::{MemoryAddr, VirtAddr, VirtAddrRange};
use page_table::PteReplaceError;
use vmobj::{
    MappingViewId, MappingViewKind, MappingViewNotifier, MappingViewSpec, ObjectInvalidateWork,
    ObjectViewHit,
};

use super::{alloc_frame, dealloc_frame, map_paging_err};
use crate::{
    FaultContext, ForkCloneTarget, InvalidateHandle, MmSpace, VmArea, VmBackingInfo, VmBackingKind,
    backend::{BackendOps, FaultCompletion, FaultCompletionResult},
    vma::VmRuntimeOps,
};

#[derive(Clone)]
pub struct PrivateBackend {
    start: VirtAddr,
    size: PageSize,
    object: Arc<AnonPrivateObject>,
    object_view: Option<AnonPrivateViewGuard>,
}

impl PrivateBackend {
    pub fn new(start: VirtAddr, pgsize: PageSize) -> Self {
        Self {
            start,
            size: pgsize,
            object: AnonPrivateObject::new_root(),
            object_view: None,
        }
    }

    pub fn relocated(&self, new_start: VirtAddr) -> Self {
        Self {
            start: new_start,
            size: self.size,
            object: self.object.clone(),
            object_view: None,
        }
    }

    fn registration_id(&self) -> Option<MappingViewId> {
        self.object_view.as_ref().map(AnonPrivateViewGuard::id)
    }

    pub fn register_object_view(
        &self,
        mm_id: u64,
        invalidate: InvalidateHandle,
        vma: &VmArea,
    ) -> Self {
        let object_start = vma.page_offset() * memaddr::PAGE_SIZE_4K as u64;
        Self {
            start: self.start,
            size: self.size,
            object: self.object.clone(),
            object_view: Some(self.object.register_view(MappingViewSpec {
                mm_id,
                vma_start: vma.start().as_usize() as u64,
                vma_len: vma.size(),
                object_start,
                object_len: vma.size(),
                kind: MappingViewKind::Private,
                notifier: Some(PrivateAnonInvalidate::new(invalidate)),
            })),
        }
    }

    fn object_offset_for(&self, addr: VirtAddr) -> KResult<u64> {
        addr.as_usize()
            .checked_sub(self.start.as_usize())
            .map(|it| it as u64)
            .ok_or(KError::BadAddress)
    }

    fn alloc_new_at(&self, va: VirtAddr, flags: MappingFlags, pgtbl: &mut PageTableMut) -> KResult {
        let object_start = self.object_offset_for(va)?;
        let prepared = self.object.prepare_first_touch_page(object_start)?;
        let frame = alloc_frame(true, self.size)?;
        if let Err(err) = pgtbl.map(va, frame, self.size, flags) {
            dealloc_frame(frame, self.size);
            return Err(map_paging_err(err));
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

    pub fn clone_for_fork_runtime(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        old_pgtbl: &mut PageTableMut,
        new_pgtbl: &mut PageTableMut,
        _new_aspace: &Arc<Mutex<MmSpace>>,
        _invalidate: Option<InvalidateHandle>,
    ) -> KResult<Self> {
        let child = self.object.fork_child();
        let cow_flags = flags - MappingFlags::WRITE;
        let object_start = self.object_offset_for(range.start)?;
        clone_private_object_pages_for_fork(
            &self.object,
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
        Ok(Self {
            start: self.start,
            size: self.size,
            object: child,
            object_view: None,
        })
    }
}

pub struct PrivateAnonInvalidate {
    handle: InvalidateHandle,
}

impl PrivateAnonInvalidate {
    fn new(handle: InvalidateHandle) -> Arc<Self> {
        Arc::new(Self { handle })
    }
}

pub fn private_anon_invalidate_notifier(handle: InvalidateHandle) -> Arc<dyn MappingViewNotifier> {
    PrivateAnonInvalidate::new(handle)
}

impl MappingViewNotifier for PrivateAnonInvalidate {
    fn invalidate(&self, work: &ObjectInvalidateWork, hit: &ObjectViewHit) {
        let Some(request) = work.request_for_hit(hit) else {
            warn!(
                "Ignored anonymous private hit {} not carried by its invalidate work",
                hit.view().id().raw()
            );
            return;
        };
        if let Err(err) = self.handle.submit(request) {
            warn!(
                "Failed to invalidate anonymous private view {}: {err}",
                hit.view().id().raw()
            );
        }
    }
}

impl BackendOps for PrivateBackend {
    fn page_size(&self) -> PageSize {
        self.size
    }

    fn backing_info(&self) -> VmBackingInfo {
        VmBackingInfo::new(
            VmBackingKind::AnonymousPrivate {
                object: self.object.id(),
            },
            self.page_size(),
        )
    }

    fn map(
        &self,
        _range: VirtAddrRange,
        _flags: MappingFlags,
        _pgtbl: &mut PageTableMut,
    ) -> KResult {
        Ok(())
    }

    fn unmap(&self, range: VirtAddrRange, pgtbl: &mut PageTableMut) -> KResult {
        let object_start = self.object_offset_for(range.start)?;
        unmap_private_object_range(&self.object, object_start, range, self.size, pgtbl)
    }

    fn handle_fault(
        &self,
        ctx: FaultContext,
        flags: MappingFlags,
        pgtbl: &mut PageTableMut,
    ) -> FaultCompletionResult {
        let _ = self.registration_id();
        let addr = ctx.address().align_down(self.page_size());
        let object_start = self.object_offset_for(addr)?;
        let populated = match pgtbl.query(addr) {
            Ok((_paddr, page_flags, page_size)) => {
                assert_eq!(self.size, page_size);
                if ctx.access_flags().contains(MappingFlags::WRITE)
                    && !page_flags.contains(MappingFlags::WRITE)
                {
                    if cow_private_object_page(
                        &self.object,
                        object_start,
                        addr,
                        flags,
                        self.size,
                        pgtbl,
                    )?
                    .is_retry()
                    {
                        return Ok(FaultCompletion::cow_conflict_retry());
                    }
                    1
                } else if page_flags.contains(ctx.access_flags()) {
                    1
                } else {
                    0
                }
            }
            Err(PagingError::NotMapped) => {
                let existing = map_existing_private_object_page(
                    &self.object,
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
                    self.alloc_new_at(addr, flags, pgtbl)?;
                }
                1
            }
            Err(_) => return Err(KError::BadAddress),
        };
        Ok(FaultCompletion::from_populate((populated, None)))
    }
}

impl VmRuntimeOps for PrivateBackend {
    fn backing_info(&self) -> VmBackingInfo {
        BackendOps::backing_info(self)
    }

    fn map(&self, range: VirtAddrRange, flags: MappingFlags, pgtbl: &mut PageTableMut) -> KResult {
        BackendOps::map(self, range, flags, pgtbl)
    }

    fn unmap(&self, range: VirtAddrRange, pgtbl: &mut PageTableMut) -> KResult {
        BackendOps::unmap(self, range, pgtbl)
    }

    fn on_protect(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        pgtbl: &mut PageTableMut,
    ) -> KResult<MappingFlags> {
        BackendOps::on_protect(self, range, flags, pgtbl)
    }

    fn madvise_dontneed(
        &self,
        vma: &VmArea,
        range: VirtAddrRange,
        pgtbl: &mut PageTableMut,
    ) -> KResult<bool> {
        let object_start = vma
            .backing_offset_for(range.start)
            .ok_or(KError::BadAddress)?;
        discard_private_object_range(&self.object, object_start, range, self.size, pgtbl)?;
        Ok(true)
    }

    fn handle_fault(
        &self,
        ctx: FaultContext,
        flags: MappingFlags,
        pgtbl: &mut PageTableMut,
    ) -> FaultCompletionResult {
        BackendOps::handle_fault(self, ctx, flags, pgtbl)
    }

    fn relocate_for_mremap(
        &self,
        new_start: VirtAddr,
        _new_mm_id: u64,
        _aspace: &Arc<Mutex<MmSpace>>,
        _invalidate: Option<InvalidateHandle>,
    ) -> KResult<Arc<dyn VmRuntimeOps>> {
        Ok(Arc::new(self.relocated(new_start)))
    }

    fn clone_for_fork(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        old_pgtbl: &mut PageTableMut,
        new_pgtbl: &mut PageTableMut,
        target: ForkCloneTarget<'_>,
    ) -> KResult<Arc<dyn VmRuntimeOps>> {
        self.clone_for_fork_runtime(
            range,
            flags,
            old_pgtbl,
            new_pgtbl,
            target.new_aspace,
            target.invalidate,
        )
        .map(|runtime| Arc::new(runtime) as Arc<dyn VmRuntimeOps>)
    }
}

/// Releases detached private-anon frames after the caller finished tearing
/// down every visible PTE that referenced them.
pub fn release_private_object_pages(pages: Vec<AnonPrivateReleasedPage>) {
    for page in pages {
        dealloc_frame(page.phys_addr(), page.page_size());
    }
}

pub enum PrivateCowResult {
    Resolved,
    Retry,
}

impl PrivateCowResult {
    pub const fn is_retry(&self) -> bool {
        matches!(self, Self::Retry)
    }
}

pub enum ExistingPrivatePageResult {
    Missing,
    Resolved,
    Retry,
}

impl ExistingPrivatePageResult {
    pub const fn is_resolved(&self) -> bool {
        matches!(self, Self::Resolved)
    }

    pub const fn is_retry(&self) -> bool {
        matches!(self, Self::Retry)
    }
}

/// Resolves a write fault on one existing private-anon object page.
pub fn cow_private_object_page(
    object: &Arc<AnonPrivateObject>,
    object_start: u64,
    addr: VirtAddr,
    flags: MappingFlags,
    size: PageSize,
    pgtbl: &mut PageTableMut,
) -> KResult<PrivateCowResult> {
    let page = object.page_at(object_start).ok_or(KError::BadAddress)?;
    if page.is_exclusive() {
        pgtbl.protect(addr, flags).map_err(super::map_paging_err)?;
        return Ok(PrivateCowResult::Resolved);
    }
    let expected = pgtbl.query_entry(addr).map_err(super::map_paging_err)?;
    if expected.paddr() != page.phys_addr() || expected.page_size() != size {
        return Ok(PrivateCowResult::Retry);
    }
    let new_frame = alloc_frame(false, size)?;
    copy_private_page(&page, new_frame, size);
    let released =
        match object.replace_page_if_same_after(object_start, &page, new_frame, size, || {
            pgtbl
                .replace_if_same(addr, expected, new_frame, flags)
                .map(|_| ())
        }) {
            Ok(released) => released,
            Err(AnonPrivatePageCommitError::Changed)
            | Err(AnonPrivatePageCommitError::Commit(PteReplaceError::Changed { .. })) => {
                dealloc_frame(new_frame, size);
                return Ok(PrivateCowResult::Retry);
            }
            Err(AnonPrivatePageCommitError::Commit(PteReplaceError::PageTable(err))) => {
                dealloc_frame(new_frame, size);
                return Err(super::map_paging_err(err));
            }
        };
    pgtbl.finish();
    release_private_object_pages(released.into_iter().collect());
    if flags.contains(MappingFlags::EXECUTE) {
        karch::flush_icache_range(p2v(new_frame), size.into());
    }
    Ok(PrivateCowResult::Resolved)
}

fn copy_private_page(
    source: &anon::AnonPrivatePageHandle,
    new_frame: memaddr::PhysAddr,
    size: PageSize,
) {
    assert_eq!(source.page_size(), size);
    // SAFETY: both source and destination are valid kernel direct-map pages of
    // `size` bytes. `new_frame` was freshly allocated with the same `size`
    // and does not overlap the source private page.
    unsafe {
        core::ptr::copy_nonoverlapping(
            p2v(source.phys_addr()).as_ptr(),
            p2v(new_frame).as_mut_ptr(),
            size as _,
        );
    }
}

fn cow_unmapped_private_object_page(
    object: &Arc<AnonPrivateObject>,
    object_start: u64,
    page: &anon::AnonPrivatePageHandle,
    addr: VirtAddr,
    flags: MappingFlags,
    size: PageSize,
    pgtbl: &mut PageTableMut,
) -> KResult<PrivateCowResult> {
    let new_frame = alloc_frame(false, size)?;
    copy_private_page(page, new_frame, size);
    let released =
        match object.replace_page_if_same_after(object_start, page, new_frame, size, || {
            pgtbl.map(addr, new_frame, size, flags)
        }) {
            Ok(released) => released,
            Err(AnonPrivatePageCommitError::Changed)
            | Err(AnonPrivatePageCommitError::Commit(PagingError::AlreadyMapped)) => {
                dealloc_frame(new_frame, size);
                return Ok(PrivateCowResult::Retry);
            }
            Err(AnonPrivatePageCommitError::Commit(err)) => {
                dealloc_frame(new_frame, size);
                return Err(super::map_paging_err(err));
            }
        };
    pgtbl.finish();
    release_private_object_pages(released.into_iter().collect());
    if flags.contains(MappingFlags::EXECUTE) {
        karch::flush_icache_range(p2v(new_frame), size.into());
    }
    Ok(PrivateCowResult::Resolved)
}

/// Materializes one existing private-anon object page into the caller page
/// table, including COW on write access when the object slot is shared.
pub fn map_existing_private_object_page(
    object: &Arc<AnonPrivateObject>,
    object_start: u64,
    addr: VirtAddr,
    access_flags: MappingFlags,
    install_flags: MappingFlags,
    size: PageSize,
    pgtbl: &mut PageTableMut,
) -> KResult<ExistingPrivatePageResult> {
    let Some(page) = object.page_at(object_start) else {
        return Ok(ExistingPrivatePageResult::Missing);
    };
    if access_flags.contains(MappingFlags::WRITE) && !page.is_exclusive() {
        if cow_unmapped_private_object_page(
            object,
            object_start,
            &page,
            addr,
            install_flags,
            size,
            pgtbl,
        )?
        .is_retry()
        {
            return Ok(ExistingPrivatePageResult::Retry);
        }
    } else {
        let page_flags = if page.is_exclusive() {
            install_flags
        } else {
            install_flags - MappingFlags::WRITE
        };
        pgtbl
            .map(addr, page.phys_addr(), size, page_flags)
            .map_err(super::map_paging_err)?;
    }
    Ok(ExistingPrivatePageResult::Resolved)
}

/// Detaches one private-anon object range, tears down the corresponding PTEs,
/// and releases frames whose last object reference disappeared.
pub fn unmap_private_object_range(
    object: &Arc<AnonPrivateObject>,
    object_start: u64,
    range: VirtAddrRange,
    size: PageSize,
    pgtbl: &mut PageTableMut,
) -> KResult {
    let detached = object.detach_range(object_start, range.size());
    pgtbl
        .unmap_sparse_region(range.start, range.size(), size)
        .map_err(super::map_paging_err)?;
    pgtbl.finish();
    release_private_object_pages(detached.finalize_release());
    Ok(())
}

/// Discards one private-anon object range for `MADV_DONTNEED`.
///
/// The object loses ownership before the visible PTEs are removed, but the
/// detached frame releases are finalized only after the page table flush. The
/// final object-side invalidation keeps registered aliases consistent.
pub fn discard_private_object_range(
    object: &Arc<AnonPrivateObject>,
    object_start: u64,
    range: VirtAddrRange,
    size: PageSize,
    pgtbl: &mut PageTableMut,
) -> KResult {
    unmap_private_object_range(object, object_start, range, size, pgtbl)?;
    let _ = object.invalidate_range(object_start, range.size());
    Ok(())
}

pub struct PrivateForkMaterialize {
    pub runtime_start: VirtAddr,
    pub cow_flags: MappingFlags,
    pub page_size: PageSize,
}

struct ForkMappedPage {
    vaddr: VirtAddr,
    parent_flags: MappingFlags,
}

fn rollback_private_fork_pages(
    mapped: &[ForkMappedPage],
    current_parent: Option<(VirtAddr, MappingFlags)>,
    page_size: PageSize,
    old_pgtbl: &mut PageTableMut,
    new_pgtbl: &mut PageTableMut,
) -> KResult {
    if let Some((vaddr, parent_flags)) = current_parent {
        let restored_size = old_pgtbl
            .protect(vaddr, parent_flags)
            .map_err(super::map_paging_err)?;
        assert_eq!(restored_size, page_size);
    }
    for page in mapped.iter().rev() {
        if let Ok((_paddr, _flags, unmapped_size)) = new_pgtbl.unmap(page.vaddr) {
            assert_eq!(unmapped_size, page_size);
        }
        let restored_size = old_pgtbl
            .protect(page.vaddr, page.parent_flags)
            .map_err(super::map_paging_err)?;
        assert_eq!(restored_size, page_size);
    }
    old_pgtbl.finish();
    new_pgtbl.finish();
    Ok(())
}

/// Shares one private-anon object range into a fork child by preparing shared
/// slots, installing child page-table entries, and committing the child object
/// state only after every page-table update succeeds.
pub fn clone_private_object_pages_for_fork(
    object: &Arc<AnonPrivateObject>,
    child: &Arc<AnonPrivateObject>,
    object_start: u64,
    object_len: usize,
    ctx: PrivateForkMaterialize,
    old_pgtbl: &mut PageTableMut,
    new_pgtbl: &mut PageTableMut,
) -> KResult {
    let prepared = object.prepare_fork_child_pages(object_start, object_len, child)?;
    let mut mapped = Vec::new();
    for shared in prepared.pages() {
        let vaddr = ctx.runtime_start + shared.object_start() as usize;
        match old_pgtbl.query(vaddr) {
            Ok((_paddr, parent_flags, mapped_size)) => {
                assert_eq!(mapped_size, ctx.page_size);
                if let Err(err) = old_pgtbl.protect(vaddr, ctx.cow_flags) {
                    rollback_private_fork_pages(
                        &mapped,
                        None,
                        ctx.page_size,
                        old_pgtbl,
                        new_pgtbl,
                    )?;
                    return Err(super::map_paging_err(err));
                }
                if let Err(err) = new_pgtbl.map(
                    vaddr,
                    shared.handle().phys_addr(),
                    ctx.page_size,
                    ctx.cow_flags,
                ) {
                    rollback_private_fork_pages(
                        &mapped,
                        Some((vaddr, parent_flags)),
                        ctx.page_size,
                        old_pgtbl,
                        new_pgtbl,
                    )?;
                    return Err(super::map_paging_err(err));
                }
                mapped.push(ForkMappedPage {
                    vaddr,
                    parent_flags,
                });
            }
            Err(PagingError::NotMapped) => {}
            Err(_) => {
                rollback_private_fork_pages(&mapped, None, ctx.page_size, old_pgtbl, new_pgtbl)?;
                return Err(KError::BadAddress);
            }
        };
    }
    old_pgtbl.finish();
    new_pgtbl.finish();
    if let Err(err) = prepared.commit() {
        rollback_private_fork_pages(&mapped, None, ctx.page_size, old_pgtbl, new_pgtbl)?;
        return Err(err);
    }
    Ok(())
}

unittest::path_mod_test!(tests, "../../ut-tests/backend_private.rs");

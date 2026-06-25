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

use super::{alloc_frame, dealloc_frame, map_paging_err, pages_in};
use crate::{
    FaultContext, ForkCloneTarget, InvalidateHandle, MmSpace, VmArea, VmBackingInfo, VmBackingKind,
    backend::{BackendOps, FaultCompat, FaultCompatResult},
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
    ) -> FaultCompatResult {
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
                        return Ok(FaultCompat::cow_conflict_retry());
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
                    return Ok(FaultCompat::cow_conflict_retry());
                }
                if !existing.is_resolved() {
                    self.alloc_new_at(addr, flags, pgtbl)?;
                }
                1
            }
            Err(_) => return Err(KError::BadAddress),
        };
        Ok(FaultCompat::from_populate((populated, None)))
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
    ) -> FaultCompatResult {
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
    for addr in pages_in(range, size)? {
        if let Ok((_frame, _flags, page_size)) = pgtbl.unmap(addr) {
            assert_eq!(page_size, size);
        }
    }
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

#[cfg(unittest)]
mod tests {
    use core::{ptr, slice};

    use khal::{
        mem::{PhysAddr, p2v},
        paging::{MappingFlags, PageSize, PageTable},
        trap::PageFaultFlags,
    };
    use memaddr::{PAGE_SIZE_4K, VirtAddr, VirtAddrRange};
    use unittest::def_test;

    use super::{
        BackendOps, PrivateBackend, PrivateForkMaterialize, clone_private_object_pages_for_fork,
        cow_private_object_page, map_existing_private_object_page,
    };
    use crate::{FaultContext, VmArea, vma::VmRuntimeOps};

    fn fill_frame_prefix(pa: PhysAddr, value: u8, len: usize) {
        assert!(len <= PAGE_SIZE_4K);
        // SAFETY: Unit tests only pass allocated frames that remain owned by an
        // AnonPrivateObject for at least `len` bytes; the assertion above
        // keeps the test prefix within the 4K frame.
        unsafe {
            ptr::write_bytes(p2v(pa).as_mut_ptr(), value, len);
        }
    }

    fn assert_frame_prefix(pa: PhysAddr, value: u8, len: usize) {
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
    fn private_backend_madvise_then_fork_then_write_fault_refaults_clean_child_page() {
        let start = VirtAddr::from_usize(0x40_0000);
        let range = VirtAddrRange::from_start_size(start, PAGE_SIZE_4K);
        let flags = MappingFlags::READ | MappingFlags::WRITE;
        let backend = PrivateBackend::new(start, PageSize::Size4K);
        let vma = VmArea::new(
            start,
            PAGE_SIZE_4K,
            flags,
            flags,
            BackendOps::backing_info(&backend),
            0,
            None,
        );
        let mut parent_pt = PageTable::try_new().expect("allocate parent page table");

        {
            let mut pgtbl = parent_pt.modify();
            let fault = FaultContext::new(start, PageFaultFlags::WRITE);
            let compat =
                BackendOps::handle_fault(&backend, fault, flags, &mut pgtbl).expect("fault in");
            assert_eq!(
                compat.populated(),
                1,
                "initial private fault should materialize one page"
            );
        }
        assert!(
            backend.object.page_at(0).is_some(),
            "parent object should own one page"
        );
        let parent_page = backend
            .object
            .page_at(0)
            .expect("parent page should exist before MADV_DONTNEED");
        fill_frame_prefix(parent_page.phys_addr(), 0x5a, 64);

        {
            let mut pgtbl = parent_pt.modify();
            let handled = VmRuntimeOps::madvise_dontneed(&backend, &vma, range, &mut pgtbl)
                .expect("madvise DONTNEED");
            assert!(handled, "private backend should handle MADV_DONTNEED");
        }
        assert!(
            backend.object.page_at(0).is_none(),
            "madvise DONTNEED should drop parent private page state"
        );
        {
            let pgtbl = parent_pt.modify();
            assert!(
                pgtbl.query(start).is_err(),
                "madvise DONTNEED should also tear down the present PTE"
            );
        }

        let child_object = backend.object.fork_child();
        let mut child_pt = PageTable::try_new().expect("allocate child page table");
        {
            let mut old_pgtbl = parent_pt.modify();
            let mut new_pgtbl = child_pt.modify();
            clone_private_object_pages_for_fork(
                &backend.object,
                &child_object,
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
            .expect("fork after DONTNEED on empty parent object");
        }
        assert!(
            child_object.page_at(0).is_none(),
            "forking an already-discarded private range must not seed child page state"
        );

        let child = PrivateBackend {
            start,
            size: PageSize::Size4K,
            object: child_object,
            object_view: None,
        };
        {
            let mut pgtbl = child_pt.modify();
            let fault = FaultContext::new(start, PageFaultFlags::WRITE);
            let compat =
                BackendOps::handle_fault(&child, fault, flags, &mut pgtbl).expect("child refault");
            assert_eq!(
                compat.populated(),
                1,
                "child refault should materialize one fresh page"
            );
        }
        let child_page = child
            .object
            .page_at(0)
            .expect("child should own a fresh private page after refault");
        assert!(
            child_page.is_exclusive(),
            "fresh child refault should produce an exclusive private page"
        );
        assert_frame_prefix(child_page.phys_addr(), 0, 64);
        assert!(
            backend.object.page_at(0).is_none(),
            "parent object should stay discarded after child refault"
        );
    }

    #[def_test]
    fn private_backend_madvise_then_same_mapping_refaults_zero_page() {
        let start = VirtAddr::from_usize(0x44_0000);
        let range = VirtAddrRange::from_start_size(start, PAGE_SIZE_4K);
        let flags = MappingFlags::READ | MappingFlags::WRITE;
        let backend = PrivateBackend::new(start, PageSize::Size4K);
        let vma = VmArea::new(
            start,
            PAGE_SIZE_4K,
            flags,
            flags,
            BackendOps::backing_info(&backend),
            0,
            None,
        );
        let mut pgtbl_owner = PageTable::try_new().expect("allocate page table");

        {
            let mut pgtbl = pgtbl_owner.modify();
            let fault = FaultContext::new(start, PageFaultFlags::WRITE);
            let compat = BackendOps::handle_fault(&backend, fault, flags, &mut pgtbl)
                .expect("initial private fault");
            assert_eq!(compat.populated(), 1);
        }
        let old_page = backend
            .object
            .page_at(0)
            .expect("initial private page should exist");
        fill_frame_prefix(old_page.phys_addr(), 0xa5, 64);
        drop(old_page);

        {
            let mut pgtbl = pgtbl_owner.modify();
            let handled = VmRuntimeOps::madvise_dontneed(&backend, &vma, range, &mut pgtbl)
                .expect("madvise DONTNEED");
            assert!(handled);
        }
        assert!(
            backend.object.page_at(0).is_none(),
            "discard must remove the private object slot"
        );

        {
            let mut pgtbl = pgtbl_owner.modify();
            let fault = FaultContext::new(start, PageFaultFlags::WRITE);
            let compat = BackendOps::handle_fault(&backend, fault, flags, &mut pgtbl)
                .expect("same mapping refault");
            assert_eq!(compat.populated(), 1);
        }
        let new_page = backend
            .object
            .page_at(0)
            .expect("same mapping should own a refaulted private page");
        assert!(new_page.is_exclusive());
        assert_frame_prefix(new_page.phys_addr(), 0, 64);
    }

    #[def_test]
    fn private_fork_rolls_back_parent_write_protect_when_child_map_fails() {
        let start = VirtAddr::from_usize(0x50_0000);
        let flags = MappingFlags::READ | MappingFlags::WRITE;
        let backend = PrivateBackend::new(start, PageSize::Size4K);
        let mut parent_pt = PageTable::try_new().expect("allocate parent page table");

        {
            let mut pgtbl = parent_pt.modify();
            let fault = FaultContext::new(start, PageFaultFlags::WRITE);
            BackendOps::handle_fault(&backend, fault, flags, &mut pgtbl).expect("fault in");
        }
        let parent_page = backend
            .object
            .page_at(0)
            .expect("parent should own one page");
        assert!(parent_page.is_exclusive());

        let child_object = backend.object.fork_child();
        let mut child_pt = PageTable::try_new().expect("allocate child page table");
        {
            let mut pgtbl = child_pt.modify();
            pgtbl
                .map(
                    start,
                    PhysAddr::from_usize(0x80_0000),
                    PageSize::Size4K,
                    flags,
                )
                .expect("seed conflicting child mapping");
        }

        {
            let mut old_pgtbl = parent_pt.modify();
            let mut new_pgtbl = child_pt.modify();
            assert!(
                clone_private_object_pages_for_fork(
                    &backend.object,
                    &child_object,
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
                .is_err(),
                "fork clone must fail on pre-existing child PTE"
            );
        }

        let (_pa, parent_flags, _size) = parent_pt
            .modify()
            .query(start)
            .expect("parent mapping must remain installed");
        assert!(
            parent_flags.contains(MappingFlags::WRITE),
            "rollback must restore parent write permission"
        );
        assert!(
            child_object.page_at(0).is_none(),
            "failed fork must not publish child object page state"
        );
        assert!(
            parent_page.is_exclusive(),
            "prepared fork references must be released on rollback"
        );
    }

    #[def_test]
    fn private_cow_write_fault_replaces_parent_and_preserves_child_page() {
        let start = VirtAddr::from_usize(0x60_0000);
        let flags = MappingFlags::READ | MappingFlags::WRITE;
        let backend = PrivateBackend::new(start, PageSize::Size4K);
        let mut parent_pt = PageTable::try_new().expect("allocate parent page table");

        {
            let mut pgtbl = parent_pt.modify();
            let fault = FaultContext::new(start, PageFaultFlags::WRITE);
            BackendOps::handle_fault(&backend, fault, flags, &mut pgtbl).expect("fault in");
        }
        let original_page = backend
            .object
            .page_at(0)
            .expect("parent should own one page");
        let original_pa = original_page.phys_addr();

        let child_object = backend.object.fork_child();
        let mut child_pt = PageTable::try_new().expect("allocate child page table");
        {
            let mut old_pgtbl = parent_pt.modify();
            let mut new_pgtbl = child_pt.modify();
            clone_private_object_pages_for_fork(
                &backend.object,
                &child_object,
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
            .expect("fork private page");
        }
        assert!(
            !original_page.is_exclusive(),
            "fork should share the original private page"
        );

        {
            let mut pgtbl = parent_pt.modify();
            let fault = FaultContext::new(start, PageFaultFlags::WRITE);
            let compat =
                BackendOps::handle_fault(&backend, fault, flags, &mut pgtbl).expect("COW fault");
            assert_eq!(compat.populated(), 1);
        }

        let parent_page = backend
            .object
            .page_at(0)
            .expect("parent should publish replacement page");
        let child_page = child_object
            .page_at(0)
            .expect("child should keep original shared page");
        assert_ne!(parent_page.phys_addr(), original_pa);
        assert_eq!(child_page.phys_addr(), original_pa);
        assert!(parent_page.is_exclusive());
        assert!(child_page.is_exclusive());

        let (parent_pa, parent_flags, _) = parent_pt
            .modify()
            .query(start)
            .expect("parent PTE should remain mapped");
        assert_eq!(parent_pa, parent_page.phys_addr());
        assert!(parent_flags.contains(MappingFlags::WRITE));

        let (child_pa, child_flags, _) = child_pt
            .modify()
            .query(start)
            .expect("child PTE should remain mapped");
        assert_eq!(child_pa, original_pa);
        assert!(!child_flags.contains(MappingFlags::WRITE));
    }

    #[def_test]
    fn private_cow_write_fault_reports_retry_when_pte_changed() {
        let start = VirtAddr::from_usize(0x70_0000);
        let flags = MappingFlags::READ | MappingFlags::WRITE;
        let backend = PrivateBackend::new(start, PageSize::Size4K);
        let mut parent_pt = PageTable::try_new().expect("allocate parent page table");

        {
            let mut pgtbl = parent_pt.modify();
            let fault = FaultContext::new(start, PageFaultFlags::WRITE);
            BackendOps::handle_fault(&backend, fault, flags, &mut pgtbl).expect("fault in");
        }
        let original_page = backend
            .object
            .page_at(0)
            .expect("parent should own one page");
        let child_object = backend.object.fork_child();
        let mut child_pt = PageTable::try_new().expect("allocate child page table");
        {
            let mut old_pgtbl = parent_pt.modify();
            let mut new_pgtbl = child_pt.modify();
            clone_private_object_pages_for_fork(
                &backend.object,
                &child_object,
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
            .expect("fork private page");

            let replacement = PhysAddr::from_usize(0x90_0000);
            old_pgtbl
                .remap(start, replacement, flags - MappingFlags::WRITE)
                .expect("simulate competing PTE replacement");
            let result = cow_private_object_page(
                &backend.object,
                0,
                start,
                flags,
                PageSize::Size4K,
                &mut old_pgtbl,
            )
            .expect("COW helper should convert changed PTE to retry");
            assert!(result.is_retry());
        }

        let current_page = backend
            .object
            .page_at(0)
            .expect("object slot should remain original");
        assert_eq!(current_page.phys_addr(), original_page.phys_addr());
        assert_eq!(
            parent_pt
                .modify()
                .query(start)
                .expect("competing PTE should remain installed")
                .0,
            PhysAddr::from_usize(0x90_0000)
        );
    }

    #[def_test]
    fn private_cow_unmapped_write_fault_reports_retry_when_child_map_conflicts() {
        let start = VirtAddr::from_usize(0x80_0000);
        let flags = MappingFlags::READ | MappingFlags::WRITE;
        let backend = PrivateBackend::new(start, PageSize::Size4K);
        let mut parent_pt = PageTable::try_new().expect("allocate parent page table");

        {
            let mut pgtbl = parent_pt.modify();
            let fault = FaultContext::new(start, PageFaultFlags::WRITE);
            BackendOps::handle_fault(&backend, fault, flags, &mut pgtbl).expect("fault in");
        }
        let original_page = backend
            .object
            .page_at(0)
            .expect("parent should own one page");
        let child_object = backend.object.fork_child();
        let mut child_pt = PageTable::try_new().expect("allocate child page table");
        {
            let mut old_pgtbl = parent_pt.modify();
            let mut new_pgtbl = child_pt.modify();
            clone_private_object_pages_for_fork(
                &backend.object,
                &child_object,
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
            .expect("fork private page");
        }

        let mut child_retry_pt = PageTable::try_new().expect("allocate retry page table");
        {
            let mut pgtbl = child_retry_pt.modify();
            pgtbl
                .map(
                    start,
                    PhysAddr::from_usize(0xa0_0000),
                    PageSize::Size4K,
                    flags - MappingFlags::WRITE,
                )
                .expect("simulate concurrent PTE install");
            let result = map_existing_private_object_page(
                &child_object,
                0,
                start,
                MappingFlags::WRITE,
                flags,
                PageSize::Size4K,
                &mut pgtbl,
            )
            .expect("existing private page helper");
            assert!(result.is_retry());
        }

        assert_eq!(
            child_object
                .page_at(0)
                .expect("child object slot should remain shared")
                .phys_addr(),
            original_page.phys_addr()
        );
        assert!(
            !original_page.is_exclusive(),
            "retry must not drop existing shared refs"
        );
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared mapping backend.
use alloc::{sync::Arc, vec::Vec};
use core::ops::Deref;

use anon::{AnonSharedObject, AnonSharedViewGuard};
use kerrno::{KError, KResult};
use khal::paging::{MappingFlags, PageSize, PageTableMut, PagingError};
use ksync::Mutex;
use memaddr::{MemoryAddr, PhysAddr, VirtAddr, VirtAddrRange};
use vmobj::{
    MappingViewId, MappingViewKind, MappingViewNotifier, MappingViewSpec, ObjectInvalidateWork,
    ObjectViewHit,
};

use super::{alloc_frame, dealloc_frame};
use crate::{
    FaultContext, ForkCloneTarget, InvalidateHandle, MmSpace, VmArea, VmBackingInfo, VmBackingKind,
    backend::{
        BackendOps, FaultCompletion, FaultCompletionResult, divide_page, map_paging_err, pages_in,
    },
    vma::VmRuntimeOps,
};

/// Shared physical pages backing a mapping.
pub struct SharedPages {
    pub phys_pages: Vec<PhysAddr>,
    pub size: PageSize,
}
impl SharedPages {
    /// Allocate a new set of shared pages.
    pub fn new(size: usize, pgsize: PageSize) -> KResult<Self> {
        let num_pages = divide_page(size, pgsize);
        let mut result = Self {
            phys_pages: Vec::with_capacity(num_pages),
            size: pgsize,
        };
        for _ in 0..num_pages {
            result.phys_pages.push(alloc_frame(true, pgsize)?);
        }
        Ok(result)
    }

    /// Return the number of pages.
    pub fn len(&self) -> usize {
        self.phys_pages.len()
    }

    /// Returns `true` if there are no pages.
    pub fn is_empty(&self) -> bool {
        self.phys_pages.is_empty()
    }
}

impl Deref for SharedPages {
    type Target = [PhysAddr];

    fn deref(&self) -> &Self::Target {
        &self.phys_pages
    }
}

impl Drop for SharedPages {
    fn drop(&mut self) {
        for frame in &self.phys_pages {
            dealloc_frame(*frame, self.size);
        }
    }
}

// FIXME: This implementation does not allow map or unmap partial ranges.
#[derive(Clone)]
pub struct SharedBackend {
    start: VirtAddr,
    pages: Arc<SharedPages>,
    object: Arc<AnonSharedObject>,
    object_view: Option<AnonSharedViewGuard>,
}
impl SharedBackend {
    pub fn new(start: VirtAddr, pages: Arc<SharedPages>, object: Arc<AnonSharedObject>) -> Self {
        Self {
            start,
            object,
            pages,
            object_view: None,
        }
    }

    pub fn new_anonymous(start: VirtAddr, size: usize, pgsize: PageSize) -> KResult<Self> {
        let pages = Arc::new(SharedPages::new(size, pgsize)?);
        Ok(Self {
            start,
            object: AnonSharedObject::new(),
            pages,
            object_view: None,
        })
    }

    /// Access the shared page set.
    pub fn pages(&self) -> &Arc<SharedPages> {
        &self.pages
    }

    fn registration_id(&self) -> Option<MappingViewId> {
        self.object_view.as_ref().map(AnonSharedViewGuard::id)
    }

    /// Create a relocated shared backend at a new start address with the same pages.
    pub fn relocated(&self, new_start: VirtAddr) -> Self {
        Self {
            start: new_start,
            pages: self.pages.clone(),
            object: self.object.clone(),
            object_view: None,
        }
    }

    /// Returns a runtime clone with an object-side shared-anon view registered
    /// for the provided VMA metadata.
    pub fn register_object_view(
        &self,
        mm_id: u64,
        invalidate: InvalidateHandle,
        vma: &VmArea,
    ) -> Self {
        let object_start = vma.page_offset() * memaddr::PAGE_SIZE_4K as u64;
        Self {
            start: self.start,
            pages: self.pages.clone(),
            object: self.object.clone(),
            object_view: Some(self.object.register_view(MappingViewSpec {
                mm_id,
                vma_start: vma.start().as_usize() as u64,
                vma_len: vma.size(),
                object_start,
                object_len: vma.size(),
                kind: MappingViewKind::Shared,
                notifier: Some(SharedAnonInvalidate::new(invalidate)),
            })),
        }
    }

    pub fn clone_for_fork_runtime(
        &self,
        _range: VirtAddrRange,
        _flags: MappingFlags,
        _old_pgtbl: &mut PageTableMut,
        _new_pgtbl: &mut PageTableMut,
        _new_aspace: &Arc<Mutex<MmSpace>>,
        _invalidate: Option<InvalidateHandle>,
    ) -> KResult<Self> {
        Ok(Self {
            start: self.start,
            pages: self.pages.clone(),
            object: self.object.clone(),
            object_view: None,
        })
    }

    fn pages_starting_from(&self, start: VirtAddr) -> &[PhysAddr] {
        debug_assert!(start.is_aligned(self.pages.size));
        let start_index = divide_page(start - self.start, self.pages.size);
        &self.pages[start_index..]
    }

    fn page_for(&self, addr: VirtAddr) -> Option<PhysAddr> {
        self.pages_starting_from(addr).first().copied()
    }
}

struct SharedAnonInvalidate {
    handle: InvalidateHandle,
}

impl SharedAnonInvalidate {
    fn new(handle: InvalidateHandle) -> Arc<Self> {
        Arc::new(Self { handle })
    }
}

impl MappingViewNotifier for SharedAnonInvalidate {
    fn invalidate(&self, work: &ObjectInvalidateWork, hit: &ObjectViewHit) {
        let Some(request) = work.request_for_hit(hit) else {
            warn!(
                "Ignored anonymous shared hit {} not carried by its invalidate work",
                hit.view().id().raw()
            );
            return;
        };
        if let Err(err) = self.handle.submit(request) {
            warn!(
                "Failed to invalidate anonymous shared view {}: {err}",
                hit.view().id().raw()
            );
        }
    }
}

impl BackendOps for SharedBackend {
    fn page_size(&self) -> PageSize {
        self.pages.size
    }

    fn backing_info(&self) -> VmBackingInfo {
        VmBackingInfo::new(
            VmBackingKind::AnonymousShared {
                object: self.object.id(),
            },
            self.page_size(),
        )
    }

    fn map(&self, range: VirtAddrRange, flags: MappingFlags, pgtbl: &mut PageTableMut) -> KResult {
        debug!("Shared::map: {:?} {:?}", range, flags);
        for (vaddr, paddr) in
            pages_in(range, self.pages.size)?.zip(self.pages_starting_from(range.start))
        {
            pgtbl
                .map(vaddr, *paddr, self.pages.size, flags)
                .map_err(map_paging_err)?;
        }
        Ok(())
    }

    fn unmap(&self, range: VirtAddrRange, pgtbl: &mut PageTableMut) -> KResult {
        debug!("Shared::unmap: {:?}", range);
        for vaddr in pages_in(range, self.pages.size)? {
            match pgtbl.unmap(vaddr) {
                Ok(_) | Err(PagingError::NotMapped) => {}
                Err(err) => return Err(map_paging_err(err)),
            }
        }
        Ok(())
    }

    fn handle_fault(
        &self,
        ctx: FaultContext,
        flags: MappingFlags,
        pgtbl: &mut PageTableMut,
    ) -> FaultCompletionResult {
        let _ = self.registration_id();
        let addr = ctx.address().align_down(self.page_size());
        let Some(frame) = self.page_for(addr) else {
            return Err(KError::BadAddress);
        };
        match pgtbl.query(addr) {
            Ok((paddr, page_flags, _)) => {
                if paddr != frame {
                    return Err(KError::BadAddress);
                }
                if !page_flags.contains(ctx.access_flags()) {
                    pgtbl.protect(addr, flags).map_err(map_paging_err)?;
                }
            }
            Err(PagingError::NotMapped) => {
                pgtbl
                    .map(addr, frame, self.page_size(), flags)
                    .map_err(map_paging_err)?;
            }
            Err(_) => return Err(KError::BadAddress),
        }
        Ok(FaultCompletion::from_populate((1, None)))
    }
}

impl VmRuntimeOps for SharedBackend {
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
        _range: VirtAddrRange,
        _flags: MappingFlags,
        _old_pgtbl: &mut PageTableMut,
        _new_pgtbl: &mut PageTableMut,
        _target: ForkCloneTarget<'_>,
    ) -> KResult<Arc<dyn VmRuntimeOps>> {
        Ok(Arc::new(self.clone()))
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::sync::Arc;

    use anon::AnonSharedObject;
    use khal::paging::PageSize;
    use memaddr::va;
    use unittest::def_test;

    use super::{SharedBackend, SharedPages};
    use crate::{VmBackingKind, backend::BackendOps};

    #[def_test]
    fn shared_backend_reuses_explicit_shared_owner_identity() {
        let object = AnonSharedObject::new();
        let pages =
            Arc::new(SharedPages::new(PageSize::Size4K as usize, PageSize::Size4K).unwrap());
        let left = SharedBackend::new(va!(0x1000), pages.clone(), object.clone());
        let right = SharedBackend::new(va!(0x2000), pages, object.clone());
        let VmBackingKind::AnonymousShared { object: left_id } = left.backing_info().kind() else {
            panic!("expected anonymous shared backing");
        };
        let VmBackingKind::AnonymousShared { object: right_id } = right.backing_info().kind()
        else {
            panic!("expected anonymous shared backing");
        };
        assert_eq!(left_id, object.id());
        assert_eq!(right_id, object.id());
    }
}

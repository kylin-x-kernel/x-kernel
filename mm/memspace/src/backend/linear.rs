// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linear mapping backend.
use alloc::sync::Arc;

use kerrno::{KError, KResult};
use khal::paging::{MappingFlags, PageSize, PageTableMut, PagingError};
use ksync::Mutex;
use memaddr::{MemoryAddr, PhysAddr, PhysAddrRange, VirtAddr, VirtAddrRange};

use crate::{
    FaultContext, ForkCloneTarget, InvalidateHandle, MmSpace, VmBackingInfo, VmBackingKind,
    backend::{BackendOps, FaultCompletion, FaultCompletionResult, map_paging_err, pages_in},
    vma::VmRuntimeOps,
};

/// Linear mapping backend.
///
/// The offset between the virtual address and the physical address is
/// constant, which is specified by `pa_va_offset`. For example, the virtual
/// address `vaddr` is mapped to the physical address `vaddr - pa_va_offset`.
#[derive(Clone)]
pub struct LinearBackend {
    offset: isize,
}

impl LinearBackend {
    pub fn new(offset: isize) -> Self {
        Self { offset }
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
        Ok(self.clone())
    }

    fn pa(&self, va: VirtAddr) -> PhysAddr {
        let pa = (va.as_usize() as isize)
            .checked_sub(self.offset)
            .expect("linear address translation overflow");
        assert!(pa >= 0, "linear address translation produced negative PA");
        PhysAddr::from(pa as usize)
    }
}

impl BackendOps for LinearBackend {
    fn page_size(&self) -> PageSize {
        PageSize::Size4K
    }

    fn backing_info(&self) -> VmBackingInfo {
        VmBackingInfo::new(VmBackingKind::Linear, self.page_size())
    }

    fn map(&self, range: VirtAddrRange, flags: MappingFlags, pgtbl: &mut PageTableMut) -> KResult {
        let pa_range = PhysAddrRange::from_start_size(self.pa(range.start), range.size());
        debug!("Linear::map: {range:?} -> {pa_range:?} {flags:?}");
        pgtbl
            .map_region(range.start, |va| self.pa(va), range.size(), flags, false)
            .map_err(map_paging_err)?;
        Ok(())
    }

    fn unmap(&self, range: VirtAddrRange, pgtbl: &mut PageTableMut) -> KResult {
        let pa_range = PhysAddrRange::from_start_size(self.pa(range.start), range.size());
        debug!("Linear::unmap: {range:?} -> {pa_range:?}");
        for vaddr in pages_in(range, PageSize::Size4K)? {
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
        let addr = ctx.address().align_down(self.page_size());
        let expected = self.pa(addr);
        match pgtbl.query(addr) {
            Ok((paddr, page_flags, _)) => {
                if paddr != expected {
                    return Err(KError::BadAddress);
                }
                if !page_flags.contains(ctx.access_flags()) {
                    pgtbl.protect(addr, flags).map_err(map_paging_err)?;
                }
            }
            Err(PagingError::NotMapped) => {
                pgtbl
                    .map(addr, expected, self.page_size(), flags)
                    .map_err(map_paging_err)?;
            }
            Err(_) => return Err(KError::BadAddress),
        }
        Ok(FaultCompletion::from_populate((1, None)))
    }
}

impl VmRuntimeOps for LinearBackend {
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
        _new_start: VirtAddr,
        _new_mm_id: u64,
        _aspace: &Arc<Mutex<MmSpace>>,
        _invalidate: Option<InvalidateHandle>,
    ) -> KResult<Arc<dyn VmRuntimeOps>> {
        Err(KError::OperationNotSupported)
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

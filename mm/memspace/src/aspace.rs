// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Address space implementation backed by VMA metadata and page tables.
use alloc::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Weak},
    vec::Vec,
};
#[cfg(all(feature = "smp", not(target_arch = "aarch64")))]
use core::ptr::NonNull;
use core::{
    fmt,
    ops::DerefMut,
    sync::atomic::{AtomicU64, Ordering},
};

/// Address resolution policy for mmap operations.
pub enum AddrPolicy {
    /// Find any suitable free area.
    Any,
    /// Use exact address, unmap existing ranges.
    Fixed,
    /// Use exact address, fail if overlaps existing mapping.
    FixedNoReplace,
}

use kerrno::{KError, KErrorKind, KResult, k_bail};
use khal::{
    mem::p2v,
    paging::{MappingFlags, PageSize, PageTable, PagingError},
    trap::PageFaultFlags,
};
use kspin::SpinNoIrq;
use ksync::Mutex;
use memaddr::{
    MemoryAddr, PAGE_SIZE_4K, PageIter4K, PhysAddr, VirtAddr, VirtAddrRange, is_aligned_4k,
};
use vmobj::{MappingViewKind, ObjectInvalidateRequest, VmObjectId};

#[cfg(target_arch = "aarch64")]
use crate::Aarch64UserAsidContext;
use crate::{
    FaultContext, FaultInput, ForkCloneTarget, MsyncPolicy, PageFaultOutcome, VmArea, VmAreaSet,
    VmBackingKind,
    backend::{FaultCompletionResult, map_paging_err, pages_in},
    cpu_residency::{MmCpuResidency, MmCpuResidencyRef},
    vma::VmRuntimeRef,
};

#[cfg(all(feature = "smp", not(target_arch = "aarch64")))]
unsafe fn page_table_user_cpu_mask(ctx: NonNull<()>) -> kcpu_id_map::KCpuMask {
    // SAFETY: `ctx` is installed from `Arc::as_ref(&cpu_residency)` in
    // `new_empty_inner()`. The `MmSpace` owns both the page table and the
    // `Arc<MmCpuResidency>`, and the page table is dropped before the
    // residency field, so the pointee remains valid for the provider's
    // lifetime.
    let residency = unsafe { ctx.cast::<MmCpuResidency>().as_ref() };
    residency.snapshot()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct PendingInvalidateId(u64);

#[derive(Default)]
struct PendingInvalidations {
    next_id: u64,
    order: VecDeque<PendingInvalidateId>,
    entries: BTreeMap<PendingInvalidateId, ObjectInvalidateRequest>,
}

static NEXT_MM_ID: AtomicU64 = AtomicU64::new(1);

impl PendingInvalidations {
    fn enqueue(&mut self, request: ObjectInvalidateRequest) -> PendingInvalidateId {
        let id = PendingInvalidateId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1);
        self.order.push_back(id);
        self.entries.insert(id, request);
        id
    }

    fn pop_front(&mut self) -> Option<(PendingInvalidateId, ObjectInvalidateRequest)> {
        loop {
            let id = self.order.pop_front()?;
            if let Some(request) = self.entries.remove(&id) {
                return Some((id, request));
            }
        }
    }

    fn remove(&mut self, id: PendingInvalidateId) -> bool {
        self.entries.remove(&id).is_some()
    }

    fn requeue_back(&mut self, id: PendingInvalidateId, request: ObjectInvalidateRequest) {
        self.order.push_back(id);
        self.entries.insert(id, request);
    }
}

#[derive(Default)]
pub struct InvalidateSink {
    queue: SpinNoIrq<PendingInvalidations>,
}

impl InvalidateSink {
    fn enqueue(&self, request: ObjectInvalidateRequest) -> PendingInvalidateId {
        self.queue.lock().enqueue(request)
    }

    fn pop_front(&self) -> Option<(PendingInvalidateId, ObjectInvalidateRequest)> {
        self.queue.lock().pop_front()
    }

    fn requeue_back(&self, id: PendingInvalidateId, request: ObjectInvalidateRequest) {
        self.queue.lock().requeue_back(id, request);
    }

    fn remove(&self, id: PendingInvalidateId) -> bool {
        self.queue.lock().remove(id)
    }
}

#[derive(Clone)]
pub struct InvalidateHandle {
    sink: Weak<InvalidateSink>,
    aspace: Weak<Mutex<MmSpace>>,
}

impl InvalidateHandle {
    pub fn enqueue(&self, request: ObjectInvalidateRequest) -> bool {
        let Some(sink) = self.sink.upgrade() else {
            return false;
        };
        sink.enqueue(request);
        true
    }

    pub fn try_apply(&self, request: &ObjectInvalidateRequest) -> KResult<bool> {
        let Some(aspace) = self.aspace.upgrade() else {
            return Ok(false);
        };
        let Some(mut aspace) = aspace.try_lock() else {
            return Ok(false);
        };
        aspace.apply_invalidate_request(request)?;
        Ok(true)
    }

    pub fn submit(&self, request: ObjectInvalidateRequest) -> KResult<bool> {
        let Some(sink) = self.sink.upgrade() else {
            return Ok(false);
        };
        let id = sink.enqueue(request.clone());
        match self.try_apply(&request) {
            Ok(true) => {
                let _ = sink.remove(id);
                Ok(true)
            }
            Ok(false) => Ok(false),
            Err(err) => Err(err),
        }
    }
}

/// Validated source mapping for `mremap()`.
///
/// Syscall code can read only the source
/// VMA metadata needed to relocate a mapping, while the runtime execution
/// handle remains encapsulated inside `memspace`.
pub struct MremapSource {
    vma: VmArea,
}

impl MremapSource {
    /// Returns the source mapping's page size.
    pub fn page_size(&self) -> PageSize {
        self.vma.backing().page_size()
    }

    /// Returns the source mapping's protection flags.
    pub fn flags(&self) -> MappingFlags {
        self.vma.flags()
    }

    /// Returns the source mapping's maximum protection flags.
    pub fn max_flags(&self) -> MappingFlags {
        self.vma.max_flags()
    }

    /// Returns the end address of the source VMA.
    pub fn end(&self) -> VirtAddr {
        self.vma.end()
    }
}
/// The virtual memory address space.
pub struct MmSpace {
    mm_id: u64,
    range: VirtAddrRange,
    vmas: VmAreaSet,
    pgtbl: PageTable,
    cpu_residency: MmCpuResidencyRef,
    #[cfg(target_arch = "aarch64")]
    user_asid_context: Option<Arc<Aarch64UserAsidContext>>,
    invalidate_sink: Arc<InvalidateSink>,
}

/// Compatibility alias retained while external crates migrate off the legacy name.
pub type AddrSpace = MmSpace;

impl MmSpace {
    pub fn invalidate_handle(&self, aspace: &Arc<Mutex<MmSpace>>) -> InvalidateHandle {
        InvalidateHandle {
            sink: Arc::downgrade(&self.invalidate_sink),
            aspace: Arc::downgrade(aspace),
        }
    }

    pub(crate) fn deferred_invalidate_handle(&self) -> InvalidateHandle {
        InvalidateHandle {
            sink: Arc::downgrade(&self.invalidate_sink),
            aspace: Weak::new(),
        }
    }

    pub fn enqueue_invalidate(&self, request: ObjectInvalidateRequest) {
        self.invalidate_sink.enqueue(request);
    }

    fn invalidate_kind_matches(
        vma_kind: VmBackingKind,
        request_kind: MappingViewKind,
        object: VmObjectId,
    ) -> bool {
        match (vma_kind, request_kind) {
            (VmBackingKind::FileShared { object: left }, MappingViewKind::Shared) => left == object,
            (VmBackingKind::AnonymousShared { object: left }, MappingViewKind::Shared) => {
                left == object
            }
            (
                VmBackingKind::FilePrivate {
                    file_object,
                    anon_object,
                    ..
                },
                MappingViewKind::Private,
            ) => match object {
                VmObjectId::File(_) => file_object == object,
                VmObjectId::Anon(_) => anon_object == object,
            },
            (VmBackingKind::AnonymousPrivate { object: left }, MappingViewKind::Private) => {
                left == object
            }
            _ => false,
        }
    }

    fn request_offset_for(vma: &VmArea, object: VmObjectId, addr: VirtAddr) -> Option<u64> {
        match (vma.backing().kind(), object) {
            (VmBackingKind::FileShared { object: left }, object)
            | (VmBackingKind::AnonymousShared { object: left }, object)
            | (VmBackingKind::AnonymousPrivate { object: left }, object)
                if left == object =>
            {
                vma.backing_offset_for(addr)
            }
            (VmBackingKind::FilePrivate { file_object, .. }, object @ VmObjectId::File(_))
                if file_object == object =>
            {
                vma.file_offset_for(addr)
            }
            (VmBackingKind::FilePrivate { anon_object, .. }, object @ VmObjectId::Anon(_))
                if anon_object == object =>
            {
                Some(addr.as_usize().saturating_sub(vma.start().as_usize()) as u64)
            }
            _ => None,
        }
    }

    fn overlap_matches_invalidate(
        &self,
        vma: &VmArea,
        overlap: VirtAddrRange,
        request: &ObjectInvalidateRequest,
    ) -> bool {
        let hit = request.hit();
        if !Self::invalidate_kind_matches(vma.backing().kind(), hit.view().kind(), request.object())
        {
            return false;
        }
        let virt_delta = overlap
            .start
            .as_usize()
            .saturating_sub(hit.vma_start() as usize) as u64;
        let expected_offset = hit.object_start() + virt_delta;
        Self::request_offset_for(vma, request.object(), overlap.start) == Some(expected_offset)
    }

    fn zap_present_ranges(&mut self, ranges: Vec<(VirtAddrRange, PageSize)>) -> KResult<()> {
        let mut modify = self.pgtbl.modify();
        let mut result = Ok(());
        'outer: for (range, page_size) in ranges {
            let iter = match pages_in(range, page_size) {
                Ok(iter) => iter,
                Err(err) => {
                    result = Err(err);
                    break;
                }
            };
            for vaddr in iter {
                match modify.unmap(vaddr) {
                    Ok(_) | Err(PagingError::NotMapped) => {}
                    Err(err) => {
                        result = Err(map_paging_err(err));
                        break 'outer;
                    }
                }
            }
        }
        modify.finish();
        result
    }

    fn apply_invalidate_request(&mut self, request: &ObjectInvalidateRequest) -> KResult<()> {
        let hit = request.hit();
        if hit.vma_len() == 0 {
            return Ok(());
        }
        let range = VirtAddrRange::from_start_size(
            VirtAddr::from_usize(hit.vma_start() as usize),
            hit.vma_len(),
        );

        let overlaps = self
            .vmas
            .collect_overlapping(range)
            .into_iter()
            .filter_map(|vma| {
                let overlap = VirtAddrRange::new(
                    VirtAddr::from_usize(hit.vma_start() as usize).max(vma.start()),
                    range.end.min(vma.end()),
                );
                if overlap.is_empty() || !self.overlap_matches_invalidate(&vma, overlap, request) {
                    return None;
                }
                Some((overlap, vma.backing().page_size()))
            })
            .collect::<Vec<_>>();

        self.zap_present_ranges(overlaps)
    }

    fn drain_pending_invalidations(&mut self) {
        while let Some((id, request)) = self.invalidate_sink.pop_front() {
            if let Err(err) = self.apply_invalidate_request(&request) {
                warn!(
                    "Failed to apply invalidate request view={} at 0x{:x} (len={}): {err}",
                    request.hit().view().id().raw(),
                    request.hit().vma_start(),
                    request.hit().vma_len()
                );
                self.invalidate_sink.requeue_back(id, request);
                break;
            }
        }
    }

    pub fn submit_invalidate_locked(&mut self, request: ObjectInvalidateRequest) -> KResult<()> {
        let id = self.invalidate_sink.enqueue(request.clone());
        match self.apply_invalidate_request(&request) {
            Ok(()) => {
                let _ = self.invalidate_sink.remove(id);
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    fn kernel_linear_max_flags(flags: MappingFlags) -> MappingFlags {
        flags
            | MappingFlags::READ
            | MappingFlags::WRITE
            | MappingFlags::EXECUTE
            | MappingFlags::DEVICE
            | MappingFlags::UNCACHED
            | MappingFlags::SHARED
    }

    fn page_fault_outcome_to_error(outcome: PageFaultOutcome) -> KResult<()> {
        match outcome {
            PageFaultOutcome::Resolved => Ok(()),
            PageFaultOutcome::Retry | PageFaultOutcome::CowConflictRetry => {
                Err(KError::ResourceBusy)
            }
            PageFaultOutcome::Unmapped | PageFaultOutcome::BusError => Err(KError::BadAddress),
            PageFaultOutcome::AccessDenied => Err(KError::PermissionDenied),
            PageFaultOutcome::OutOfMemory | PageFaultOutcome::NoProgress => Err(KError::NoMemory),
            PageFaultOutcome::Failed => Err(KError::BadAddress),
        }
    }

    fn page_fault_outcome_is_handled(outcome: PageFaultOutcome) -> bool {
        outcome.is_resolved() || outcome.is_retryable()
    }

    fn install_runtime_mapping(&mut self, mut vma: VmArea, runtime: VmRuntimeRef) -> KResult {
        runtime.map(vma.range(), vma.flags(), &mut self.pgtbl.modify())?;
        vma.set_runtime(runtime.register_object_views(
            self.mm_id,
            self.deferred_invalidate_handle(),
            &vma,
        ));
        self.vmas.insert(vma);
        Ok(())
    }

    fn remove_mapping_range(&mut self, start: VirtAddr, size: usize) -> KResult {
        let range = VirtAddrRange::from_start_size(start, size);
        let overlapped_vmas = self.vmas.collect_overlapping(range);
        let operations = overlapped_vmas
            .iter()
            .map(|vma| {
                let overlap = VirtAddrRange::new(start.max(vma.start()), range.end.min(vma.end()));
                let runtime = vma.runtime().cloned().ok_or(KError::BadAddress)?;
                Ok((overlap, runtime))
            })
            .collect::<KResult<Vec<_>>>()?;
        {
            let mut modify = self.pgtbl.modify();
            for (overlap, runtime) in &operations {
                runtime.unmap(*overlap, &mut modify)?;
            }
            modify.finish();
        }
        self.vmas.unmap(start, size);
        Ok(())
    }

    fn remove_mapping_metadata(&mut self, start: VirtAddr, size: usize) -> KResult {
        self.validate_region(start, size)?;
        self.vmas.unmap(start, size);
        Ok(())
    }

    fn extend_mapping_range(&mut self, start: VirtAddr, additional: usize) -> KResult {
        let vma = self.vmas.find(start).ok_or(KError::NoMemory)?;
        let runtime = vma.runtime().cloned().ok_or(KError::BadAddress)?;
        let current_end = vma.end();
        let new_end = current_end
            .checked_add(additional)
            .ok_or(KError::InvalidInput)?;
        if self.vmas.overlaps(VirtAddrRange::new(current_end, new_end)) {
            return Err(KError::AlreadyExists);
        }
        runtime.map(
            VirtAddrRange::from_start_size(current_end, additional),
            vma.flags(),
            &mut self.pgtbl.modify(),
        )?;
        self.vmas.extend(start, additional);
        Ok(())
    }

    fn protect_mapping_range(
        &mut self,
        start: VirtAddr,
        size: usize,
        flags: MappingFlags,
    ) -> KResult {
        let range = VirtAddrRange::from_start_size(start, size);
        let overlapped_vmas = self.vmas.collect_overlapping(range);
        let operations = overlapped_vmas
            .iter()
            .map(|vma| {
                let protect_range =
                    VirtAddrRange::new(start.max(vma.start()), range.end.min(vma.end()));
                let runtime = vma.runtime().cloned().ok_or(KError::BadAddress)?;
                Ok((protect_range, runtime))
            })
            .collect::<KResult<Vec<_>>>()?;
        {
            let mut modify = self.pgtbl.modify();
            for (protect_range, runtime) in &operations {
                let pte_flags = runtime.on_protect(*protect_range, flags, &mut modify)?;
                modify
                    .protect_region(protect_range.start, protect_range.size(), pte_flags)
                    .map_err(crate::backend::map_paging_err)?;
            }
            modify.finish();
        }
        self.vmas.protect(start, size, flags);
        Ok(())
    }

    fn clear_all_mappings(&mut self) {
        let vmas = self.vmas.iter().cloned().collect::<Vec<_>>();
        let operations = vmas
            .iter()
            .map(|vma| {
                let runtime = vma.runtime().cloned().ok_or(KError::BadAddress)?;
                Ok((vma.range(), runtime))
            })
            .collect::<KResult<Vec<_>>>()
            .expect("runtime entry must exist for every VMA");
        {
            let mut modify = self.pgtbl.modify();
            for (range, runtime) in &operations {
                runtime
                    .unmap(*range, &mut modify)
                    .expect("backend clear must succeed");
            }
            modify.finish();
        }
        self.vmas.clear();
    }

    /// Returns the address space base.
    pub const fn base(&self) -> VirtAddr {
        self.range.start
    }

    /// Returns the address space end.
    pub const fn end(&self) -> VirtAddr {
        self.range.end
    }

    /// Returns the address space size.
    pub fn size(&self) -> usize {
        self.range.size()
    }

    /// Returns the reference to the inner page table.
    pub const fn page_table(&self) -> &PageTable {
        &self.pgtbl
    }

    /// Returns a mutable reference to the inner page table.
    pub const fn page_table_mut(&mut self) -> &mut PageTable {
        &mut self.pgtbl
    }

    /// Returns the root physical address of the inner page table.
    pub const fn page_table_root(&self) -> PhysAddr {
        self.pgtbl.root_paddr()
    }

    /// Returns the hardware page-table root value for user context switches.
    pub fn page_table_hw_root(&self) -> karch::HwPageTableRoot {
        #[cfg(target_arch = "aarch64")]
        {
            self.user_asid_context
                .as_ref()
                .map_or_else(|| self.page_table_root().into(), |ctx| ctx.hardware_root())
        }
        #[cfg(not(target_arch = "aarch64"))]
        {
            self.page_table_root().into()
        }
    }

    /// Returns the stable address-space identity used by object-side rmap
    /// registrations.
    pub const fn mm_id(&self) -> u64 {
        self.mm_id
    }

    /// Checks if the address space contains the given address range.
    pub fn contains_range(&self, start: VirtAddr, size: usize) -> bool {
        self.range.contains(start) && (self.range.end - start) >= size
    }

    /// Creates a new empty user address space.
    ///
    /// The returned address space uses a user page table; TLB shootdowns for
    /// page table modifications are scoped to this address space's CPU
    /// residency mask.
    pub fn new_empty_user(base: VirtAddr, size: usize) -> KResult<Self> {
        Self::new_empty_inner(base, size, false)
    }

    /// Creates a new empty kernel address space.
    ///
    /// The returned address space uses a kernel page table; TLB shootdowns for
    /// page table modifications are broadcast to **all** online CPUs.
    pub fn new_empty_kernel(base: VirtAddr, size: usize) -> KResult<Self> {
        Self::new_empty_inner(base, size, true)
    }

    /// Internal helper: creates a new empty address space.
    ///
    /// Set `is_kernel` to `true` for the global kernel address space so that
    /// page table modifications flush TLB entries on **all** online CPUs.
    fn new_empty_inner(base: VirtAddr, size: usize, is_kernel: bool) -> KResult<Self> {
        let cpu_residency = Arc::new(MmCpuResidency::new());
        #[cfg(target_arch = "aarch64")]
        let pgtbl = {
            if is_kernel {
                PageTable::try_new_kernel().map_err(|_| KError::NoMemory)?
            } else {
                PageTable::try_new().map_err(|_| KError::NoMemory)?
            }
        };
        #[cfg(not(target_arch = "aarch64"))]
        let pgtbl = if is_kernel {
            PageTable::try_new_kernel().map_err(|_| KError::NoMemory)?
        } else {
            PageTable::try_new().map_err(|_| KError::NoMemory)?
        };
        #[cfg(target_arch = "aarch64")]
        let (pgtbl, user_asid_context) = if is_kernel {
            (pgtbl, None)
        } else {
            let mut pgtbl = pgtbl;
            let ctx = Arc::new(Aarch64UserAsidContext::new(pgtbl.root_paddr()));
            ctx.install_page_table_asid_provider(&mut pgtbl);
            (pgtbl, Some(ctx))
        };
        #[cfg(all(feature = "smp", not(target_arch = "aarch64")))]
        let pgtbl = if is_kernel {
            pgtbl
        } else {
            let mut pgtbl = pgtbl;
            let ctx = NonNull::from(Arc::as_ref(&cpu_residency)).cast();
            // SAFETY: the provider snapshots the `MmCpuResidency` owned by
            // this `MmSpace`. That residency allocation stays alive for the
            // full lifetime of the page table because both are fields of the
            // same `MmSpace`, and `MmSpace` drops the page table before the
            // residency handle.
            unsafe { pgtbl.set_user_cpu_mask_provider(ctx, page_table_user_cpu_mask) };
            pgtbl
        };
        Ok(Self {
            mm_id: NEXT_MM_ID.fetch_add(1, Ordering::Relaxed),
            range: VirtAddrRange::from_start_size(base, size),
            vmas: VmAreaSet::new(),
            pgtbl,
            cpu_residency,
            #[cfg(target_arch = "aarch64")]
            user_asid_context,
            invalidate_sink: Arc::new(InvalidateSink::default()),
        })
    }

    #[cfg(target_arch = "aarch64")]
    pub fn user_asid_context(&self) -> Option<&Arc<Aarch64UserAsidContext>> {
        self.user_asid_context.as_ref()
    }

    /// Returns the mm-owned CPU residency state used by non-AArch64 user TLB
    /// shootdown targeting.
    pub fn cpu_residency(&self) -> &MmCpuResidencyRef {
        &self.cpu_residency
    }

    /// Creates a new empty user address space with the standard user-space range.
    pub fn new_user_empty() -> KResult<Self> {
        let mut aspace = Self::new_empty_user(
            VirtAddr::from_usize(kaddr_layout::USER_SPACE_BASE),
            kaddr_layout::USER_SPACE_SIZE,
        )?;
        aspace.copy_kernel_mappings()?;
        Ok(aspace)
    }

    /// Copies page table mappings from another address space.
    ///
    /// It copies the page table entries only rather than the memory regions,
    /// usually used to copy a portion of the kernel space mapping to the
    /// user space.
    ///
    /// Returns an error if the two address spaces overlap.
    pub fn copy_mappings_from(&mut self, other: &MmSpace) -> KResult {
        self.pgtbl
            .modify()
            .copy_from(&other.pgtbl, other.base(), other.size());
        Ok(())
    }

    /// If the target architecture requires it, copies the kernel portion
    /// of the address space to this address space.
    ///
    /// On aarch64 and loongarch64, user space uses separate page tables
    /// (TTBR0_EL1 / PGDL), so no copy is needed. On other architectures
    /// the kernel mappings are shared by copying page table entries.
    fn copy_kernel_mappings(&mut self) -> KResult {
        #[cfg(not(any(target_arch = "aarch64", target_arch = "loongarch64")))]
        {
            self.copy_mappings_from(&crate::kernel_layout().lock())?;
        }
        Ok(())
    }

    fn validate_region(&self, start: VirtAddr, size: usize) -> KResult {
        if !self.contains_range(start, size) {
            k_bail!(NoMemory, "address out of range");
        }
        if !start.is_aligned_4k() || !is_aligned_4k(size) {
            k_bail!(InvalidInput, "address is not aligned");
        }
        Ok(())
    }

    /// Finds a free area that can accommodate the given size.
    ///
    /// The search starts from the given hint address, and the area should be
    /// within the given limit range.
    ///
    /// Returns the start address of the free area. Returns None if no such area
    /// is found.
    pub fn find_free_area(
        &self,
        hint: VirtAddr,
        size: usize,
        limit: VirtAddrRange,
        align: usize,
    ) -> Option<VirtAddr> {
        self.vmas.find_free_area(hint, size, limit, align)
    }

    /// Finds the VMA containing `vaddr` and returns a Linux-aligned view.
    pub fn find_vma(&self, vaddr: VirtAddr) -> Option<&VmArea> {
        self.vmas.find(vaddr)
    }

    fn runtime_for_vma(&self, vma: &VmArea) -> Option<VmRuntimeRef> {
        let runtime = vma.runtime()?.clone();
        if runtime.backing_info() != vma.backing() {
            warn!(
                "runtime ref/VMA drift at {:?}: backend={:?} vma={:?}",
                vma.start(),
                runtime.backing_info(),
                vma.backing()
            );
            return None;
        }
        Some(runtime)
    }

    fn clone_vma_for_runtime(&self, vma: &VmArea) -> Option<VmArea> {
        self.runtime_for_vma(vma)?;
        Some(vma.clone())
    }

    fn clone_vmas<'a>(&self, vmas: impl IntoIterator<Item = &'a VmArea>) -> KResult<Vec<VmArea>> {
        vmas.into_iter()
            .map(|vma| {
                self.clone_vma_for_runtime(vma).ok_or_else(|| {
                    warn!("could not clone VMA metadata at {:?}", vma.start());
                    KError::BadAddress
                })
            })
            .collect()
    }

    fn covering_vmas_in_range(&self, range: VirtAddrRange) -> KResult<Vec<VmArea>> {
        if range.is_empty() {
            return Ok(Vec::new());
        }
        if !self.range.contains(range.start) || self.range.end < range.end {
            return Err(KError::NoMemory);
        }
        let overlapped_vmas = self.vmas.collect_overlapping(range);
        let mut cursor = range.start;
        for vma in &overlapped_vmas {
            if vma.start() > cursor {
                return Err(KError::NoMemory);
            }
            cursor = vma.end();
            if cursor >= range.end {
                return Ok(overlapped_vmas);
            }
        }
        Err(KError::NoMemory)
    }

    fn snapshot_vmas_in_range(
        &self,
        start: VirtAddr,
        size: usize,
    ) -> KResult<Vec<(VmArea, VirtAddrRange)>> {
        self.validate_region(start, size)?;
        let end = start + size;
        let mut overlaps = Vec::new();
        let overlapped_vmas = self.covering_vmas_in_range(VirtAddrRange::new(start, end))?;
        let mut cursor = start;
        for vma in &overlapped_vmas {
            overlaps.push(VirtAddrRange::new(
                cursor.max(vma.start()),
                vma.end().min(end),
            ));
            cursor = vma.end();
        }

        let vmas = self.clone_vmas(overlapped_vmas.iter())?;
        Ok(vmas.into_iter().zip(overlaps).collect())
    }

    fn clone_all_vmas(&self) -> KResult<Vec<VmArea>> {
        self.clone_vmas(self.vmas.iter())
    }

    fn relocated_mapping_from_snapshot(
        &self,
        vma: &VmArea,
        new_start: VirtAddr,
        new_size: usize,
        new_flags: MappingFlags,
        aspace: &Arc<Mutex<MmSpace>>,
    ) -> KResult<(VmArea, VmRuntimeRef)> {
        let runtime = self.runtime_for_vma(vma).ok_or(KError::BadAddress)?;
        let invalidate = Some(self.invalidate_handle(aspace));
        Ok((
            vma.relocated(new_start, new_size, new_flags),
            runtime.relocate_for_mremap(new_start, self.mm_id(), aspace, invalidate)?,
        ))
    }

    /// Resolves and validates the source mapping for an `mremap()` request.
    ///
    /// The returned snapshot is anchored at the source VMA start and guarantees
    /// that `old_size` fits within the VMA and respects the backing page size.
    pub fn resolve_mremap_source(&self, addr: VirtAddr, old_size: usize) -> KResult<MremapSource> {
        let vma = self.find_vma(addr).ok_or(KError::BadAddress)?;
        let vma = self.clone_vma_for_runtime(vma).ok_or(KError::BadAddress)?;
        if addr != vma.start() {
            return Err(KError::InvalidInput);
        }
        if addr.as_usize() + old_size > vma.end().as_usize() {
            return Err(KError::BadAddress);
        }
        if !vma.backing().page_size().is_aligned(addr.as_usize()) {
            return Err(KError::InvalidInput);
        }
        Ok(MremapSource { vma })
    }

    /// Finds a destination range suitable for relocating a mapping.
    ///
    /// The search first honors the provided hint, then falls back to the
    /// address-space base, matching current `mremap()` relocation policy.
    pub fn find_relocation_target(
        &self,
        hint: VirtAddr,
        size: usize,
        page_size: PageSize,
    ) -> KResult<VirtAddr> {
        let limit = VirtAddrRange::new(self.base(), self.end());
        self.find_free_area(hint, size, limit, page_size as usize)
            .or(self.find_free_area(self.base(), size, limit, page_size as usize))
            .ok_or(KError::NoMemory)
    }

    /// Resolve the mapping address for an mmap operation.
    ///
    /// For `Fixed`, validates the address and unmaps existing ranges.
    /// For `FixedNoReplace`, returns `EEXIST` if the range overlaps an existing
    /// mapping. For `Any`, finds a suitable free area.
    pub fn mmap_resolve_addr(
        &mut self,
        hint: VirtAddr,
        length: usize,
        page_size: usize,
        policy: AddrPolicy,
    ) -> KResult<VirtAddr> {
        match policy {
            AddrPolicy::Any => {
                let limit = VirtAddrRange::new(self.base(), self.end());
                self.find_free_area(hint, length, limit, page_size)
                    .or(self.find_free_area(self.base(), length, limit, page_size))
                    .ok_or(KError::NoMemory)
            }
            AddrPolicy::Fixed => {
                self.unmap(hint, length)?;
                Ok(hint)
            }
            AddrPolicy::FixedNoReplace => {
                let range = VirtAddrRange::from_start_size(hint, length);
                if self.vmas.overlaps(range) {
                    k_bail!(AlreadyExists, "mapping overlaps existing area");
                }
                Ok(hint)
            }
        }
    }

    /// Add a new linear mapping.
    ///
    /// The mapping is backed by a linear [`VmRuntimeRef`].
    ///
    /// The `flags` parameter indicates the mapping permissions and attributes.
    ///
    /// Returns an error if the address range is out of the address space or not
    /// aligned.
    pub fn map_linear(
        &mut self,
        start_vaddr: VirtAddr,
        start_paddr: PhysAddr,
        size: usize,
        flags: MappingFlags,
    ) -> KResult {
        self.drain_pending_invalidations();
        self.validate_region(start_vaddr, size)?;

        if !start_paddr.is_aligned_4k() {
            k_bail!(InvalidInput, "address is not aligned");
        }

        let offset = start_vaddr.as_usize() as isize - start_paddr.as_usize() as isize;
        let runtime = VmRuntimeRef::new_linear(offset);
        let vma = VmArea::new(
            start_vaddr,
            size,
            flags,
            Self::kernel_linear_max_flags(flags),
            runtime.backing_info(),
            0,
            None,
        );
        self.map_runtime_vma(vma, false, runtime)
    }

    /// Map a region using the provided runtime execution reference and flags.
    pub fn map(
        &mut self,
        start: VirtAddr,
        size: usize,
        flags: MappingFlags,
        populate: bool,
        runtime: VmRuntimeRef,
    ) -> KResult {
        self.map_with_max_flags(start, size, flags, flags, populate, runtime)
    }

    /// Map a region using the provided runtime reference and explicit maximum
    /// permissions.
    pub fn map_with_max_flags(
        &mut self,
        start: VirtAddr,
        size: usize,
        flags: MappingFlags,
        max_flags: MappingFlags,
        populate: bool,
        runtime: VmRuntimeRef,
    ) -> KResult {
        self.drain_pending_invalidations();
        self.validate_region(start, size)?;

        if matches!(
            runtime.backing_info().kind(),
            VmBackingKind::FileShared { .. } | VmBackingKind::FilePrivate { .. }
        ) {
            warn!(
                "file-backed mappings must use MmSpace::map_runtime_vma with explicit VmArea \
                 metadata"
            );
            return Err(KError::InvalidInput);
        }

        let vma = VmArea::new(
            start,
            size,
            flags,
            max_flags,
            runtime.backing_info(),
            0,
            None,
        );
        self.map_runtime_vma(vma, populate, runtime)
    }

    /// Map a region using an explicit VMA metadata record and runtime reference.
    pub fn map_runtime_vma(
        &mut self,
        vma: VmArea,
        populate: bool,
        runtime: VmRuntimeRef,
    ) -> KResult {
        self.drain_pending_invalidations();
        let start = vma.start();
        let size = vma.size();
        let flags = vma.flags();
        self.validate_region(start, size)?;
        self.install_runtime_mapping(vma, runtime)?;
        if populate {
            self.populate_area(start, size, flags)?;
        }
        Ok(())
    }

    /// Installs a relocated copy of an existing mapping snapshot.
    ///
    /// This keeps `mremap`-style relocation on the `MmSpace -> VmArea`
    /// construction path instead of open-coding `relocated VMA + relocated
    /// backend` assembly in upper layers.
    pub fn map_relocated_snapshot(
        &mut self,
        snapshot: &MremapSource,
        new_start: VirtAddr,
        new_size: usize,
        new_flags: MappingFlags,
        owner: &Arc<Mutex<MmSpace>>,
    ) -> KResult {
        self.drain_pending_invalidations();
        let (vma, runtime) = self.relocated_mapping_from_snapshot(
            &snapshot.vma,
            new_start,
            new_size,
            new_flags,
            owner,
        )?;
        self.install_runtime_mapping(vma, runtime)
    }

    /// Populates the area with physical frames, returning false if the area
    /// contains unmapped area.
    pub fn populate_area(
        &mut self,
        start: VirtAddr,
        size: usize,
        access_flags: MappingFlags,
    ) -> KResult {
        self.drain_pending_invalidations();
        self.validate_region(start, size)?;
        let end = start + size;
        let overlapped_vmas = self.covering_vmas_in_range(VirtAddrRange::new(start, end))?;
        let mut cursor = start;

        for vma in overlapped_vmas {
            let range = VirtAddrRange::new(cursor.max(vma.start()), vma.end().min(end));
            for vaddr in pages_in(range, vma.backing().page_size())? {
                let outcome = vma.handle_fault(self, FaultContext::new(vaddr, access_flags));
                Self::page_fault_outcome_to_error(outcome)?;
            }
            cursor = vma.end();
            assert!(cursor.is_aligned_4k());
            if cursor >= end {
                break;
            }
        }

        Ok(())
    }

    /// Removes mappings within the specified virtual address range.
    ///
    /// Returns an error if the address range is out of the address space or not
    /// aligned.
    pub fn unmap(&mut self, start: VirtAddr, size: usize) -> KResult {
        self.drain_pending_invalidations();
        self.validate_region(start, size)?;
        self.remove_mapping_range(start, size)
    }

    /// Removes VMA metadata for a range whose PTEs and backing ownership have
    /// already been transferred elsewhere.
    pub fn drop_mapping_metadata(&mut self, start: VirtAddr, size: usize) -> KResult {
        self.drain_pending_invalidations();
        self.remove_mapping_metadata(start, size)
    }

    /// Drops present PTEs in the specified range while keeping VMA metadata.
    ///
    /// The backing object zaps affected PTEs but the VMA remains so subsequent
    /// faults can re-evaluate current object state after truncate or invalidate.
    pub fn invalidate_present(&mut self, start: VirtAddr, size: usize) -> KResult {
        self.drain_pending_invalidations();
        let overlaps = self
            .snapshot_vmas_in_range(start, size)?
            .into_iter()
            .collect::<Vec<_>>();
        let operations = overlaps
            .iter()
            .map(|(vma, overlap)| Ok((*overlap, vma.backing().page_size())))
            .collect::<KResult<Vec<_>>>()?;

        self.zap_present_ranges(operations)
    }

    /// To process data in this area with the given function.
    ///
    /// Now it supports reading and writing data in the given interval.
    fn process_area_data<F>(&self, start: VirtAddr, size: usize, mut f: F) -> KResult
    where
        F: FnMut(VirtAddr, usize, usize),
    {
        if !self.contains_range(start, size) {
            k_bail!(InvalidInput, "address out of range");
        }
        let mut cnt = 0;
        // If start is aligned to 4K, start_align_down will be equal to start_align_up.
        let end_align_up = (start + size).align_up_4k();
        for vaddr in PageIter4K::new(start.align_down_4k(), end_align_up)
            .expect("Failed to create page iterator")
        {
            let (mut paddr, ..) = self.pgtbl.query(vaddr).map_err(|_| KError::BadAddress)?;

            let mut copy_size = (size - cnt).min(PAGE_SIZE_4K);

            if copy_size == 0 {
                break;
            }
            if vaddr == start.align_down_4k() && start.align_offset_4k() != 0 {
                let align_offset = start.align_offset_4k();
                copy_size = copy_size.min(PAGE_SIZE_4K - align_offset);
                paddr += align_offset;
            }
            f(p2v(paddr), cnt, copy_size);
            cnt += copy_size;
        }
        Ok(())
    }

    /// To read data from the address space.
    ///
    /// # Arguments
    ///
    /// * `start` - The start virtual address to read.
    /// * `buf` - The buffer to store the data.
    pub fn read(&self, start: VirtAddr, buf: &mut [u8]) -> KResult {
        self.process_area_data(start, buf.len(), |src, offset, read_size| {
            // SAFETY: `process_area_data` bounds-checks the source region, and
            // `offset..offset + read_size` lies within the destination slice.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    src.as_ptr(),
                    buf.as_mut_ptr().add(offset),
                    read_size,
                );
            }
        })
    }

    /// To write data to the address space.
    ///
    /// # Arguments
    ///
    /// * `start_vaddr` - The start virtual address to write.
    /// * `buf` - The buffer to write to the address space.
    pub fn write(&self, start: VirtAddr, buf: &[u8]) -> KResult {
        self.process_area_data(start, buf.len(), |dst, offset, write_size| {
            // SAFETY: `process_area_data` bounds-checks the destination region,
            // and `offset..offset + write_size` lies within the source slice.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    buf.as_ptr().add(offset),
                    dst.as_mut_ptr(),
                    write_size,
                );
            }
        })
    }

    /// Updates mapping within the specified virtual address range.
    ///
    /// Returns an error if the address range is out of the address space or not
    /// aligned.
    pub fn protect(&mut self, start: VirtAddr, size: usize, flags: MappingFlags) -> KResult {
        self.drain_pending_invalidations();
        self.validate_region(start, size)?;
        let overlapped_vmas =
            self.covering_vmas_in_range(VirtAddrRange::from_start_size(start, size))?;
        for vma in &overlapped_vmas {
            if !vma.allows_protection(flags) {
                return Err(KError::PermissionDenied);
            }
        }
        self.protect_mapping_range(start, size, flags)
    }

    /// Applies a `MADV_DONTNEED`-style discard hint through object-side
    /// producers for private-anon and file-private-anon mappings.
    pub fn madvise_dontneed(&mut self, start: VirtAddr, size: usize) -> KResult {
        self.drain_pending_invalidations();
        self.validate_region(start, size)?;
        let range = VirtAddrRange::from_start_size(start, size);
        let overlapped_vmas = self.covering_vmas_in_range(range)?;
        let mut handled = false;
        let mut pgtbl = self.pgtbl.modify();
        for vma in overlapped_vmas {
            let overlap = VirtAddrRange::new(start.max(vma.start()), range.end.min(vma.end()));
            if overlap.is_empty() {
                continue;
            }
            let Some(runtime) = vma.runtime() else {
                continue;
            };
            handled |= runtime.madvise_dontneed(&vma, overlap, &mut pgtbl)?;
        }
        pgtbl.finish();
        drop(pgtbl);
        if handled {
            self.drain_pending_invalidations();
        }
        Ok(())
    }

    /// Synchronizes file-backed shared mappings intersecting `start..start+size`.
    ///
    /// This mirrors Linux `msync()` range walking: holes are recorded as an
    /// address error, but later VMAs in the range are still processed unless a
    /// stronger runtime/provider error occurs.
    pub fn msync_range(&mut self, start: VirtAddr, size: usize, policy: MsyncPolicy) -> KResult {
        self.drain_pending_invalidations();
        self.validate_region(start, size)?;
        if size == 0 {
            return Ok(());
        }

        let range = VirtAddrRange::from_start_size(start, size);
        let overlapped_vmas = self.vmas.collect_overlapping(range);
        let mut cursor = start;
        let mut saw_hole = overlapped_vmas.is_empty();

        for vma in overlapped_vmas {
            if vma.start() > cursor {
                saw_hole = true;
            }
            let overlap = VirtAddrRange::new(cursor.max(vma.start()), range.end.min(vma.end()));
            if !overlap.is_empty() {
                // TODO: when mlock is implemented, check VM_LOCKED and
                // return EBUSY for MS_INVALIDATE as Linux does.
                let runtime = self.runtime_for_vma(&vma).ok_or(KError::BadAddress)?;
                runtime.msync(&vma, overlap, policy)?;
            }
            cursor = cursor.max(vma.end());
            if cursor >= range.end {
                break;
            }
        }

        if cursor < range.end {
            saw_hole = true;
        }
        if saw_hole {
            return Err(KError::NoMemory);
        }
        Ok(())
    }

    /// Extend an existing memory area in place.
    ///
    /// Returns an error if the extension would exceed address space bounds
    /// or overlap with another mapping.
    pub fn extend_area(&mut self, start: VirtAddr, additional: usize) -> KResult {
        self.drain_pending_invalidations();
        // Validate the extension range [area.end, area.end + additional),
        // not [start, start + additional).
        let current_end = self
            .find_vma(start)
            .map(|vma| vma.end())
            .ok_or(KError::NoMemory)?;
        if additional > 0 {
            self.validate_region(current_end, additional)?;
        }
        self.extend_mapping_range(start, additional)
    }

    /// Move page table entries from src to dst within the same address space.
    ///
    /// For each mapped page in `[src, src+size)`, queries the PTE, maps it at
    /// the corresponding dst address, and unmaps from src. Unmapped (lazy)
    /// pages are skipped and will be demand-paged at the new location.
    pub fn move_pages(
        &mut self,
        src: VirtAddr,
        dst: VirtAddr,
        size: usize,
        page_size: PageSize,
    ) -> KResult {
        let range = VirtAddrRange::from_start_size(src, size);
        let mut modify = self.pgtbl.modify();
        for vaddr in crate::backend::pages_in(range, page_size)? {
            let dst_vaddr = dst + (vaddr - src);
            if let Ok((paddr, flags, _)) = modify.query(vaddr) {
                modify
                    .map(dst_vaddr, paddr, page_size, flags)
                    .map_err(|_| KError::NoMemory)?;
                // Safe: we just confirmed the mapping exists via query(), and we
                // hold exclusive access to the page table through the modify guard.
                modify
                    .unmap(vaddr)
                    .expect("unmap must succeed after successful query");
            }
        }
        modify.finish();
        Ok(())
    }

    /// Removes all mappings in the address space.
    pub fn clear(&mut self) {
        self.drain_pending_invalidations();
        self.clear_all_mappings();
    }

    /// Checks whether an access to the specified memory region is valid.
    ///
    /// Returns `true` if the memory region given by `range` is all mapped and
    /// has proper permission flags (i.e. containing `access_flags`).
    pub fn can_access_range(
        &self,
        start: VirtAddr,
        size: usize,
        access_flags: MappingFlags,
    ) -> bool {
        let Some(range) = VirtAddrRange::try_from_start_size(start, size) else {
            return false;
        };
        let Ok(overlapped_vmas) = self.covering_vmas_in_range(range) else {
            return false;
        };
        for vma in overlapped_vmas {
            if !vma.flags().contains(access_flags) {
                return false;
            }
        }
        true
    }

    fn finish_backend_fault(
        &mut self,
        vaddr: VirtAddr,
        flags: MappingFlags,
        result: FaultCompletionResult,
    ) -> PageFaultOutcome {
        match result {
            Ok(mut completion) => {
                if let Some(cb) = completion.take_post_action() {
                    cb(self);
                }
                if completion.is_cow_conflict_retry() {
                    return PageFaultOutcome::CowConflictRetry;
                }
                if completion.populated() == 0 {
                    warn!("No pages populated for {vaddr:?} ({flags:?})");
                    PageFaultOutcome::NoProgress
                } else {
                    PageFaultOutcome::Resolved
                }
            }
            Err(err) => {
                if matches!(
                    KErrorKind::try_from(err.canonicalize()),
                    Ok(KErrorKind::NoMemory)
                ) {
                    PageFaultOutcome::OutOfMemory
                } else {
                    warn!("Failed to populate pages for {vaddr:?} ({flags:?}): {err}");
                    PageFaultOutcome::Failed
                }
            }
        }
    }

    fn finish_file_backend_fault(
        &mut self,
        vaddr: VirtAddr,
        flags: MappingFlags,
        result: FaultCompletionResult,
    ) -> PageFaultOutcome {
        match result {
            Ok(completion) => self.finish_backend_fault(vaddr, flags, Ok(completion)),
            Err(err) => match KErrorKind::try_from(err.canonicalize()) {
                Ok(KErrorKind::NoMemory) => PageFaultOutcome::OutOfMemory,
                Ok(
                    KErrorKind::BadAddress
                    | KErrorKind::InvalidData
                    | KErrorKind::InvalidInput
                    | KErrorKind::Io
                    | KErrorKind::UnexpectedEof,
                ) => PageFaultOutcome::BusError,
                _ => {
                    warn!("Failed to populate file-backed pages for {vaddr:?} ({flags:?}): {err}");
                    PageFaultOutcome::Failed
                }
            },
        }
    }

    fn fault_vma_runtime(&self, vma: &VmArea, _vaddr: VirtAddr) -> Option<(VmArea, VmRuntimeRef)> {
        let runtime = self.runtime_for_vma(vma)?;
        Some((vma.clone(), runtime))
    }

    fn materialize_fault_for_vma(
        &mut self,
        vma: VmArea,
        runtime: VmRuntimeRef,
        ctx: FaultContext,
    ) -> PageFaultOutcome {
        let vaddr = ctx.address();
        let result = runtime.handle_fault(ctx, vma.flags(), &mut self.pgtbl.modify());
        self.finish_backend_fault(vaddr, vma.flags(), result)
    }

    fn materialize_anonymous_fault_for_vma(
        &mut self,
        vma: VmArea,
        ctx: FaultContext,
    ) -> PageFaultOutcome {
        let vaddr = ctx.address();
        let Some((vma, runtime)) = self.fault_vma_runtime(&vma, vaddr) else {
            warn!("Backend fault path could not find area for {:?}", vaddr);
            return PageFaultOutcome::Unmapped;
        };
        self.materialize_fault_for_vma(vma, runtime, ctx)
    }

    fn materialize_file_fault_for_vma(
        &mut self,
        vma: VmArea,
        ctx: FaultContext,
    ) -> PageFaultOutcome {
        let vaddr = ctx.address();
        let Some((vma, runtime)) = self.fault_vma_runtime(&vma, vaddr) else {
            warn!("Backend fault path could not find area for {:?}", vaddr);
            return PageFaultOutcome::Unmapped;
        };
        let result = runtime.handle_fault(ctx, vma.flags(), &mut self.pgtbl.modify());
        self.finish_file_backend_fault(vaddr, vma.flags(), result)
    }

    pub(crate) fn handle_linear_fault(
        &mut self,
        vma: VmArea,
        ctx: FaultContext,
    ) -> PageFaultOutcome {
        self.materialize_anonymous_fault_for_vma(vma, ctx)
    }

    pub(crate) fn handle_anonymous_shared_fault(
        &mut self,
        vma: VmArea,
        ctx: FaultContext,
    ) -> PageFaultOutcome {
        self.materialize_anonymous_fault_for_vma(vma, ctx)
    }

    pub(crate) fn handle_anonymous_private_fault(
        &mut self,
        vma: VmArea,
        ctx: FaultContext,
    ) -> PageFaultOutcome {
        self.materialize_anonymous_fault_for_vma(vma, ctx)
    }

    pub(crate) fn handle_file_shared_fault(
        &mut self,
        vma: VmArea,
        ctx: FaultContext,
    ) -> PageFaultOutcome {
        self.materialize_file_fault_for_vma(vma, ctx)
    }

    pub(crate) fn handle_file_private_fault(
        &mut self,
        vma: VmArea,
        ctx: FaultContext,
    ) -> PageFaultOutcome {
        self.materialize_file_fault_for_vma(vma, ctx)
    }

    /// Handles a typed page-fault request.
    ///
    /// The dispatch consults explicit VMA metadata first and lets the VMA
    /// choose the fault policy before a runtime materializes pages.
    pub fn handle_fault_input(&mut self, input: FaultInput) -> PageFaultOutcome {
        self.drain_pending_invalidations();
        let vaddr = input.address();
        if !self.range.contains(vaddr) {
            return PageFaultOutcome::Unmapped;
        }
        let Some(vma) = self.find_vma(vaddr).cloned() else {
            return PageFaultOutcome::Unmapped;
        };
        let access_flags = input.access_flags();
        if !vma.allows_fault(access_flags) {
            return PageFaultOutcome::AccessDenied;
        }
        vma.handle_fault(self, input.into_context())
    }

    /// Compatibility wrapper for callers that still pass raw fault fields.
    pub fn handle_page_fault(
        &mut self,
        vaddr: VirtAddr,
        access_flags: PageFaultFlags,
    ) -> PageFaultOutcome {
        self.handle_fault_input(FaultInput::new(vaddr, access_flags))
    }

    /// Legacy bool-shaped page-fault hook retained for current trap callers.
    pub fn dispatch_irq_page_fault(
        &mut self,
        vaddr: VirtAddr,
        access_flags: PageFaultFlags,
    ) -> bool {
        let outcome = self.handle_page_fault(vaddr, access_flags);
        Self::page_fault_outcome_is_handled(outcome)
    }

    /// Attempts to clone the current address space into a new one.
    ///
    /// This method creates a new empty address space with the same base and
    /// size, then iterates over all memory areas in the original address
    /// space to copy or share their mappings into the new one.
    pub fn try_clone(&mut self) -> KResult<Arc<Mutex<Self>>> {
        self.drain_pending_invalidations();
        let new_aspace = Arc::new(Mutex::new(Self::new_empty_user(self.base(), self.size())?));
        let new_aspace_clone = new_aspace.clone();

        let mut guard = new_aspace.lock();
        guard.copy_kernel_mappings()?;

        let vmas = self.clone_all_vmas()?;
        let operations = vmas
            .iter()
            .map(|vma| {
                let runtime = self.runtime_for_vma(vma).ok_or(KError::BadAddress)?;
                Ok((vma.clone(), runtime))
            })
            .collect::<KResult<Vec<_>>>()?;
        let mut self_modify = self.pgtbl.modify();
        for (vma_meta, runtime) in &operations {
            let invalidate = Some(guard.invalidate_handle(&new_aspace_clone));
            let new_mm_id = guard.mm_id();
            let new_runtime = {
                let mut new_modify = guard.pgtbl.modify();
                runtime.clone_for_fork(
                    vma_meta.range(),
                    vma_meta.flags(),
                    &mut self_modify,
                    &mut new_modify,
                    ForkCloneTarget {
                        new_mm_id,
                        new_aspace: &new_aspace_clone,
                        invalidate,
                    },
                )?
            };
            let vma = vma_meta
                .relocated(vma_meta.start(), vma_meta.size(), vma_meta.flags())
                .with_backing(new_runtime.backing_info());

            let aspace = guard.deref_mut();
            aspace.install_runtime_mapping(vma, new_runtime)?;
        }
        drop(guard);

        Ok(new_aspace)
    }

    /// Returns VMA views with explicit backing descriptions.
    pub fn vmas(&self) -> impl Iterator<Item = &VmArea> {
        self.vmas.iter()
    }
}

impl fmt::Debug for MmSpace {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("MmSpace")
            .field("va_range", &self.range)
            .field("page_table_root", &self.pgtbl.root_paddr())
            .field("vmas", &self.vmas.iter().count())
            .finish()
    }
}

impl Drop for MmSpace {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::sync::Arc;

    use khal::{
        paging::{MappingFlags, PageSize},
        trap::PageFaultFlags,
    };
    use ksync::Mutex;
    use memaddr::{PAGE_SIZE_4K, VirtAddr};
    use unittest::def_test;
    use vmobj::{
        FileObjectId, MappingView, MappingViewId, MappingViewKind, MappingViewRange,
        ObjectInvalidateRequest, ObjectViewHit, VmObjectId,
    };

    use super::*;
    use crate::{PageFaultOutcome, vma::VmRuntimeRef};

    fn new_test_aspace() -> AddrSpace {
        AddrSpace::new_empty_kernel(VirtAddr::from(0x1000usize), PAGE_SIZE_4K * 64)
            .expect("test address space should be constructible")
    }

    fn map_shared(
        aspace: &mut AddrSpace,
        start: usize,
        size: usize,
        flags: MappingFlags,
    ) -> VmRuntimeRef {
        let start = VirtAddr::from(start);
        let backend = VmRuntimeRef::new_anon_shared(start, size, PageSize::Size4K)
            .expect("shared backend allocation should succeed");
        aspace
            .map(start, size, flags, false, backend.clone())
            .expect("mapping should succeed");
        backend
    }

    fn sample_request() -> ObjectInvalidateRequest {
        let view = MappingView::new(
            MappingViewId::from_raw(1),
            7,
            MappingViewRange {
                vma_start: 0x4000,
                vma_len: PAGE_SIZE_4K,
                object_start: 0,
                object_len: PAGE_SIZE_4K,
            },
            MappingViewKind::Shared,
        );
        ObjectInvalidateRequest::new(
            VmObjectId::File(FileObjectId::from_raw(11)),
            ObjectViewHit::new(view, 0, PAGE_SIZE_4K),
        )
    }

    #[def_test]
    fn invalidate_sink_removes_exact_enqueued_request() {
        let sink = InvalidateSink::default();
        let req = sample_request();

        let first = sink.enqueue(req.clone());
        let second = sink.enqueue(req.clone());

        assert!(sink.remove(second));

        let (remaining_id, remaining_req) = sink.pop_front().expect("first request must remain");
        assert_eq!(remaining_id, first);
        assert_eq!(remaining_req, req);
        assert!(sink.pop_front().is_none());
    }

    #[def_test]
    fn fault_adapter_treats_retry_outcomes_as_handled() {
        assert!(MmSpace::page_fault_outcome_is_handled(
            PageFaultOutcome::Resolved
        ));
        assert!(MmSpace::page_fault_outcome_is_handled(
            PageFaultOutcome::Retry
        ));
        assert!(MmSpace::page_fault_outcome_is_handled(
            PageFaultOutcome::CowConflictRetry
        ));
        assert!(!MmSpace::page_fault_outcome_is_handled(
            PageFaultOutcome::AccessDenied
        ));
    }

    #[def_test]
    fn madvise_dontneed_requires_fully_mapped_range() {
        let start = VirtAddr::from_usize(0x4000);
        let flags = MappingFlags::READ | MappingFlags::WRITE;
        let mut aspace =
            MmSpace::new_empty_user(start, PAGE_SIZE_4K * 4).expect("allocate test address space");
        aspace
            .map(
                start,
                PAGE_SIZE_4K,
                flags,
                false,
                VmRuntimeRef::new_anon_private(start, khal::paging::PageSize::Size4K),
            )
            .expect("map first page");
        aspace
            .map(
                start + PAGE_SIZE_4K * 2,
                PAGE_SIZE_4K,
                flags,
                false,
                VmRuntimeRef::new_anon_private(
                    start + PAGE_SIZE_4K * 2,
                    khal::paging::PageSize::Size4K,
                ),
            )
            .expect("map third page");
        assert!(
            aspace
                .handle_page_fault(start, PageFaultFlags::WRITE)
                .is_resolved()
        );

        assert!(
            aspace.madvise_dontneed(start, PAGE_SIZE_4K * 3).is_err(),
            "range with an unmapped gap must not be partially discarded"
        );
        assert!(
            aspace.pgtbl.modify().query(start).is_ok(),
            "failed MADV_DONTNEED must leave existing PTEs intact"
        );
    }

    #[def_test]
    fn test_mmap_resolve_addr_honors_fixed_and_no_replace_policies() {
        let mut aspace = new_test_aspace();
        map_shared(
            &mut aspace,
            0x5000,
            PAGE_SIZE_4K * 2,
            MappingFlags::READ | MappingFlags::WRITE,
        );

        let any = aspace
            .mmap_resolve_addr(
                VirtAddr::from(0x5000usize),
                PAGE_SIZE_4K,
                PAGE_SIZE_4K,
                AddrPolicy::Any,
            )
            .expect("allocator should find another free gap");
        assert_ne!(any, VirtAddr::from(0x5000usize));
        assert!(aspace.contains_range(any, PAGE_SIZE_4K));

        let err = aspace
            .mmap_resolve_addr(
                VirtAddr::from(0x5000usize),
                PAGE_SIZE_4K,
                PAGE_SIZE_4K,
                AddrPolicy::FixedNoReplace,
            )
            .expect_err("occupied fixed-no-replace mapping must fail");
        assert!(matches!(err, KError::AlreadyExists));

        let fixed = aspace
            .mmap_resolve_addr(
                VirtAddr::from(0x5000usize),
                PAGE_SIZE_4K,
                PAGE_SIZE_4K,
                AddrPolicy::Fixed,
            )
            .expect("fixed mappings should unmap and reuse the requested address");
        assert_eq!(fixed, VirtAddr::from(0x5000usize));
        assert!(aspace.find_vma(VirtAddr::from(0x5000usize)).is_none());
        assert!(aspace.find_vma(VirtAddr::from(0x6000usize)).is_some());
    }

    #[def_test]
    fn test_map_protect_and_access_checks_follow_permissions() {
        let mut aspace = new_test_aspace();
        let start = VirtAddr::from(0x9000usize);

        map_shared(
            &mut aspace,
            start.as_usize(),
            PAGE_SIZE_4K * 2,
            MappingFlags::READ | MappingFlags::WRITE,
        );

        assert!(aspace.can_access_range(start, PAGE_SIZE_4K * 2, MappingFlags::READ));
        assert!(aspace.can_access_range(start, PAGE_SIZE_4K * 2, MappingFlags::WRITE));
        assert!(!aspace.can_access_range(start, PAGE_SIZE_4K * 2, MappingFlags::EXECUTE));

        aspace
            .protect(start, PAGE_SIZE_4K * 2, MappingFlags::READ)
            .expect("protection update should succeed");

        assert!(aspace.can_access_range(start, PAGE_SIZE_4K * 2, MappingFlags::READ));
        assert!(!aspace.can_access_range(start, PAGE_SIZE_4K * 2, MappingFlags::WRITE));
    }

    #[def_test]
    fn test_extend_area_grows_existing_mapping_and_rejects_missing_start() {
        let mut aspace = new_test_aspace();
        let start = VirtAddr::from(0xd000usize);
        map_shared(
            &mut aspace,
            start.as_usize(),
            PAGE_SIZE_4K,
            MappingFlags::READ,
        );

        aspace
            .extend_area(start, PAGE_SIZE_4K)
            .expect("adjacent free space should allow extension");

        let area = aspace
            .find_vma(start)
            .expect("extended area should remain discoverable");
        assert_eq!(area.size(), PAGE_SIZE_4K * 2);

        let err = aspace
            .extend_area(VirtAddr::from(0x20000usize), PAGE_SIZE_4K)
            .expect_err("extending a missing area must fail");
        assert!(matches!(err, KError::NoMemory));
    }

    #[def_test]
    fn test_move_pages_preserves_contents_and_clears_source_mapping() {
        let mut aspace = new_test_aspace();
        let src = VirtAddr::from(0x11000usize);
        let dst = VirtAddr::from(0x15000usize);
        map_shared(
            &mut aspace,
            src.as_usize(),
            PAGE_SIZE_4K,
            MappingFlags::READ | MappingFlags::WRITE,
        );

        let payload = [1_u8, 2, 3, 4, 5];
        aspace
            .write(src + 37, &payload)
            .expect("mapped page should accept writes");

        aspace
            .move_pages(src, dst, PAGE_SIZE_4K, PageSize::Size4K)
            .expect("moving pages within one address space should succeed");

        let mut buf = [0_u8; 5];
        aspace
            .read(dst + 37, &mut buf)
            .expect("moved mapping should stay readable");
        assert_eq!(buf, payload);

        let err = aspace
            .read(src + 37, &mut buf)
            .expect_err("source mapping should be removed after move");
        assert!(matches!(err, KError::BadAddress));
    }

    #[def_test]
    fn map_with_max_flags_allows_protection_raise_within_may_permissions() {
        let start = VirtAddr::from_usize(0x4000);
        let current = MappingFlags::USER;
        let max = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
        let raised = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
        let mut aspace =
            MmSpace::new_empty_user(start, PAGE_SIZE_4K * 2).expect("allocate test address space");

        aspace
            .map_with_max_flags(
                start,
                PAGE_SIZE_4K,
                current,
                max,
                false,
                VmRuntimeRef::new_anon_private(start, khal::paging::PageSize::Size4K),
            )
            .expect("map prot-none-style private region");

        aspace
            .protect(start, PAGE_SIZE_4K, raised)
            .expect("mprotect should allow permissions inside max flags");
        assert!(aspace.can_access_range(start, PAGE_SIZE_4K, raised));
    }

    #[def_test]
    fn relocated_private_mapping_keeps_object_contents_after_source_metadata_drop() {
        let start = VirtAddr::from_usize(0x4000);
        let target = VirtAddr::from_usize(0x10000);
        let flags = MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER;
        let len = PAGE_SIZE_4K * 2;
        let aspace = Arc::new(Mutex::new(
            MmSpace::new_empty_user(start, PAGE_SIZE_4K * 32)
                .expect("allocate relocation test address space"),
        ));

        let mut mm = aspace.lock();
        mm.map(
            start,
            len,
            flags,
            false,
            VmRuntimeRef::new_anon_private(start, khal::paging::PageSize::Size4K),
        )
        .expect("map private source");
        mm.populate_area(start, PAGE_SIZE_4K, flags)
            .expect("populate source page");
        mm.write(start, b"abc").expect("seed source contents");

        let source = mm
            .resolve_mremap_source(start, len)
            .expect("resolve relocation source");
        mm.map_relocated_snapshot(&source, target, len, flags, &aspace)
            .expect("install relocated snapshot");
        mm.move_pages(start, target, len, khal::paging::PageSize::Size4K)
            .expect("move present pages");
        mm.drop_mapping_metadata(start, len)
            .expect("retire old metadata");

        assert!(!mm.can_access_range(start, PAGE_SIZE_4K, flags));

        mm.invalidate_present(target, PAGE_SIZE_4K)
            .expect("drop present destination PTE");
        mm.populate_area(target, PAGE_SIZE_4K, flags)
            .expect("refault destination page");

        let mut buf = [0u8; 3];
        mm.read(target, &mut buf)
            .expect("read relocated contents after refault");
        assert_eq!(&buf, b"abc");
    }
}

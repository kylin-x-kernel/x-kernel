// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux-aligned VMA/backing descriptors.

use alloc::{collections::BTreeMap, string::String, sync::Arc, vec::Vec};
use core::fmt;

use anon::{AnonLineageId, AnonSharedObject};
use kerrno::{KError, KResult};
use khal::{
    paging::{MappingFlags, PageSize, PageTableMut},
    trap::PageFaultFlags,
};
use ksync::Mutex;
use memaddr::{MemoryAddr, PAGE_SIZE_4K, VirtAddr, VirtAddrRange, is_aligned_4k};
use vmobj::VmObjectId;

use crate::{
    FaultContext, InvalidateHandle, MmSpace, PageFaultOutcome,
    backend::{
        FaultCompletionResult,
        linear::LinearBackend,
        private::PrivateBackend,
        shared::{SharedBackend, SharedPages},
    },
};

/// VMA-side execution operations.
///
/// This is the Linux-aligned analogue of the `vm_operations_struct` role:
/// the VMA holds an execution reference that knows how to materialize faults,
/// adjust protections, clone for fork, and relocate for `mremap`, while the
/// backing content object itself still lives in `Mapping` or anonymous/private
/// objects rather than inside the VMA.
pub trait VmRuntimeOps: Send + Sync {
    fn backing_info(&self) -> VmBackingInfo;
    fn map(&self, range: VirtAddrRange, flags: MappingFlags, pgtbl: &mut PageTableMut) -> KResult;
    fn unmap(&self, range: VirtAddrRange, pgtbl: &mut PageTableMut) -> KResult;
    fn on_protect(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        pgtbl: &mut PageTableMut,
    ) -> KResult<MappingFlags>;
    fn madvise_dontneed(
        &self,
        _vma: &VmArea,
        _range: VirtAddrRange,
        _pgtbl: &mut PageTableMut,
    ) -> KResult<bool> {
        Ok(false)
    }
    fn msync(
        &self,
        _vma: &VmArea,
        _range: VirtAddrRange,
        _policy: MsyncPolicy,
    ) -> KResult<MsyncRuntimeResult> {
        Ok(MsyncRuntimeResult::NotApplicable)
    }
    fn handle_fault(
        &self,
        ctx: FaultContext,
        flags: MappingFlags,
        pgtbl: &mut PageTableMut,
    ) -> FaultCompletionResult;
    fn relocate_for_mremap(
        &self,
        new_start: VirtAddr,
        new_mm_id: u64,
        aspace: &Arc<Mutex<MmSpace>>,
        invalidate: Option<InvalidateHandle>,
    ) -> KResult<Arc<dyn VmRuntimeOps>>;
    fn clone_for_fork(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        old_pgtbl: &mut PageTableMut,
        new_pgtbl: &mut PageTableMut,
        target: ForkCloneTarget<'_>,
    ) -> KResult<Arc<dyn VmRuntimeOps>>;
}

pub struct ForkCloneTarget<'a> {
    pub new_mm_id: u64,
    pub new_aspace: &'a Arc<Mutex<MmSpace>>,
    pub invalidate: Option<InvalidateHandle>,
}

/// Validated `msync()` policy visible to MM internals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsyncPolicy {
    sync: bool,
    async_: bool,
    invalidate: bool,
    data_only: bool,
}

impl MsyncPolicy {
    /// Creates a new validated `msync()` policy.
    pub fn try_new(sync: bool, async_: bool, invalidate: bool, data_only: bool) -> KResult<Self> {
        if sync && async_ {
            return Err(KError::InvalidInput);
        }
        Ok(Self {
            sync,
            async_,
            invalidate,
            data_only,
        })
    }

    /// Returns whether this request requires synchronous file writeback.
    pub const fn is_sync(self) -> bool {
        self.sync
    }

    /// Returns whether this request is `MS_ASYNC`.
    pub const fn is_async(self) -> bool {
        self.async_
    }

    /// Returns whether this request carries `MS_INVALIDATE`.
    pub const fn has_invalidate(self) -> bool {
        self.invalidate
    }

    /// Returns whether metadata sync can be skipped.
    pub const fn is_data_only(self) -> bool {
        self.data_only
    }
}

/// Result of a VMA runtime `msync()` hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsyncRuntimeResult {
    /// The runtime has no file-backed sync work for this range.
    NotApplicable,
    /// The runtime handled the sync request.
    Synced,
}

/// Lightweight runtime execution reference attached to a VMA instance.
///
/// This is not an object-identity handle like `VmObjectId`; it only carries
/// the VMA-side execution helper used by `MmSpace` for map, protect, unmap,
/// fault, fork clone, and mremap relocation.
#[derive(Clone)]
pub enum VmRuntimeRef {
    Linear(LinearBackend),
    AnonShared(SharedBackend),
    AnonPrivate(PrivateBackend),
    FileShared(Arc<dyn VmRuntimeOps>),
    FilePrivate(Arc<dyn VmRuntimeOps>),
}

/// Semantic class of a VMA-side runtime reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmRuntimeKind {
    Linear,
    AnonShared,
    AnonPrivate,
    FileShared,
    FilePrivate,
}

impl VmRuntimeRef {
    pub fn new_linear(offset: isize) -> Self {
        Self::Linear(LinearBackend::new(offset))
    }

    pub fn new_anon_shared(start: VirtAddr, size: usize, pgsize: PageSize) -> KResult<Self> {
        Ok(Self::AnonShared(SharedBackend::new_anonymous(
            start, size, pgsize,
        )?))
    }

    pub fn new_shared_pages(
        start: VirtAddr,
        pages: Arc<SharedPages>,
        object: Arc<AnonSharedObject>,
    ) -> Self {
        Self::AnonShared(SharedBackend::new(start, pages, object))
    }

    pub fn new_anon_private(start: VirtAddr, pgsize: PageSize) -> Self {
        Self::AnonPrivate(PrivateBackend::new(start, pgsize))
    }

    pub fn new_file_shared(ops: Arc<dyn VmRuntimeOps>) -> Self {
        Self::FileShared(ops)
    }

    pub fn new_file_private(ops: Arc<dyn VmRuntimeOps>) -> Self {
        Self::FilePrivate(ops)
    }

    fn backing_kind(&self) -> VmBackingKind {
        self.backing_info().kind()
    }

    fn kind(&self) -> VmRuntimeKind {
        match self {
            Self::Linear(_) => VmRuntimeKind::Linear,
            Self::AnonShared(_) => VmRuntimeKind::AnonShared,
            Self::AnonPrivate(_) => VmRuntimeKind::AnonPrivate,
            Self::FileShared(_) => VmRuntimeKind::FileShared,
            Self::FilePrivate(_) => VmRuntimeKind::FilePrivate,
        }
    }

    fn runtime_label(&self) -> &'static str {
        match self {
            Self::Linear(_) => "LinearRuntime",
            Self::AnonShared(_) => "AnonSharedRuntime",
            Self::AnonPrivate(_) => "AnonPrivateRuntime",
            Self::FileShared(_) => "FileSharedRuntime",
            Self::FilePrivate(_) => "FilePrivateRuntime",
        }
    }

    pub fn backing_info(&self) -> VmBackingInfo {
        match self {
            Self::Linear(inner) => VmRuntimeOps::backing_info(inner),
            Self::AnonShared(inner) => VmRuntimeOps::backing_info(inner),
            Self::AnonPrivate(inner) => VmRuntimeOps::backing_info(inner),
            Self::FileShared(inner) => inner.backing_info(),
            Self::FilePrivate(inner) => inner.backing_info(),
        }
    }

    pub(crate) fn register_object_views(
        &self,
        mm_id: u64,
        invalidate: InvalidateHandle,
        vma: &VmArea,
    ) -> Self {
        match self {
            Self::AnonShared(inner) => {
                Self::AnonShared(inner.register_object_view(mm_id, invalidate, vma))
            }
            Self::AnonPrivate(inner) => {
                Self::AnonPrivate(inner.register_object_view(mm_id, invalidate, vma))
            }
            _ => self.clone(),
        }
    }

    pub(crate) fn map(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        pgtbl: &mut PageTableMut,
    ) -> KResult {
        match self {
            Self::Linear(inner) => VmRuntimeOps::map(inner, range, flags, pgtbl),
            Self::AnonShared(inner) => VmRuntimeOps::map(inner, range, flags, pgtbl),
            Self::AnonPrivate(inner) => VmRuntimeOps::map(inner, range, flags, pgtbl),
            Self::FileShared(inner) => inner.map(range, flags, pgtbl),
            Self::FilePrivate(inner) => inner.map(range, flags, pgtbl),
        }
    }

    pub(crate) fn unmap(&self, range: VirtAddrRange, pgtbl: &mut PageTableMut) -> KResult {
        match self {
            Self::Linear(inner) => VmRuntimeOps::unmap(inner, range, pgtbl),
            Self::AnonShared(inner) => VmRuntimeOps::unmap(inner, range, pgtbl),
            Self::AnonPrivate(inner) => VmRuntimeOps::unmap(inner, range, pgtbl),
            Self::FileShared(inner) => inner.unmap(range, pgtbl),
            Self::FilePrivate(inner) => inner.unmap(range, pgtbl),
        }
    }

    pub(crate) fn on_protect(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        pgtbl: &mut PageTableMut,
    ) -> KResult<MappingFlags> {
        match self {
            Self::Linear(inner) => VmRuntimeOps::on_protect(inner, range, flags, pgtbl),
            Self::AnonShared(inner) => VmRuntimeOps::on_protect(inner, range, flags, pgtbl),
            Self::AnonPrivate(inner) => VmRuntimeOps::on_protect(inner, range, flags, pgtbl),
            Self::FileShared(inner) => inner.on_protect(range, flags, pgtbl),
            Self::FilePrivate(inner) => inner.on_protect(range, flags, pgtbl),
        }
    }

    pub(crate) fn madvise_dontneed(
        &self,
        vma: &VmArea,
        range: VirtAddrRange,
        pgtbl: &mut PageTableMut,
    ) -> KResult<bool> {
        match self {
            Self::Linear(inner) => VmRuntimeOps::madvise_dontneed(inner, vma, range, pgtbl),
            Self::AnonShared(inner) => VmRuntimeOps::madvise_dontneed(inner, vma, range, pgtbl),
            Self::AnonPrivate(inner) => VmRuntimeOps::madvise_dontneed(inner, vma, range, pgtbl),
            Self::FileShared(inner) => inner.madvise_dontneed(vma, range, pgtbl),
            Self::FilePrivate(inner) => inner.madvise_dontneed(vma, range, pgtbl),
        }
    }

    pub(crate) fn msync(
        &self,
        vma: &VmArea,
        range: VirtAddrRange,
        policy: MsyncPolicy,
    ) -> KResult<MsyncRuntimeResult> {
        match self {
            Self::Linear(inner) => VmRuntimeOps::msync(inner, vma, range, policy),
            Self::AnonShared(inner) => VmRuntimeOps::msync(inner, vma, range, policy),
            Self::AnonPrivate(inner) => VmRuntimeOps::msync(inner, vma, range, policy),
            Self::FileShared(inner) => inner.msync(vma, range, policy),
            Self::FilePrivate(inner) => inner.msync(vma, range, policy),
        }
    }

    pub(crate) fn handle_fault(
        &self,
        ctx: FaultContext,
        flags: MappingFlags,
        pgtbl: &mut PageTableMut,
    ) -> FaultCompletionResult {
        match self {
            Self::Linear(inner) => VmRuntimeOps::handle_fault(inner, ctx, flags, pgtbl),
            Self::AnonShared(inner) => VmRuntimeOps::handle_fault(inner, ctx, flags, pgtbl),
            Self::AnonPrivate(inner) => VmRuntimeOps::handle_fault(inner, ctx, flags, pgtbl),
            Self::FileShared(inner) => inner.handle_fault(ctx, flags, pgtbl),
            Self::FilePrivate(inner) => inner.handle_fault(ctx, flags, pgtbl),
        }
    }

    pub(crate) fn relocate_for_mremap(
        &self,
        new_start: VirtAddr,
        new_mm_id: u64,
        aspace: &Arc<Mutex<MmSpace>>,
        invalidate: Option<InvalidateHandle>,
    ) -> KResult<Self> {
        match self {
            Self::Linear(_) => Err(kerrno::KError::OperationNotSupported),
            Self::AnonShared(inner) => Ok(Self::AnonShared(inner.relocated(new_start))),
            Self::AnonPrivate(inner) => Ok(Self::AnonPrivate(inner.relocated(new_start))),
            Self::FileShared(inner) => inner
                .relocate_for_mremap(new_start, new_mm_id, aspace, invalidate)
                .map(Self::new_file_shared),
            Self::FilePrivate(inner) => inner
                .relocate_for_mremap(new_start, new_mm_id, aspace, invalidate)
                .map(Self::new_file_private),
        }
    }

    pub(crate) fn clone_for_fork(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        old_pgtbl: &mut PageTableMut,
        new_pgtbl: &mut PageTableMut,
        target: ForkCloneTarget<'_>,
    ) -> KResult<Self> {
        match self {
            Self::Linear(inner) => inner
                .clone_for_fork_runtime(
                    range,
                    flags,
                    old_pgtbl,
                    new_pgtbl,
                    target.new_aspace,
                    target.invalidate,
                )
                .map(Self::Linear),
            Self::AnonShared(inner) => inner
                .clone_for_fork_runtime(
                    range,
                    flags,
                    old_pgtbl,
                    new_pgtbl,
                    target.new_aspace,
                    target.invalidate,
                )
                .map(Self::AnonShared),
            Self::AnonPrivate(inner) => inner
                .clone_for_fork_runtime(
                    range,
                    flags,
                    old_pgtbl,
                    new_pgtbl,
                    target.new_aspace,
                    target.invalidate,
                )
                .map(Self::AnonPrivate),
            Self::FileShared(inner) => inner
                .clone_for_fork(range, flags, old_pgtbl, new_pgtbl, target)
                .map(Self::new_file_shared),
            Self::FilePrivate(inner) => inner
                .clone_for_fork(range, flags, old_pgtbl, new_pgtbl, target)
                .map(Self::new_file_private),
        }
    }
}

impl fmt::Debug for VmRuntimeRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("VmRuntimeRef")
            .field(&self.runtime_label())
            .field(&self.backing_kind())
            .finish()
    }
}

/// High-level backing classification for a VMA instance.
///
/// This mirrors the Linux model more closely than the old backend type names:
/// the VMA references either an anonymous object, a file-backed shared object,
/// or a private file-backed mapping with COW semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmBackingKind {
    /// Direct linear/physical mapping with no higher-level memory object.
    Linear,
    /// Shared anonymous backing object.
    AnonymousShared { object: VmObjectId },
    /// Private anonymous/COW backing object.
    AnonymousPrivate { object: VmObjectId },
    /// Shared file-backed mapping referencing an inode-owned object.
    FileShared { object: VmObjectId },
    /// Private file-backed mapping with explicit file-source and anonymous
    /// result-object identities.
    FilePrivate {
        file_object: VmObjectId,
        anon_object: VmObjectId,
        anon_lineage: AnonLineageId,
    },
}

/// Description of the object referenced by a VMA.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmBackingInfo {
    kind: VmBackingKind,
    page_size: PageSize,
}

impl VmBackingInfo {
    /// Creates a new VMA backing description.
    pub const fn new(kind: VmBackingKind, page_size: PageSize) -> Self {
        Self { kind, page_size }
    }

    /// Returns the backing classification.
    pub const fn kind(self) -> VmBackingKind {
        self.kind
    }

    /// Returns the page size used by the backing.
    pub const fn page_size(self) -> PageSize {
        self.page_size
    }
}

/// Current page permissions carried by a VMA.
///
/// This is VMA metadata, not page-table state. Existing execution paths still
/// translate it to `MappingFlags` when touching PTEs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VmPerm {
    flags: MappingFlags,
}

impl VmPerm {
    /// Creates a permission descriptor from legacy mapping flags.
    pub const fn from_mapping_flags(flags: MappingFlags) -> Self {
        Self { flags }
    }

    /// Returns the legacy mapping flags used by current page-table code.
    pub const fn mapping_flags(self) -> MappingFlags {
        self.flags
    }

    /// Returns whether this permission set contains `flags`.
    pub fn contains(self, flags: MappingFlags) -> bool {
        self.flags.contains(flags)
    }
}

impl From<MappingFlags> for VmPerm {
    fn from(flags: MappingFlags) -> Self {
        Self::from_mapping_flags(flags)
    }
}

impl From<VmPerm> for MappingFlags {
    fn from(perm: VmPerm) -> Self {
        perm.mapping_flags()
    }
}

/// Maximum permissions a VMA may be raised to by `mprotect()`.
///
/// This corresponds to Linux `VM_MAY*`-style metadata. It must survive VMA
/// split, merge, relocation, and fork independently from current permissions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VmMayPerm {
    flags: MappingFlags,
}

impl VmMayPerm {
    /// Creates a maximum-permission descriptor from legacy mapping flags.
    pub const fn from_mapping_flags(flags: MappingFlags) -> Self {
        Self { flags }
    }

    /// Returns the legacy mapping flags used by current compatibility paths.
    pub const fn mapping_flags(self) -> MappingFlags {
        self.flags
    }

    /// Returns whether `perm` stays inside the allowed permission envelope.
    pub fn allows(self, perm: VmPerm) -> bool {
        self.flags.contains(perm.mapping_flags())
    }
}

impl From<MappingFlags> for VmMayPerm {
    fn from(flags: MappingFlags) -> Self {
        Self::from_mapping_flags(flags)
    }
}

impl From<VmMayPerm> for MappingFlags {
    fn from(perm: VmMayPerm) -> Self {
        perm.mapping_flags()
    }
}

/// Fork/exec inheritance policy carried by a VMA.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmInheritance {
    /// Mapping is copied across `fork()` using normal shared/private rules.
    Copy,
    /// Mapping is excluded from child address spaces.
    DontCopy,
    /// Mapping exists in the child but is wiped to fresh contents.
    WipeOnFork,
}

impl VmInheritance {
    /// Returns the default Linux-like inheritance policy.
    pub const fn default_for_mapping() -> Self {
        Self::Copy
    }
}

/// File-backed metadata attached to a VMA for introspection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMappingInfo {
    /// File-relative byte offset corresponding to the VMA start.
    pub offset: u64,
    /// Inode number of the mapped file object.
    pub inode: u64,
    /// Resolved absolute path, if available.
    pub path: Option<String>,
}

/// Process-facing VMA metadata with explicit backing semantics.
#[derive(Debug, Clone)]
pub struct VmArea {
    range: VirtAddrRange,
    perm: VmPerm,
    may_perm: VmMayPerm,
    inheritance: VmInheritance,
    backing: VmBackingInfo,
    page_offset: u64,
    file: Option<FileMappingInfo>,
    runtime: Option<VmRuntimeRef>,
}

impl VmArea {
    fn shifted_file_mapping(&self, delta_pages: u64) -> Option<FileMappingInfo> {
        self.file.as_ref().map(|file| FileMappingInfo {
            offset: file.offset + delta_pages * PAGE_SIZE_4K as u64,
            inode: file.inode,
            path: file.path.clone(),
        })
    }

    /// Creates a new VMA metadata record.
    pub fn new(
        start: VirtAddr,
        size: usize,
        flags: khal::paging::MappingFlags,
        max_flags: khal::paging::MappingFlags,
        backing: VmBackingInfo,
        page_offset: u64,
        file: Option<FileMappingInfo>,
    ) -> Self {
        Self {
            range: VirtAddrRange::from_start_size(start, size),
            perm: VmPerm::from_mapping_flags(flags),
            may_perm: VmMayPerm::from_mapping_flags(max_flags),
            inheritance: VmInheritance::default_for_mapping(),
            backing,
            page_offset,
            file,
            runtime: None,
        }
    }

    /// Returns the VMA start address.
    pub fn start(&self) -> VirtAddr {
        self.range.start
    }

    /// Returns the VMA end address.
    pub fn end(&self) -> VirtAddr {
        self.range.end
    }

    /// Returns the VMA size in bytes.
    pub fn size(&self) -> usize {
        self.range.size()
    }

    /// Returns the VMA virtual-address range.
    pub fn range(&self) -> VirtAddrRange {
        self.range
    }

    /// Returns the raw mapping flags.
    pub fn flags(&self) -> khal::paging::MappingFlags {
        self.perm.mapping_flags()
    }

    /// Returns the maximum protection bits this VMA may be raised to.
    pub fn max_flags(&self) -> khal::paging::MappingFlags {
        self.may_perm.mapping_flags()
    }

    /// Returns the current VMA permission metadata.
    pub fn perm(&self) -> VmPerm {
        self.perm
    }

    /// Returns the maximum VMA permission metadata.
    pub fn may_perm(&self) -> VmMayPerm {
        self.may_perm
    }

    /// Returns the fork/exec inheritance policy for this VMA.
    pub fn inheritance(&self) -> VmInheritance {
        self.inheritance
    }

    /// Returns the Linux-aligned backing description for this VMA.
    pub fn backing(&self) -> VmBackingInfo {
        self.backing
    }

    /// Returns the backing-object page offset (`vm_pgoff`-style metadata).
    pub fn page_offset(&self) -> u64 {
        self.page_offset
    }

    /// Returns file-backed metadata for proc-style introspection, if present.
    pub fn file_mapping(&self) -> Option<&FileMappingInfo> {
        self.file.as_ref()
    }

    /// Returns the runtime execution reference for this VMA, if installed into
    /// an address space.
    pub(crate) fn runtime(&self) -> Option<&VmRuntimeRef> {
        self.runtime.as_ref()
    }

    fn runtime_kind(&self) -> Option<VmRuntimeKind> {
        self.runtime.as_ref().map(VmRuntimeRef::kind)
    }

    fn has_same_known_runtime_kind(&self, next: &Self) -> bool {
        matches!(
            (self.runtime_kind(), next.runtime_kind()),
            (Some(left), Some(right)) if left == right
        )
    }

    /// Returns the backing-object page index for the given address.
    ///
    /// This mirrors Linux's use of `vm_pgoff + ((addr - vm_start) >> PAGE_SHIFT)`.
    pub fn page_index_for(&self, addr: VirtAddr) -> u64 {
        self.page_offset + ((addr - self.start()) / PAGE_SIZE_4K) as u64
    }

    /// Returns the precise file byte offset for the given address, if this is
    /// a file-backed VMA with known file metadata.
    pub fn file_offset_for(&self, addr: VirtAddr) -> Option<u64> {
        let rel = addr.as_usize().saturating_sub(self.start().as_usize()) as u64;
        self.file.as_ref().map(|file| file.offset + rel)
    }

    /// Returns the backing-object byte offset for the given address.
    pub fn backing_offset_for(&self, addr: VirtAddr) -> Option<u64> {
        if let Some(offset) = self.file_offset_for(addr) {
            return Some(offset);
        }
        let rel = addr.as_usize().checked_sub(self.start().as_usize())? as u64;
        Some(self.page_offset() * PAGE_SIZE_4K as u64 + rel)
    }

    fn next_page_offset(&self) -> Option<u64> {
        if !is_aligned_4k(self.size()) {
            return None;
        }
        self.page_offset
            .checked_add((self.size() / PAGE_SIZE_4K) as u64)
    }

    fn file_mapping_contiguous_with(&self, next: &Self) -> bool {
        match (self.file_mapping(), next.file_mapping()) {
            (None, None) => true,
            (Some(left), Some(right)) => {
                left.inode == right.inode
                    && left.path == right.path
                    && left
                        .offset
                        .checked_add(self.size() as u64)
                        .is_some_and(|offset| offset == right.offset)
            }
            _ => false,
        }
    }

    /// Returns whether two adjacent VMAs may be merged without changing
    /// mapping semantics.
    pub(crate) fn can_merge_with(&self, next: &Self) -> bool {
        self.end() == next.start()
            && self.perm == next.perm
            && self.may_perm == next.may_perm
            && self.inheritance == next.inheritance
            && self.backing == next.backing
            && self.next_page_offset() == Some(next.page_offset)
            && self.file_mapping_contiguous_with(next)
            && self.has_same_known_runtime_kind(next)
    }

    /// Returns a relocated copy of this VMA metadata.
    pub fn relocated(
        &self,
        start: VirtAddr,
        size: usize,
        flags: khal::paging::MappingFlags,
    ) -> Self {
        Self::new(
            start,
            size,
            flags,
            self.max_flags(),
            self.backing,
            self.page_offset,
            self.file.clone(),
        )
        .with_inheritance(self.inheritance)
        .with_runtime(self.runtime.clone())
    }

    /// Returns a copy of this VMA metadata with a replacement backing
    /// description.
    pub fn with_backing(mut self, backing: VmBackingInfo) -> Self {
        self.backing = backing;
        self
    }

    /// Returns a copy of this VMA metadata with a replacement inheritance
    /// policy.
    pub fn with_inheritance(mut self, inheritance: VmInheritance) -> Self {
        self.inheritance = inheritance;
        self
    }

    /// Returns whether this VMA permits the requested page-fault access mode.
    pub fn allows_fault(&self, access_flags: PageFaultFlags) -> bool {
        self.perm.contains(access_flags)
    }

    /// Returns whether `mprotect()` may raise this VMA to `new_flags`.
    pub fn allows_protection(&self, new_flags: khal::paging::MappingFlags) -> bool {
        self.may_perm.allows(VmPerm::from_mapping_flags(new_flags))
    }

    /// Dispatches a page fault using this VMA's explicit backing semantics.
    ///
    /// This mirrors Linux's `vm_area_struct` role: the VMA selects the high-
    /// level fault policy before the backing-specific implementation
    /// materializes pages.
    pub fn handle_fault(&self, aspace: &mut MmSpace, ctx: FaultContext) -> PageFaultOutcome {
        let page_base = ctx.address().align_down(self.backing.page_size());
        let ctx = ctx.with_backing(
            Some(self.page_index_for(ctx.address())),
            self.file_offset_for(ctx.address()),
            Some(self.start().as_usize().saturating_sub(page_base.as_usize())),
        );
        match self.backing.kind() {
            VmBackingKind::Linear => aspace.handle_linear_fault(self.clone(), ctx),
            VmBackingKind::AnonymousShared { .. } => {
                aspace.handle_anonymous_shared_fault(self.clone(), ctx)
            }
            VmBackingKind::AnonymousPrivate { .. } => {
                aspace.handle_anonymous_private_fault(self.clone(), ctx)
            }
            VmBackingKind::FileShared { .. } => aspace.handle_file_shared_fault(self.clone(), ctx),
            VmBackingKind::FilePrivate { .. } => {
                aspace.handle_file_private_fault(self.clone(), ctx)
            }
        }
    }

    pub(crate) fn set_end(&mut self, new_end: VirtAddr) {
        self.range.end = new_end;
    }

    pub(crate) fn set_flags(&mut self, flags: khal::paging::MappingFlags) {
        self.perm = VmPerm::from_mapping_flags(flags);
    }

    pub(crate) fn with_runtime(mut self, runtime: Option<VmRuntimeRef>) -> Self {
        self.runtime = runtime;
        self
    }

    pub(crate) fn set_runtime(&mut self, runtime: VmRuntimeRef) {
        self.runtime = Some(runtime);
    }

    pub(crate) fn split(&mut self, pos: VirtAddr) -> Option<Self> {
        if self.start() < pos && pos < self.end() {
            let delta_pages = ((pos - self.start()) / PAGE_SIZE_4K) as u64;
            let new_area = Self {
                range: VirtAddrRange::new(pos, self.end()),
                perm: self.perm,
                may_perm: self.may_perm,
                inheritance: self.inheritance,
                backing: self.backing,
                page_offset: self.page_offset + delta_pages,
                file: self.shifted_file_mapping(delta_pages),
                runtime: self.runtime.clone(),
            };
            self.range.end = pos;
            Some(new_area)
        } else {
            None
        }
    }
}

/// Owned VMA metadata container maintained alongside the legacy `MemorySet`.
pub struct VmAreaSet {
    areas: BTreeMap<VirtAddr, VmArea>,
}

impl VmAreaSet {
    /// Creates an empty VMA set.
    pub const fn new() -> Self {
        Self {
            areas: BTreeMap::new(),
        }
    }

    /// Iterates all VMAs in ascending address order.
    pub fn iter(&self) -> impl Iterator<Item = &VmArea> {
        self.areas.values()
    }

    /// Finds the VMA containing `addr`.
    pub fn find(&self, addr: VirtAddr) -> Option<&VmArea> {
        let candidate = self.areas.range(..=addr).last().map(|(_, area)| area)?;
        candidate.range().contains(addr).then_some(candidate)
    }

    /// Returns whether `range` overlaps any existing VMA.
    pub fn overlaps(&self, range: VirtAddrRange) -> bool {
        if let Some(area) = self
            .areas
            .range(..range.end)
            .next_back()
            .map(|(_, area)| area)
            && area.end() > range.start
        {
            return true;
        }
        self.areas
            .range(range.start..)
            .next()
            .is_some_and(|(_, area)| area.start() < range.end)
    }

    /// Collects cloned VMAs overlapping `range` in ascending address order.
    pub fn collect_overlapping(&self, range: VirtAddrRange) -> Vec<VmArea> {
        let mut overlapped = Vec::new();
        if let Some(area) = self
            .areas
            .range(..=range.start)
            .next_back()
            .map(|(_, area)| area)
            && area.end() > range.start
        {
            overlapped.push(area.clone());
        }
        for area in self
            .areas
            .range(range.start..range.end)
            .map(|(_, area)| area)
        {
            if area.start() >= range.end {
                break;
            }
            if overlapped
                .last()
                .is_some_and(|last| last.start() == area.start())
            {
                continue;
            }
            overlapped.push(area.clone());
        }
        overlapped
    }

    /// Finds a free range that can accommodate `size` bytes within `limit`.
    pub fn find_free_area(
        &self,
        hint: VirtAddr,
        size: usize,
        limit: VirtAddrRange,
        align: usize,
    ) -> Option<VirtAddr> {
        if !size.is_multiple_of(align) {
            return None;
        }
        let mut last_end = hint.max(limit.start).align_up(align);
        if let Some((_, area)) = self.areas.range(..last_end).last() {
            last_end = last_end.max(area.end()).align_up(align);
        }
        for (&addr, area) in self.areas.range(last_end..) {
            if last_end.checked_add(size).is_some_and(|end| end <= addr) {
                return Some(last_end);
            }
            last_end = area.end().align_up(align);
        }
        if last_end
            .checked_add(size)
            .is_some_and(|end| end <= limit.end)
        {
            Some(last_end)
        } else {
            None
        }
    }

    /// Inserts a new VMA and panics if it overlaps existing areas.
    pub fn insert(&mut self, area: VmArea) {
        assert!(self.try_insert(area).is_ok());
    }

    /// Attempts to insert a new VMA while preserving the non-overlap invariant.
    pub fn try_insert(&mut self, area: VmArea) -> KResult {
        if self.overlaps(area.range()) {
            return Err(kerrno::KError::AlreadyExists);
        }
        let previous = self.areas.insert(area.start(), area);
        debug_assert!(previous.is_none());
        Ok(())
    }

    /// Merges the VMA at `start` with adjacent VMAs accepted by `can_merge`.
    ///
    /// Task 1.2 only provides the container operation. The semantic predicate
    /// that decides whether two adjacent VMAs are equivalent is frozen in
    /// Task 1.3.
    pub(crate) fn merge_adjacent_where(
        &mut self,
        start: VirtAddr,
        can_merge: impl Fn(&VmArea, &VmArea) -> bool,
    ) -> Option<VirtAddr> {
        let mut current_start = start;
        let current = self.areas.get(&current_start)?;

        if let Some((&left_start, left)) = self.areas.range(..current_start).next_back()
            && left.end() == current.start()
            && can_merge(left, current)
        {
            let current = self.areas.remove(&current_start)?;
            self.areas
                .get_mut(&left_start)
                .expect("left VMA must still exist")
                .set_end(current.end());
            current_start = left_start;
        }

        let current_end = self.areas.get(&current_start)?.end();
        if let Some((&right_start, right)) = self.areas.range(current_end..).next()
            && right.start() == current_end
            && can_merge(self.areas.get(&current_start)?, right)
        {
            let right = self.areas.remove(&right_start)?;
            self.areas
                .get_mut(&current_start)
                .expect("current VMA must still exist")
                .set_end(right.end());
        }

        Some(current_start)
    }

    /// Merges the VMA at `start` with semantically identical adjacent VMAs.
    ///
    /// This is intentionally conservative: a missed merge is acceptable, but
    /// merging distinct mapping semantics is not.
    #[cfg_attr(
        not(unittest),
        expect(
            dead_code,
            reason = "production merge call sites need runtime/view lifecycle updates"
        )
    )]
    pub(crate) fn merge_adjacent(&mut self, start: VirtAddr) -> Option<VirtAddr> {
        self.merge_adjacent_where(start, VmArea::can_merge_with)
    }

    /// Removes mappings within `[start, start + size)`.
    pub fn unmap(&mut self, start: VirtAddr, size: usize) {
        if size == 0 {
            return;
        }
        let range = VirtAddrRange::from_start_size(start, size);
        let end = range.end;

        self.areas
            .retain(|_, area| !area.range().contained_in(range));

        let before_key = self.areas.range(..start).next_back().map(|(&key, _)| key);
        if let Some(before_start) = before_key {
            let mut split_right = None;
            let mut remove_before = false;
            {
                let before = self.areas.get_mut(&before_start).unwrap();
                let before_end = before.end();
                if before_end > start {
                    if before_end <= end {
                        before.set_end(start);
                    } else {
                        split_right = before.split(end);
                        before.set_end(start);
                    }
                    remove_before = before.size() == 0;
                }
            }
            if remove_before {
                self.areas.remove(&before_start);
            }
            if let Some(right) = split_right {
                self.areas.insert(right.start(), right);
            }
        }

        let after_key = self.areas.range(start..).next().map(|(&key, _)| key);
        if let Some(after_start) = after_key {
            let after_end = self.areas.get(&after_start).unwrap().end();
            if after_start < end {
                let mut new_area = self.areas.remove(&after_start).unwrap();
                let delta_pages = ((end - after_start) / PAGE_SIZE_4K) as u64;
                new_area.page_offset += delta_pages;
                new_area.file = new_area.shifted_file_mapping(delta_pages);
                new_area.range.start = end;
                if new_area.start() < after_end {
                    self.areas.insert(new_area.start(), new_area);
                }
            }
        }
    }

    /// Updates mapping flags for VMAs overlapping `[start, start + size)`.
    pub fn protect(&mut self, start: VirtAddr, size: usize, new_flags: khal::paging::MappingFlags) {
        if size == 0 {
            return;
        }
        let end = start + size;
        let mut to_insert = Vec::new();
        for (&area_start, area) in self.areas.iter_mut() {
            let area_end = area.end();

            if area_start >= end {
                break;
            } else if area_end <= start {
                continue;
            } else if area_start >= start && area_end <= end {
                area.set_flags(new_flags);
            } else if area_start < start && area_end > end {
                let right = area.split(end).unwrap();
                area.set_end(start);

                let middle = VmArea::new(
                    start,
                    size,
                    new_flags,
                    area.max_flags(),
                    area.backing(),
                    area.page_offset + ((start - area_start) / PAGE_SIZE_4K) as u64,
                    area.shifted_file_mapping(((start - area_start) / PAGE_SIZE_4K) as u64),
                )
                .with_inheritance(area.inheritance())
                .with_runtime(area.runtime().cloned());
                to_insert.push((right.start(), right));
                to_insert.push((middle.start(), middle));
            } else if area_end > end {
                let right = area.split(end).unwrap();
                area.set_flags(new_flags);
                to_insert.push((right.start(), right));
            } else {
                let mut right = area.split(start).unwrap();
                right.set_flags(new_flags);
                to_insert.push((right.start(), right));
            }
        }
        self.areas.extend(to_insert);
    }

    /// Extends the VMA starting at `start`.
    pub fn extend(&mut self, start: VirtAddr, additional: usize) {
        if additional == 0 {
            return;
        }
        let area = self
            .areas
            .get_mut(&start)
            .expect("vma metadata must exist for extend");
        let new_end = area.end().wrapping_add(additional);
        assert!(is_aligned_4k(additional));
        area.set_end(new_end);
    }

    /// Clears all metadata.
    pub fn clear(&mut self) {
        self.areas.clear();
    }
}

impl Default for VmAreaSet {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(unittest)]
mod tests {
    use kerrno::KError;
    use khal::paging::{MappingFlags, PageSize};
    use memaddr::{PAGE_SIZE_4K, VirtAddr};
    use unittest::def_test;
    use vmobj::{AnonObjectId, FileObjectId, VmObjectId};

    use super::{
        FileMappingInfo, VmArea, VmAreaSet, VmBackingInfo, VmBackingKind, VmInheritance,
        VmRuntimeRef,
    };

    fn vaddr(addr: usize) -> VirtAddr {
        VirtAddr::from_usize(addr)
    }

    fn test_area(start: usize, size: usize, page_offset: u64) -> VmArea {
        area_with_flags(
            start,
            size,
            page_offset,
            MappingFlags::READ | MappingFlags::WRITE,
            MappingFlags::READ | MappingFlags::WRITE,
        )
    }

    fn area_with_flags(
        start: usize,
        size: usize,
        page_offset: u64,
        flags: MappingFlags,
        max_flags: MappingFlags,
    ) -> VmArea {
        VmArea::new(
            vaddr(start),
            size,
            flags,
            max_flags,
            VmBackingInfo::new(VmBackingKind::Linear, PageSize::Size4K),
            page_offset,
            None,
        )
    }

    fn file_area(start: usize, size: usize, page_offset: u64, file_offset: u64) -> VmArea {
        VmArea::new(
            vaddr(start),
            size,
            MappingFlags::READ | MappingFlags::WRITE,
            MappingFlags::READ | MappingFlags::WRITE,
            VmBackingInfo::new(
                VmBackingKind::FileShared {
                    object: VmObjectId::File(FileObjectId::from_raw(7)),
                },
                PageSize::Size4K,
            ),
            page_offset,
            Some(FileMappingInfo {
                offset: file_offset,
                inode: 11,
                path: None,
            }),
        )
    }

    fn ranges(set: &VmAreaSet) -> alloc::vec::Vec<(usize, usize, u64)> {
        set.iter()
            .map(|area| {
                (
                    area.start().as_usize(),
                    area.end().as_usize(),
                    area.page_offset(),
                )
            })
            .collect()
    }

    fn file_offsets(set: &VmAreaSet) -> alloc::vec::Vec<u64> {
        set.iter()
            .map(|area| area.file_mapping().expect("file metadata").offset)
            .collect()
    }

    fn merge_result(left: VmArea, right: VmArea) -> alloc::vec::Vec<(usize, usize, u64)> {
        let mut set = VmAreaSet::new();
        let start = left.start();
        set.insert(left);
        set.insert(right);
        set.merge_adjacent(start).expect("target VMA must exist");
        ranges(&set)
    }

    #[def_test]
    fn vma_set_rejects_overlapping_insert() {
        let mut set = VmAreaSet::new();
        set.try_insert(test_area(0x1000, PAGE_SIZE_4K * 2, 4))
            .expect("first insert must succeed");

        assert_eq!(
            set.try_insert(test_area(0x2000, PAGE_SIZE_4K, 5)),
            Err(KError::AlreadyExists)
        );
        set.try_insert(test_area(0x3000, PAGE_SIZE_4K, 6))
            .expect("adjacent insert must succeed");
    }

    #[def_test]
    fn vma_set_middle_unmap_splits_and_preserves_offsets() {
        let mut set = VmAreaSet::new();
        set.insert(test_area(0x1000, PAGE_SIZE_4K * 4, 10));

        set.unmap(vaddr(0x2000), PAGE_SIZE_4K);

        assert_eq!(
            ranges(&set),
            alloc::vec![(0x1000, 0x2000, 10), (0x3000, 0x5000, 12)]
        );
    }

    #[def_test]
    fn vma_set_front_and_back_unmap_preserve_remaining_metadata() {
        let mut set = VmAreaSet::new();
        set.insert(test_area(0x1000, PAGE_SIZE_4K * 4, 20));

        set.unmap(vaddr(0x1000), PAGE_SIZE_4K);
        assert_eq!(ranges(&set), alloc::vec![(0x2000, 0x5000, 21)]);

        set.unmap(vaddr(0x4000), PAGE_SIZE_4K);
        assert_eq!(ranges(&set), alloc::vec![(0x2000, 0x4000, 21)]);
    }

    #[def_test]
    fn vma_set_unmap_preserves_file_metadata_offsets() {
        let mut set = VmAreaSet::new();
        set.insert(file_area(0x1000, PAGE_SIZE_4K * 4, 50, 0x8000));

        set.unmap(vaddr(0x2000), PAGE_SIZE_4K);

        assert_eq!(
            ranges(&set),
            alloc::vec![(0x1000, 0x2000, 50), (0x3000, 0x5000, 52)]
        );
        assert_eq!(file_offsets(&set), alloc::vec![0x8000, 0xa000]);
    }

    #[def_test]
    fn vma_set_exact_unmap_removes_only_target_area() {
        let mut set = VmAreaSet::new();
        set.insert(test_area(0x1000, PAGE_SIZE_4K, 1));
        set.insert(test_area(0x3000, PAGE_SIZE_4K, 3));

        set.unmap(vaddr(0x1000), PAGE_SIZE_4K);

        assert!(set.find(vaddr(0x1000)).is_none());
        assert!(set.find(vaddr(0x3000)).is_some());
        assert_eq!(ranges(&set), alloc::vec![(0x3000, 0x4000, 3)]);
    }

    #[def_test]
    fn vma_set_protect_middle_preserves_non_permission_metadata() {
        let mut set = VmAreaSet::new();
        set.insert(
            test_area(0x1000, PAGE_SIZE_4K * 3, 30).with_inheritance(VmInheritance::DontCopy),
        );

        set.protect(vaddr(0x2000), PAGE_SIZE_4K, MappingFlags::READ);

        let areas = set.iter().collect::<alloc::vec::Vec<_>>();
        assert_eq!(areas.len(), 3);
        assert_eq!(areas[0].inheritance(), VmInheritance::DontCopy);
        assert_eq!(areas[1].inheritance(), VmInheritance::DontCopy);
        assert_eq!(areas[2].inheritance(), VmInheritance::DontCopy);
        assert_eq!(areas[1].flags(), MappingFlags::READ);
        assert_eq!(
            areas[1].max_flags(),
            MappingFlags::READ | MappingFlags::WRITE
        );
        assert_eq!(areas[1].page_offset(), 31);
    }

    #[def_test]
    fn vma_set_protect_preserves_file_metadata_offsets() {
        let mut set = VmAreaSet::new();
        set.insert(file_area(0x1000, PAGE_SIZE_4K * 3, 70, 0x10000));

        set.protect(vaddr(0x2000), PAGE_SIZE_4K, MappingFlags::READ);

        let areas = set.iter().collect::<alloc::vec::Vec<_>>();
        assert_eq!(areas.len(), 3);
        assert_eq!(file_offsets(&set), alloc::vec![0x10000, 0x11000, 0x12000]);
        assert_eq!(areas[1].file_offset_for(vaddr(0x2000)), Some(0x11000));
        assert_eq!(areas[1].page_offset(), 71);
    }

    #[def_test]
    fn vma_set_merge_adjacent_uses_caller_predicate() {
        let mut set = VmAreaSet::new();
        set.insert(test_area(0x1000, PAGE_SIZE_4K, 40));
        set.insert(test_area(0x2000, PAGE_SIZE_4K, 41));
        set.insert(test_area(0x3000, PAGE_SIZE_4K, 99));

        let merged_start = set
            .merge_adjacent_where(vaddr(0x2000), |left, right| {
                left.flags() == right.flags()
                    && left.max_flags() == right.max_flags()
                    && left.backing() == right.backing()
                    && left.inheritance() == right.inheritance()
                    && left.page_offset() + (left.size() / PAGE_SIZE_4K) as u64
                        == right.page_offset()
            })
            .expect("target VMA must exist");

        assert_eq!(merged_start, vaddr(0x1000));
        assert_eq!(
            ranges(&set),
            alloc::vec![(0x1000, 0x3000, 40), (0x3000, 0x4000, 99)]
        );
    }

    #[def_test]
    fn vma_set_semantic_merge_accepts_matching_adjacent_vmas() {
        let left =
            test_area(0x1000, PAGE_SIZE_4K, 40).with_runtime(Some(VmRuntimeRef::new_linear(0)));
        let right =
            test_area(0x2000, PAGE_SIZE_4K, 41).with_runtime(Some(VmRuntimeRef::new_linear(0)));

        assert_eq!(merge_result(left, right), alloc::vec![(0x1000, 0x3000, 40)]);
    }

    #[def_test]
    fn vma_set_semantic_merge_rejects_permission_mismatch() {
        let left = test_area(0x1000, PAGE_SIZE_4K, 40);
        let right = area_with_flags(
            0x2000,
            PAGE_SIZE_4K,
            41,
            MappingFlags::READ,
            MappingFlags::READ | MappingFlags::WRITE,
        );

        assert_eq!(
            merge_result(left, right),
            alloc::vec![(0x1000, 0x2000, 40), (0x2000, 0x3000, 41)]
        );
    }

    #[def_test]
    fn vma_set_semantic_merge_rejects_max_permission_mismatch() {
        let left = test_area(0x1000, PAGE_SIZE_4K, 40);
        let right = area_with_flags(
            0x2000,
            PAGE_SIZE_4K,
            41,
            MappingFlags::READ | MappingFlags::WRITE,
            MappingFlags::READ,
        );

        assert_eq!(
            merge_result(left, right),
            alloc::vec![(0x1000, 0x2000, 40), (0x2000, 0x3000, 41)]
        );
    }

    #[def_test]
    fn vma_set_semantic_merge_rejects_backing_mismatch() {
        let left = test_area(0x1000, PAGE_SIZE_4K, 40);
        let right = test_area(0x2000, PAGE_SIZE_4K, 41).with_backing(VmBackingInfo::new(
            VmBackingKind::AnonymousPrivate {
                object: VmObjectId::Anon(AnonObjectId::from_raw(9)),
            },
            PageSize::Size4K,
        ));

        assert_eq!(
            merge_result(left, right),
            alloc::vec![(0x1000, 0x2000, 40), (0x2000, 0x3000, 41)]
        );
    }

    #[def_test]
    fn vma_set_semantic_merge_rejects_object_offset_gap() {
        let left = test_area(0x1000, PAGE_SIZE_4K, 40);
        let right = test_area(0x2000, PAGE_SIZE_4K, 99);

        assert_eq!(
            merge_result(left, right),
            alloc::vec![(0x1000, 0x2000, 40), (0x2000, 0x3000, 99)]
        );
    }

    #[def_test]
    fn vma_set_semantic_merge_rejects_file_metadata_gap() {
        let left = file_area(0x1000, PAGE_SIZE_4K, 40, 0x8000);
        let right = file_area(0x2000, PAGE_SIZE_4K, 41, 0xc000);

        assert_eq!(
            merge_result(left, right),
            alloc::vec![(0x1000, 0x2000, 40), (0x2000, 0x3000, 41)]
        );
    }

    #[def_test]
    fn vma_set_semantic_merge_rejects_inheritance_mismatch() {
        let left = test_area(0x1000, PAGE_SIZE_4K, 40);
        let right = test_area(0x2000, PAGE_SIZE_4K, 41).with_inheritance(VmInheritance::DontCopy);

        assert_eq!(
            merge_result(left, right),
            alloc::vec![(0x1000, 0x2000, 40), (0x2000, 0x3000, 41)]
        );
    }

    #[def_test]
    fn vma_set_semantic_merge_rejects_runtime_kind_mismatch() {
        let left =
            test_area(0x1000, PAGE_SIZE_4K, 40).with_runtime(Some(VmRuntimeRef::new_linear(0)));
        let right = test_area(0x2000, PAGE_SIZE_4K, 41).with_runtime(Some(
            VmRuntimeRef::new_anon_private(vaddr(0x2000), PageSize::Size4K),
        ));

        assert_eq!(
            merge_result(left, right),
            alloc::vec![(0x1000, 0x2000, 40), (0x2000, 0x3000, 41)]
        );
    }

    #[def_test]
    fn vma_set_semantic_merge_rejects_unknown_runtime_kind() {
        let left = test_area(0x1000, PAGE_SIZE_4K, 40);
        let right = test_area(0x2000, PAGE_SIZE_4K, 41);

        assert_eq!(
            merge_result(left, right),
            alloc::vec![(0x1000, 0x2000, 40), (0x2000, 0x3000, 41)]
        );
    }
}

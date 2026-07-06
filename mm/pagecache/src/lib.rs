// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Inode-owned page cache objects.
//!
//! This crate introduces the first stage of a Linux-aligned `address_space`
//! analogue for X-Kernel. The key ownership boundary mirrors Linux:
//!
//! - the inode-facing object owns cached folios;
//! - VMA or file-open instances reference that object;
//! - the object, not the VMA, owns cached content and truncation semantics.
//!
//! Linux references:
//! - `struct address_space` in `include/linux/fs.h`
//! - generic page cache helpers in `mm/filemap.c`
//! - shmem-backed page cache usage in `mm/shmem.c`
#![no_std]

extern crate alloc;

use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use kalloc::{UsageKind, global_allocator};
use kerrno::{KError, KResult};
use khal::mem::{PhysAddr, VirtAddr, v2p};
use ksync::Mutex;
use log::warn;
use memaddr::PAGE_SIZE_4K;
use vmobj::{
    FileObjectId, MappingView, MappingViewId, MappingViewNotifier, MappingViewRange,
    MappingViewSpec, ObjectInvalidateWork, ObjectViewHit, VmObjectId, next_mapping_view_id,
};

/// Page index within a mapping.
pub type PageIndex = u64;

/// Mapping storage class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MappingKind {
    /// tmpfs/shmem-style in-memory object.
    InMemory,
    /// Regular inode-backed file object.
    FileBacked,
}

/// Stable identity for a shared cached object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MappingIdentity(u64);

impl MappingIdentity {
    /// Returns the opaque identity value.
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Returns the typed VM object identity for this file-backed mapping.
    pub const fn vm_object_id(self) -> VmObjectId {
        VmObjectId::File(FileObjectId::from_raw(self.raw()))
    }
}

static NEXT_MAPPING_ID: AtomicU64 = AtomicU64::new(1);

struct MappingViewRegistration {
    mapping: Arc<Mapping>,
    id: MappingViewId,
}

/// Lifetime guard for one registered mapping view.
#[derive(Clone)]
pub struct MappingViewGuard {
    inner: Arc<MappingViewRegistration>,
}

impl MappingViewGuard {
    /// Returns the stable registration id kept alive by this guard.
    pub fn id(&self) -> MappingViewId {
        self.inner.id
    }
}

impl Drop for MappingViewRegistration {
    fn drop(&mut self) {
        self.mapping.unregister_view(self.id);
    }
}

type EvictListenerFn = dyn Fn(PageIndex, &Folio) + Send + Sync;

struct EvictListener {
    id: usize,
    listener: Arc<EvictListenerFn>,
}

struct EvictRegistrationInner {
    mapping: Weak<Mapping>,
    listener_id: usize,
}

/// Lifetime guard for one registered folio-eviction listener.
///
/// Dropping the guard unregisters the callback, matching Rust ownership and
/// avoiding a separate numeric-id cleanup path.
#[derive(Clone)]
pub struct EvictRegistration {
    _inner: Arc<EvictRegistrationInner>,
}

impl Drop for EvictRegistrationInner {
    fn drop(&mut self) {
        if let Some(mapping) = self.mapping.upgrade() {
            mapping.remove_evict_listener(self.listener_id);
        }
    }
}

struct RegisteredView {
    view: MappingView,
    notifier: Option<Arc<dyn MappingViewNotifier>>,
}

/// Tail-byte range zeroed within the last surviving folio after a shrink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TailZeroRange {
    index: PageIndex,
    offset: usize,
}

impl TailZeroRange {
    /// Returns the folio index whose tail bytes were zeroed.
    pub const fn index(self) -> PageIndex {
        self.index
    }

    /// Returns the starting byte offset within the folio that was zeroed.
    pub const fn offset(self) -> usize {
        self.offset
    }
}

/// Object-level truncate/invalidate plan emitted by [`Mapping::resize`].
///
/// This is the first-stage analogue of Linux invalidation work driven from
/// `address_space`: the content object decides which cached folios were
/// dropped and which surviving tail bytes had to be zeroed. Later `MmSpace`
/// reverse-mapping work can consume this plan to unmap affected PTEs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncatePlan {
    old_len: u64,
    new_len: u64,
    dropped_pages: Vec<PageIndex>,
    zeroed_tail: Option<TailZeroRange>,
    invalidate_work: Option<ObjectInvalidateWork>,
}

impl TruncatePlan {
    fn unchanged(len: u64) -> Self {
        Self {
            old_len: len,
            new_len: len,
            dropped_pages: Vec::new(),
            zeroed_tail: None,
            invalidate_work: None,
        }
    }

    /// Returns the previous visible object length.
    pub const fn old_len(&self) -> u64 {
        self.old_len
    }

    /// Returns the new visible object length.
    pub const fn new_len(&self) -> u64 {
        self.new_len
    }

    /// Returns the folio indices removed past the new EOF.
    pub fn dropped_pages(&self) -> &[PageIndex] {
        &self.dropped_pages
    }

    /// Returns the surviving tail range zeroed after shrink, if any.
    pub const fn zeroed_tail(&self) -> Option<TailZeroRange> {
        self.zeroed_tail
    }

    /// Returns the mapping views that may need invalidate/unmap work.
    pub fn affected_views(&self) -> &[ObjectViewHit] {
        self.invalidate_work
            .as_ref()
            .map_or(&[], ObjectInvalidateWork::hits)
    }

    /// Returns the object-side invalidation work emitted by this resize.
    pub fn invalidate_work(&self) -> Option<&ObjectInvalidateWork> {
        self.invalidate_work.as_ref()
    }

    /// Returns whether this resize produced no cache invalidation work.
    pub fn is_noop(&self) -> bool {
        self.old_len == self.new_len
            && self.dropped_pages.is_empty()
            && self.zeroed_tail.is_none()
            && self.invalidate_work.is_none()
    }

    fn invalidated_range(&self) -> Option<(u64, u64)> {
        (self.new_len < self.old_len).then_some((self.new_len, self.old_len))
    }
}

/// Mapping-specific object operations.
pub trait MappingOps: Send + Sync {
    /// Materialize a new folio for `index`.
    fn instantiate_folio(&self, index: PageIndex) -> KResult<Folio>;
}

struct InMemoryMappingOps;

impl MappingOps for InMemoryMappingOps {
    fn instantiate_folio(&self, _index: PageIndex) -> KResult<Folio> {
        Folio::new_zeroed()
    }
}

struct MappingInner {
    pages: BTreeMap<PageIndex, Arc<Mutex<Folio>>>,
    views: BTreeMap<MappingViewId, RegisteredView>,
    evict_listeners: Vec<EvictListener>,
    len: u64,
}

/// Linux-like inode-owned cached object.
pub struct Mapping {
    kind: MappingKind,
    identity: MappingIdentity,
    ops: Arc<dyn MappingOps>,
    next_evict_listener_id: AtomicUsize,
    inner: Mutex<MappingInner>,
}

impl Mapping {
    /// Creates a new mapping with source-specific materialization operations.
    pub fn new(kind: MappingKind, len: u64, ops: Arc<dyn MappingOps>) -> Arc<Self> {
        Arc::new(Self {
            kind,
            identity: MappingIdentity(NEXT_MAPPING_ID.fetch_add(1, Ordering::Relaxed)),
            ops,
            next_evict_listener_id: AtomicUsize::new(1),
            inner: Mutex::new(MappingInner {
                pages: BTreeMap::new(),
                views: BTreeMap::new(),
                evict_listeners: Vec::new(),
                len,
            }),
        })
    }

    /// Creates a new tmpfs/shmem-style in-memory mapping.
    pub fn new_in_memory() -> Arc<Self> {
        Self::new(MappingKind::InMemory, 0, Arc::new(InMemoryMappingOps))
    }

    /// Returns the mapping kind.
    pub const fn kind(&self) -> MappingKind {
        self.kind
    }

    /// Returns the stable identity for this mapping.
    pub const fn identity(&self) -> MappingIdentity {
        self.identity
    }

    /// Returns the current object length.
    pub fn len(&self) -> u64 {
        self.inner.lock().len
    }

    /// Returns `true` if the mapping currently has no visible bytes.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Registers one VMA view against this cached object.
    pub fn register_view(self: &Arc<Self>, spec: MappingViewSpec) -> MappingViewGuard {
        let id = next_mapping_view_id();
        self.inner.lock().views.insert(
            id,
            RegisteredView {
                view: MappingView::new(
                    id,
                    spec.mm_id,
                    MappingViewRange {
                        vma_start: spec.vma_start,
                        vma_len: spec.vma_len,
                        object_start: spec.object_start,
                        object_len: spec.object_len,
                    },
                    spec.kind,
                ),
                notifier: spec.notifier,
            },
        );
        MappingViewGuard {
            inner: Arc::new(MappingViewRegistration {
                mapping: self.clone(),
                id,
            }),
        }
    }

    fn unregister_view(&self, id: MappingViewId) {
        self.inner.lock().views.remove(&id);
    }

    /// Registers a callback invoked before a cached folio is evicted.
    pub fn add_evict_listener<F>(self: &Arc<Self>, listener: F) -> EvictRegistration
    where
        F: Fn(PageIndex, &Folio) + Send + Sync + 'static,
    {
        let id = self.next_evict_listener_id.fetch_add(1, Ordering::Relaxed);
        self.inner.lock().evict_listeners.push(EvictListener {
            id,
            listener: Arc::new(listener),
        });
        EvictRegistration {
            _inner: Arc::new(EvictRegistrationInner {
                mapping: Arc::downgrade(self),
                listener_id: id,
            }),
        }
    }

    fn remove_evict_listener(&self, listener_id: usize) -> bool {
        let mut inner = self.inner.lock();
        let old_len = inner.evict_listeners.len();
        inner
            .evict_listeners
            .retain(|listener| listener.id != listener_id);
        inner.evict_listeners.len() != old_len
    }

    fn notify_evicted_folios(&self, folios: &[(PageIndex, Arc<Mutex<Folio>>)]) {
        if folios.is_empty() {
            return;
        }
        let listeners = self
            .inner
            .lock()
            .evict_listeners
            .iter()
            .map(|listener| listener.listener.clone())
            .collect::<Vec<_>>();
        if listeners.is_empty() {
            return;
        }
        for (index, folio) in folios {
            let folio = folio.lock();
            for listener in &listeners {
                listener(*index, &folio);
            }
        }
    }

    /// Runs `f` with the cached folio at `index`, if present.
    pub fn with_folio<R>(&self, index: PageIndex, f: impl FnOnce(Option<&mut Folio>) -> R) -> R {
        let folio = self.inner.lock().pages.get(&index).cloned();
        if let Some(folio) = folio {
            let mut folio = folio.lock();
            f(Some(&mut folio))
        } else {
            f(None)
        }
    }

    /// Runs `f` with a folio at `index`, allocating one on demand.
    pub fn with_folio_or_create<R>(
        &self,
        index: PageIndex,
        f: impl FnOnce(&mut Folio) -> KResult<R>,
    ) -> KResult<R> {
        let folio = {
            let mut inner = self.inner.lock();
            if let Some(folio) = inner.pages.get(&index) {
                folio.clone()
            } else {
                let folio = Arc::new(Mutex::new(self.ops.instantiate_folio(index)?));
                inner.pages.insert(index, folio.clone());
                folio
            }
        };
        let mut folio = folio.lock();
        f(&mut folio)
    }

    /// Reads bytes from the mapping into `dst`.
    pub fn read_into(&self, offset: u64, dst: &mut [u8]) -> usize {
        let len = self.len();
        if offset >= len || dst.is_empty() {
            return 0;
        }

        let end = (offset + dst.len() as u64).min(len);
        let mut copied = 0usize;
        while copied < (end - offset) as usize {
            let pos = offset + copied as u64;
            let index = pos / PAGE_SIZE_4K as u64;
            let page_off = (pos % PAGE_SIZE_4K as u64) as usize;
            let step = ((end - pos) as usize).min(PAGE_SIZE_4K - page_off);
            self.with_folio(index, |folio| {
                if let Some(folio) = folio {
                    dst[copied..copied + step]
                        .copy_from_slice(&folio.data()[page_off..page_off + step]);
                } else {
                    dst[copied..copied + step].fill(0);
                }
            });
            copied += step;
        }
        copied
    }

    /// Reads bytes from the mapping into `dst`, materializing missing folios.
    pub fn read_into_or_create(&self, offset: u64, dst: &mut [u8]) -> KResult<usize> {
        let len = self.len();
        if offset >= len || dst.is_empty() {
            return Ok(0);
        }

        let end = (offset + dst.len() as u64).min(len);
        let mut copied = 0usize;
        while copied < (end - offset) as usize {
            let pos = offset + copied as u64;
            let index = pos / PAGE_SIZE_4K as u64;
            let page_off = (pos % PAGE_SIZE_4K as u64) as usize;
            let step = ((end - pos) as usize).min(PAGE_SIZE_4K - page_off);
            self.with_folio_or_create(index, |folio| {
                dst[copied..copied + step]
                    .copy_from_slice(&folio.data()[page_off..page_off + step]);
                Ok(())
            })?;
            copied += step;
        }
        Ok(copied)
    }

    /// Writes bytes from `src` into the mapping starting at `offset`.
    pub fn write_from(&self, offset: u64, src: &[u8]) -> KResult<usize> {
        self.write_from_with_dirty(offset, src, |folio| {
            folio.mark_dirty();
            Ok(())
        })
    }

    /// Writes bytes from `src` and delegates dirty-state policy to `dirty_folio_fn`.
    pub fn write_from_with_dirty(
        &self,
        offset: u64,
        src: &[u8],
        mut dirty_folio_fn: impl FnMut(&mut Folio) -> KResult<()>,
    ) -> KResult<usize> {
        if src.is_empty() {
            return Ok(0);
        }

        let end = offset
            .checked_add(src.len() as u64)
            .ok_or(KError::InvalidInput)?;
        self.ensure_len(end);

        let mut written = 0usize;
        while written < src.len() {
            let pos = offset + written as u64;
            let index = pos / PAGE_SIZE_4K as u64;
            let page_off = (pos % PAGE_SIZE_4K as u64) as usize;
            let step = (src.len() - written).min(PAGE_SIZE_4K - page_off);
            self.with_folio_or_create(index, |folio| {
                folio.data()[page_off..page_off + step]
                    .copy_from_slice(&src[written..written + step]);
                dirty_folio_fn(folio)?;
                Ok(())
            })?;
            written += step;
        }
        Ok(written)
    }

    /// Appends `src` to the end of the mapping.
    pub fn append_from(&self, src: &[u8]) -> KResult<(usize, u64)> {
        let start = self.len();
        let written = self.write_from(start, src)?;
        Ok((written, start + written as u64))
    }

    /// Resizes the visible object length and returns the resulting truncate plan.
    pub fn resize(&self, len: u64) -> KResult<TruncatePlan> {
        let mut inner = self.inner.lock();
        let old_len = inner.len;
        if len == old_len {
            return Ok(TruncatePlan::unchanged(len));
        }
        inner.len = len;

        if len >= old_len {
            if len > old_len {
                self.zero_growth_tail(&mut inner, old_len, len)?;
            }
            return Ok(TruncatePlan {
                old_len,
                new_len: len,
                dropped_pages: Vec::new(),
                zeroed_tail: None,
                invalidate_work: None,
            });
        }

        let first_truncated = len.div_ceil(PAGE_SIZE_4K as u64);
        let dropped_folios = inner
            .pages
            .range(first_truncated..)
            .map(|(index, folio)| (*index, folio.clone()))
            .collect::<Vec<_>>();
        // Explicit truncation: clear dirty before eviction, analogous to
        // Linux's cancel_dirty_page() in truncate_inode_pages_range().
        for (_, folio) in &dropped_folios {
            folio.lock().clear_dirty();
        }
        let dropped_pages = dropped_folios.iter().map(|(index, _)| *index).collect();
        inner.pages.retain(|index, _| *index < first_truncated);

        let tail_index = len / PAGE_SIZE_4K as u64;
        let tail_off = (len % PAGE_SIZE_4K as u64) as usize;
        let mut zeroed_tail = None;
        if tail_off != 0
            && let Some(folio) = inner.pages.get(&tail_index)
        {
            let mut folio = folio.lock();
            folio.data()[tail_off..].fill(0);
            folio.mark_dirty();
            zeroed_tail = Some(TailZeroRange {
                index: tail_index,
                offset: tail_off,
            });
        }

        let mut plan = TruncatePlan {
            old_len,
            new_len: len,
            dropped_pages,
            zeroed_tail,
            invalidate_work: None,
        };
        let Some((invalid_start, invalid_end)) = plan.invalidated_range() else {
            drop(inner);
            self.notify_evicted_folios(&dropped_folios);
            return Ok(plan);
        };
        let invalid_len = (invalid_end - invalid_start) as usize;
        let mut affected_views = Vec::new();
        let mut notifiers = Vec::new();
        for registered in inner.views.values() {
            let Some(hit) = registered.view.page_hit(invalid_start, invalid_len) else {
                continue;
            };
            affected_views.push(hit.clone());
            if let Some(notifier) = &registered.notifier {
                notifiers.push((hit, notifier.clone()));
            }
        }
        plan.invalidate_work = (!affected_views.is_empty()).then(|| {
            ObjectInvalidateWork::new(
                self.identity.vm_object_id(),
                invalid_start,
                invalid_len,
                affected_views,
            )
        });
        drop(inner);
        self.notify_evicted_folios(&dropped_folios);
        if let Some(work) = plan.invalidate_work.as_ref() {
            for (hit, notifier) in notifiers {
                notifier.invalidate(work, &hit);
            }
        }
        Ok(plan)
    }

    /// Sets the visible object length and truncates cached folios past EOF.
    pub fn set_len(&self, len: u64) -> KResult<()> {
        self.resize(len).map(|_| ())
    }

    /// Writes back dirty cached folios through `write_folio_fn`.
    pub fn writeback(
        &self,
        mut write_folio_fn: impl FnMut(PageIndex, &[u8], usize) -> KResult<()>,
    ) -> KResult<()> {
        self.writeback_until(0, u64::MAX, &mut write_folio_fn)
    }

    /// Writes back dirty cached folios intersecting `[start, start + len)`.
    pub fn writeback_range(
        &self,
        start: u64,
        len: usize,
        mut write_folio_fn: impl FnMut(PageIndex, &[u8], usize) -> KResult<()>,
    ) -> KResult<()> {
        if len == 0 {
            return Ok(());
        }
        let end = start.checked_add(len as u64).ok_or(KError::InvalidInput)?;
        self.writeback_until(start, end, &mut write_folio_fn)
    }

    /// Writes back dirty cached folios from `start` through the end of the object.
    pub fn writeback_from(
        &self,
        start: u64,
        mut write_folio_fn: impl FnMut(PageIndex, &[u8], usize) -> KResult<()>,
    ) -> KResult<()> {
        self.writeback_until(start, u64::MAX, &mut write_folio_fn)
    }

    /// Writes back dirty cached folios intersecting `[start, end)`.
    pub fn writeback_until(
        &self,
        start: u64,
        end: u64,
        write_folio_fn: &mut impl FnMut(PageIndex, &[u8], usize) -> KResult<()>,
    ) -> KResult<()> {
        let (len, folios) = {
            let inner = self.inner.lock();
            (
                inner.len,
                inner
                    .pages
                    .iter()
                    .filter(|(index, _)| {
                        let Some(page_start) = index.checked_mul(PAGE_SIZE_4K as u64) else {
                            return true;
                        };
                        let page_end = page_start.saturating_add(PAGE_SIZE_4K as u64);
                        page_start < end && start < page_end
                    })
                    .map(|(index, folio)| (*index, folio.clone()))
                    .collect::<Vec<_>>(),
            )
        };

        for (index, folio) in folios {
            let mut folio = folio.lock();
            if !folio.is_dirty() {
                continue;
            }
            let page_start = index
                .checked_mul(PAGE_SIZE_4K as u64)
                .ok_or(KError::InvalidInput)?;
            let valid_len = len.saturating_sub(page_start).min(PAGE_SIZE_4K as u64) as usize;
            write_folio_fn(index, &folio.data()[..valid_len], valid_len)?;
            folio.clear_dirty();
        }
        Ok(())
    }

    /// Drops cached folios whose index is at or after `first_page`.
    pub fn invalidate_from_page(&self, first_page: PageIndex) -> KResult<Vec<PageIndex>> {
        let dropped_folios = {
            let mut inner = self.inner.lock();
            let dropped = inner
                .pages
                .range(first_page..)
                .map(|(index, folio)| (*index, folio.clone()))
                .collect::<Vec<_>>();
            inner.pages.retain(|index, _| *index < first_page);
            dropped
        };
        // Callers must writeback before invalidation.
        assert!(
            dropped_folios.iter().all(|(_, f)| !f.lock().is_dirty()),
            "invalidate_from_page: dirty folios in eviction range — call writeback first"
        );
        self.notify_evicted_folios(&dropped_folios);
        Ok(dropped_folios.into_iter().map(|(index, _)| index).collect())
    }

    fn ensure_len(&self, len: u64) {
        let mut inner = self.inner.lock();
        if len > inner.len {
            inner.len = len;
        }
    }

    fn zero_growth_tail(
        &self,
        inner: &mut MappingInner,
        old_len: u64,
        new_len: u64,
    ) -> KResult<()> {
        if old_len == 0 {
            return Ok(());
        }
        let tail_index = (old_len - 1) / PAGE_SIZE_4K as u64;
        let Some(folio) = inner.pages.get(&tail_index).cloned() else {
            return Ok(());
        };
        let page_start = tail_index * PAGE_SIZE_4K as u64;
        let old_off = (old_len - page_start) as usize;
        let new_off = (new_len - page_start).min(PAGE_SIZE_4K as u64) as usize;
        if old_off >= new_off {
            return Ok(());
        }
        let mut folio = folio.lock();
        folio.data()[old_off..new_off].fill(0);
        Ok(())
    }
}

/// Cached folio stored inside a [`Mapping`].
#[derive(Debug)]
pub struct Folio {
    addr: VirtAddr,
    dirty: bool,
}

impl Folio {
    /// Allocates a fresh zero-filled folio.
    pub fn new_zeroed() -> KResult<Self> {
        let addr = global_allocator()
            .alloc_pages(1, PAGE_SIZE_4K, UsageKind::PageCache)
            .map_err(|_| KError::NoMemory)?;
        let addr = VirtAddr::from(addr);
        // SAFETY: `alloc_pages` returns a writable virtually mapped page-sized
        // region owned by this folio. Zeroing exactly one page is sound.
        unsafe { core::ptr::write_bytes(addr.as_mut_ptr(), 0, PAGE_SIZE_4K) };
        Ok(Self { addr, dirty: false })
    }

    /// Returns the physical address of this folio.
    pub fn paddr(&self) -> PhysAddr {
        v2p(self.addr)
    }

    /// Marks the folio dirty.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Returns whether the folio is currently dirty.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Clears the dirty bit after successful writeback or truncation cleanup.
    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    /// Returns the folio contents as a mutable 4 KiB slice.
    pub fn data(&mut self) -> &mut [u8] {
        // SAFETY: the folio uniquely owns the page at `self.addr`; callers
        // obtain `&mut self`, so producing a mutable slice for that page is
        // exclusive for the duration of the borrow.
        unsafe { core::slice::from_raw_parts_mut(self.addr.as_mut_ptr(), PAGE_SIZE_4K) }
    }
}

impl Drop for Folio {
    fn drop(&mut self) {
        if self.dirty {
            warn!("dropping dirty in-memory folio without writeback");
        }
        global_allocator().dealloc_pages(self.addr.as_usize(), 1, UsageKind::PageCache);
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::{sync::Arc, vec, vec::Vec};

    use kerrno::{KError, KResult};
    use ksync::Mutex;
    use unittest::{assert_eq, assert_ne, def_test};
    use vmobj::{MappingViewKind, MappingViewRange, MappingViewSpec};

    use super::{Folio, Mapping, MappingKind, MappingOps, PAGE_SIZE_4K, PageIndex, TailZeroRange};

    struct RecordingOps {
        writes: Mutex<Vec<(PageIndex, usize)>>,
        fail_writeback: bool,
    }

    impl RecordingOps {
        fn new(fail_writeback: bool) -> Arc<Self> {
            Arc::new(Self {
                writes: Mutex::new(Vec::new()),
                fail_writeback,
            })
        }

        fn writes(&self) -> Vec<(PageIndex, usize)> {
            self.writes.lock().clone()
        }

        fn record_write(&self, index: PageIndex, valid_len: usize) -> KResult<()> {
            self.writes.lock().push((index, valid_len));
            if self.fail_writeback {
                return Err(KError::InvalidInput);
            }
            Ok(())
        }
    }

    impl MappingOps for RecordingOps {
        fn instantiate_folio(&self, _index: PageIndex) -> KResult<Folio> {
            Folio::new_zeroed()
        }
    }

    #[def_test]
    fn mapping_identity_is_stable() {
        let first = Mapping::new_in_memory();
        let second = Mapping::new_in_memory();
        assert_ne!(first.identity().raw(), second.identity().raw());
    }

    #[def_test]
    fn mapping_read_write_roundtrip() {
        let mapping = Mapping::new_in_memory();
        mapping.write_from(0, b"hello").unwrap();
        let mut buf = [0u8; 5];
        assert_eq!(mapping.read_into(0, &mut buf), 5);
        assert_eq!(&buf, b"hello");
    }

    #[def_test]
    fn mapping_sparse_reads_zero_fill() {
        let mapping = Mapping::new_in_memory();
        mapping.write_from(4096, b"x").unwrap();
        let mut buf = vec![0xaa; 4];
        assert_eq!(mapping.read_into(0, &mut buf), 4);
        assert_eq!(&buf, &[0, 0, 0, 0]);
    }

    #[def_test]
    fn writeback_range_writes_only_intersecting_dirty_folios() {
        let ops = RecordingOps::new(false);
        let mapping = Mapping::new(
            MappingKind::FileBacked,
            (PAGE_SIZE_4K * 2) as u64,
            ops.clone(),
        );
        mapping.write_from(0, &vec![1; PAGE_SIZE_4K]).unwrap();
        mapping
            .write_from(PAGE_SIZE_4K as u64, &vec![2; PAGE_SIZE_4K])
            .unwrap();

        mapping
            .writeback_range(
                PAGE_SIZE_4K as u64,
                PAGE_SIZE_4K,
                |index, _data, valid_len| ops.record_write(index, valid_len),
            )
            .unwrap();

        assert_eq!(ops.writes(), vec![(1, PAGE_SIZE_4K)]);
        mapping.with_folio(0, |folio| {
            assert!(folio.expect("folio 0").is_dirty());
        });
        mapping.with_folio(1, |folio| {
            assert!(!folio.expect("folio 1").is_dirty());
        });
    }

    #[def_test]
    fn writeback_range_uses_valid_len_for_partial_final_folio() {
        let ops = RecordingOps::new(false);
        let mapping = Mapping::new(MappingKind::FileBacked, 0, ops.clone());
        mapping.write_from(0, &vec![3; PAGE_SIZE_4K + 123]).unwrap();

        mapping
            .writeback_range(
                PAGE_SIZE_4K as u64,
                PAGE_SIZE_4K,
                |index, _data, valid_len| ops.record_write(index, valid_len),
            )
            .unwrap();

        assert_eq!(ops.writes(), vec![(1, 123)]);
    }

    #[def_test]
    fn writeback_range_keeps_dirty_on_writeback_error() {
        let ops = RecordingOps::new(true);
        let mapping = Mapping::new(MappingKind::FileBacked, PAGE_SIZE_4K as u64, ops.clone());
        mapping.write_from(0, b"dirty").unwrap();

        assert!(
            mapping
                .writeback_range(0, PAGE_SIZE_4K, |index, _data, valid_len| {
                    ops.record_write(index, valid_len)
                })
                .is_err()
        );
        assert_eq!(ops.writes(), vec![(0, PAGE_SIZE_4K)]);
        mapping.with_folio(0, |folio| {
            assert!(folio.expect("folio").is_dirty());
        });
    }

    #[def_test]
    fn evict_listener_is_raii_and_invalidate_notifies() {
        let mapping = Mapping::new_in_memory();
        mapping.write_from(0, &vec![1; PAGE_SIZE_4K * 2]).unwrap();
        mapping.writeback(|_, _, _| Ok(())).unwrap();

        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_for_listener = seen.clone();
        let guard = mapping.add_evict_listener(move |index, _folio| {
            seen_for_listener.lock().push(index);
        });

        assert_eq!(mapping.invalidate_from_page(1).unwrap(), vec![1]);
        assert_eq!(*seen.lock(), vec![1]);

        drop(guard);
        mapping
            .write_from(PAGE_SIZE_4K as u64, &vec![2; PAGE_SIZE_4K])
            .unwrap();
        mapping.writeback(|_, _, _| Ok(())).unwrap();
        assert_eq!(mapping.invalidate_from_page(1).unwrap(), vec![1]);
        assert_eq!(*seen.lock(), vec![1]);
    }

    #[def_test]
    fn mapping_resize_returns_truncate_plan() {
        let mapping = Mapping::new_in_memory();
        mapping.write_from(0, &[1; 5000]).unwrap();
        let _view = mapping.register_view(MappingViewSpec {
            mm_id: 1,
            vma_start: 0x4000,
            vma_len: 5000,
            object_start: 0,
            object_len: 5000,
            kind: MappingViewKind::Shared,
            notifier: None,
        });
        let plan = mapping.resize(3000).unwrap();
        assert_eq!(plan.old_len(), 5000);
        assert_eq!(plan.new_len(), 3000);
        assert_eq!(plan.dropped_pages(), &[1]);
        assert_eq!(plan.affected_views().len(), 1);
        assert_eq!(
            plan.zeroed_tail(),
            Some(TailZeroRange {
                index: 0,
                offset: 3000,
            })
        );
    }

    #[def_test]
    fn mapping_resize_only_marks_overlapping_views() {
        let mapping = Mapping::new_in_memory();
        mapping.write_from(0, &[1; 9000]).unwrap();
        let _first = mapping.register_view(MappingViewSpec {
            mm_id: 1,
            vma_start: 0x4000,
            vma_len: PAGE_SIZE_4K,
            object_start: 0,
            object_len: PAGE_SIZE_4K,
            kind: MappingViewKind::Shared,
            notifier: None,
        });
        let _second = mapping.register_view(MappingViewSpec {
            mm_id: 2,
            vma_start: 0x8000,
            vma_len: PAGE_SIZE_4K,
            object_start: 2 * PAGE_SIZE_4K as u64,
            object_len: PAGE_SIZE_4K,
            kind: MappingViewKind::Shared,
            notifier: None,
        });

        let plan = mapping.resize(5000).unwrap();
        assert_eq!(plan.affected_views().len(), 1);
        assert_eq!(plan.affected_views()[0].view().mm_id(), 2);
        assert_eq!(
            plan.affected_views()[0].view().object_start(),
            2 * PAGE_SIZE_4K as u64
        );
    }

    #[def_test]
    fn mapping_view_can_override_object_start() {
        let mapping = Mapping::new_in_memory();
        mapping.write_from(0, &[1; PAGE_SIZE_4K]).unwrap();
        let _view = mapping.register_view(MappingViewSpec {
            mm_id: 1,
            vma_start: 0x4000,
            vma_len: 0x500,
            object_start: 0,
            object_len: 0x500,
            kind: MappingViewKind::Private,
            notifier: None,
        });

        let plan = mapping.resize(0x480).unwrap();
        assert_eq!(plan.affected_views().len(), 1);
        assert_eq!(plan.affected_views()[0].view().object_start(), 0);
        assert_eq!(plan.affected_views()[0].view().object_end(), 0x500);
    }

    #[def_test]
    fn mapping_view_exposes_stable_rmap_coordinates() {
        let mapping = Mapping::new_in_memory();
        let guard = mapping.register_view(MappingViewSpec {
            mm_id: 7,
            vma_start: 0x8000,
            vma_len: 0x3000,
            object_start: 0x1000,
            object_len: 0x3000,
            kind: MappingViewKind::Private,
            notifier: None,
        });
        let view = mapping
            .inner
            .lock()
            .views
            .get(&guard.id())
            .expect("view must stay registered")
            .view
            .clone();
        assert_eq!(view.id(), guard.id());
        assert_eq!(
            view.range(),
            MappingViewRange {
                vma_start: 0x8000,
                vma_len: 0x3000,
                object_start: 0x1000,
                object_len: 0x3000,
            }
        );
        assert_eq!(view.page_offset(), 1);
        assert!(view.overlaps_object_range(0x1800, 0x2000));
        assert_eq!(view.object_to_vma_offset(0x1800), Some(0x800));
        assert_eq!(view.object_to_vma_offset(0x5000), None);
    }

    /// `invalidate_from_page` asserts evicted folios are clean.
    /// Callers must writeback first.
    #[def_test]
    fn invalidate_from_page_rejects_dirty_folios() {
        let ops = RecordingOps::new(false);
        let mapping = Mapping::new(
            MappingKind::FileBacked,
            (PAGE_SIZE_4K * 2) as u64,
            ops.clone(),
        );
        mapping.write_from(0, &vec![1; PAGE_SIZE_4K]).unwrap();
        mapping
            .write_from(PAGE_SIZE_4K as u64, &vec![2; PAGE_SIZE_4K])
            .unwrap();

        // Writeback first, then invalidate.
        mapping
            .writeback(|index, _data, valid_len| ops.record_write(index, valid_len))
            .unwrap();
        let dropped = mapping.invalidate_from_page(0).unwrap();

        assert_eq!(dropped.len(), 2);
        assert_eq!(ops.writes(), vec![(0, PAGE_SIZE_4K), (1, PAGE_SIZE_4K)]);
    }

    /// Regression test: `set_len` truncation drops dirty folios without
    /// calling writeback — the "dropping dirty in-memory folio" warning path.
    /// This is triggered by `echo > file` in shell scripts.
    #[def_test]
    fn set_len_truncation_without_writeback_drops_dirty_folios() {
        let ops = RecordingOps::new(false);
        let mapping = Mapping::new(
            MappingKind::FileBacked,
            (PAGE_SIZE_4K * 2) as u64,
            ops.clone(),
        );
        // Write data: two folios become dirty.
        mapping.write_from(0, &vec![1; PAGE_SIZE_4K]).unwrap();
        mapping
            .write_from(PAGE_SIZE_4K as u64, &vec![2; PAGE_SIZE_4K])
            .unwrap();

        // Verify both folios are dirty.
        mapping.with_folio(0, |folio| {
            assert!(folio.expect("folio 0").is_dirty());
        });
        mapping.with_folio(1, |folio| {
            assert!(folio.expect("folio 1").is_dirty());
        });

        // Truncate to 0 without writeback — dirty folios are silently lost.
        mapping.set_len(0).unwrap();

        assert!(
            ops.writes().is_empty(),
            "writeback must not have been called during set_len truncation"
        );
    }

    /// Regression test: writeback-then-invalidate preserves dirty data.
    /// This is the correct pattern that `AddressSpace::evict()` now follows.
    #[def_test]
    fn writeback_before_invalidate_preserves_dirty_data() {
        let ops = RecordingOps::new(false);
        let mapping = Mapping::new(
            MappingKind::FileBacked,
            (PAGE_SIZE_4K * 2) as u64,
            ops.clone(),
        );
        mapping.write_from(0, &vec![1; PAGE_SIZE_4K]).unwrap();
        mapping
            .write_from(PAGE_SIZE_4K as u64, &vec![2; PAGE_SIZE_4K])
            .unwrap();

        // Writeback THEN invalidate — the correct pattern.
        mapping
            .writeback(|index, _data, valid_len| ops.record_write(index, valid_len))
            .unwrap();
        let dropped = mapping.invalidate_from_page(0).unwrap();

        assert_eq!(dropped.len(), 2);
        assert_eq!(ops.writes(), vec![(0, PAGE_SIZE_4K), (1, PAGE_SIZE_4K)]);
    }
}

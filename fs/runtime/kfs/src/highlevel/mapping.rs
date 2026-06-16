// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Inode-scoped file mapping and page cache support.

use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    num::NonZeroUsize,
    ptr::NonNull,
    sync::atomic::{AtomicU8, Ordering},
};

use intrusive_collections::{LinkedList, LinkedListAtomicLink, intrusive_adapter};
use kalloc::{UsageKind, global_allocator};
use khal::mem::{PhysAddr, VirtAddr, v2p};
use ksync::Mutex;
use ktask::WaitQueue;
use kvfs::{AddressSpaceOperations, FileNode, Location, VfsError, VfsResult};
use lru::LruCache;
use memaddr::PAGE_SIZE_4K;

const PAGE_STATE_LOADING: u8 = 0;
const PAGE_STATE_READY: u8 = 1;
const PAGE_STATE_FAILED: u8 = 2;

/// Page-cache index within a file mapping.
pub type PageIndex = u64;

/// One resident page in an inode-scoped file mapping.
#[derive(Debug)]
pub struct PageCache {
    addr: VirtAddr,
    dirty: bool,
}

impl PageCache {
    fn new() -> VfsResult<Self> {
        let addr = global_allocator()
            .alloc_pages(1, PAGE_SIZE_4K, UsageKind::PageCache)
            .inspect_err(|err| {
                warn!("Failed to allocate page cache: {:?}", err);
            })
            .map_err(|e| match e {
                alloc_engine::AllocError::NoMemory => VfsError::NoMemory,
                _ => VfsError::InvalidInput,
            })?;
        Ok(Self {
            addr: addr.into(),
            dirty: false,
        })
    }

    /// Returns the physical address backing this cached page.
    pub fn paddr(&self) -> PhysAddr {
        v2p(self.addr)
    }

    /// Mark this cached page dirty.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Returns the mutable page bytes.
    pub fn data(&mut self) -> &mut [u8] {
        // SAFETY: `addr` is a dedicated page-cache allocation of exactly one
        // 4 KiB page and `&mut self` guarantees unique mutable access here.
        unsafe { core::slice::from_raw_parts_mut(self.addr.as_mut_ptr(), PAGE_SIZE_4K) }
    }
}

impl Drop for PageCache {
    fn drop(&mut self) {
        if self.dirty {
            warn!("dirty page dropped without flushing");
        }
        global_allocator().dealloc_pages(self.addr.as_usize(), 1, UsageKind::PageCache);
    }
}

struct CachedPageSlot {
    state: AtomicU8,
    waiters: WaitQueue,
    page: Mutex<Option<PageCache>>,
}

impl CachedPageSlot {
    fn new_loading() -> Self {
        Self {
            state: AtomicU8::new(PAGE_STATE_LOADING),
            waiters: WaitQueue::new(),
            page: Mutex::new(None),
        }
    }

    fn is_ready(&self) -> bool {
        self.state.load(Ordering::Acquire) == PAGE_STATE_READY
    }

    fn wait_ready(&self) {
        self.waiters
            .wait_until(|| self.state.load(Ordering::Acquire) != PAGE_STATE_LOADING);
    }

    fn publish(&self, page: PageCache) {
        *self.page.lock() = Some(page);
        self.state.store(PAGE_STATE_READY, Ordering::Release);
        self.waiters.notify_all(false);
    }

    fn fail(&self) {
        *self.page.lock() = None;
        self.state.store(PAGE_STATE_FAILED, Ordering::Release);
        self.waiters.notify_all(false);
    }

    fn with_ready_page<R>(&self, f: impl FnOnce(&mut PageCache) -> R) -> R {
        let mut guard = self.page.lock();
        f(guard.as_mut().expect("ready page slot must be populated"))
    }

    fn with_page<R>(&self, f: impl FnOnce(Option<&mut PageCache>) -> R) -> R {
        let mut guard = self.page.lock();
        f(guard.as_mut())
    }
}

type EvictListenerFn = dyn Fn(PageIndex, &PageCache) + Send + Sync;

struct EvictListener {
    listener: Box<EvictListenerFn>,
    link: LinkedListAtomicLink,
}

intrusive_adapter!(EvictListenerAdapter = Box<EvictListener>: EvictListener { link: LinkedListAtomicLink });

pub(super) struct FileMapping {
    page_cache: Mutex<LruCache<PageIndex, Arc<CachedPageSlot>>>,
    evict_listeners: Mutex<LinkedList<EvictListenerAdapter>>,
}

impl FileMapping {
    pub fn new() -> Self {
        Self {
            page_cache: Mutex::new(LruCache::new(NonZeroUsize::new(64).unwrap())),
            evict_listeners: Mutex::new(LinkedList::default()),
        }
    }

    pub fn new_unbounded() -> Self {
        Self {
            page_cache: Mutex::new(LruCache::unbounded()),
            evict_listeners: Mutex::new(LinkedList::default()),
        }
    }

    pub fn add_evict_listener<F>(self: &Arc<Self>, listener: F) -> EvictRegistration
    where
        F: Fn(PageIndex, &PageCache) + Send + Sync + 'static,
    {
        let pointer = Box::new(EvictListener {
            listener: Box::new(listener),
            link: LinkedListAtomicLink::new(),
        });
        let listener_ptr = NonNull::from(pointer.as_ref());
        self.evict_listeners.lock().push_back(pointer);
        EvictRegistration {
            mapping: Arc::downgrade(self),
            listener_ptr,
        }
    }

    fn remove_evict_listener(&self, listener_ptr: NonNull<EvictListener>) {
        let mut guard = self.evict_listeners.lock();
        // SAFETY: `listener_ptr` is created only from an `EvictListener`
        // allocated by `add_evict_listener` and is kept private inside
        // `EvictRegistration`. Dropping the registration consumes it exactly
        // once.
        let mut cursor = unsafe { guard.cursor_mut_from_ptr(listener_ptr.as_ptr()) };
        cursor.remove();
    }

    fn page_start(pn: PageIndex) -> VfsResult<u64> {
        pn.checked_mul(PAGE_SIZE_4K as u64)
            .ok_or(VfsError::InvalidInput)
    }

    fn writeback_page(
        &self,
        file: &FileNode,
        pn: PageIndex,
        page: &mut PageCache,
    ) -> VfsResult<()> {
        if page.dirty {
            let page_start = Self::page_start(pn)?;
            let len = (file.len()?.saturating_sub(page_start)).min(PAGE_SIZE_4K as u64) as usize;
            if len > 0 {
                file.write_at(&page.data()[..len], page_start)?;
            }
            page.dirty = false;
        }
        Ok(())
    }

    fn evict_cache(&self, file: &FileNode, pn: PageIndex, page: &mut PageCache) -> VfsResult<()> {
        for listener in self.evict_listeners.lock().iter() {
            (listener.listener)(pn, page);
        }
        self.writeback_page(file, pn, page)
    }

    fn evict_slot(&self, file: &FileNode, pn: PageIndex, slot: &CachedPageSlot) -> VfsResult<()> {
        slot.with_ready_page(|page| self.evict_cache(file, pn, page))
    }

    fn pop_ready_slot(
        guard: &mut LruCache<PageIndex, Arc<CachedPageSlot>>,
    ) -> Option<(PageIndex, Arc<CachedPageSlot>)> {
        let pn = guard
            .iter()
            .find_map(|(pn, slot)| slot.is_ready().then_some(*pn))?;
        guard.pop(&pn).map(|slot| (pn, slot))
    }

    fn load_page(&self, file: &FileNode, in_memory: bool, pn: PageIndex) -> VfsResult<PageCache> {
        let mut page = PageCache::new()?;
        // Always zero-fill first so short reads leave a defined zero tail.
        // This is required for mmap semantics: bytes past EOF in the last
        // mapped page must read as zero.
        page.data().fill(0);
        if !in_memory {
            file.read_at(page.data(), Self::page_start(pn)?)?;
        }
        Ok(page)
    }

    fn rollback_loading_slot(
        &self,
        pn: PageIndex,
        slot: &Arc<CachedPageSlot>,
        evicted: Option<(PageIndex, Arc<CachedPageSlot>)>,
    ) {
        let mut guard = self.page_cache.lock();
        if guard
            .get(&pn)
            .is_some_and(|current| Arc::ptr_eq(current, slot))
        {
            guard.pop(&pn);
        }
        if let Some((evicted_pn, evicted_slot)) = evicted
            && guard.get(&evicted_pn).is_none()
            && guard.len() < guard.cap().get()
        {
            guard.put(evicted_pn, evicted_slot);
        }
        drop(guard);
        slot.fail();
    }

    fn ensure_page_ready(
        &self,
        file: &FileNode,
        in_memory: bool,
        pn: PageIndex,
    ) -> VfsResult<(Arc<CachedPageSlot>, Vec<PageIndex>)> {
        loop {
            let mut created = false;
            let mut evicted = None;
            let slot = {
                let mut guard = self.page_cache.lock();
                if let Some(slot) = guard.get(&pn) {
                    slot.clone()
                } else {
                    let slot = Arc::new(CachedPageSlot::new_loading());
                    if guard.len() >= guard.cap().get() {
                        evicted = Self::pop_ready_slot(&mut guard);
                        if evicted.is_none() {
                            let wait_slot = guard.iter().next().map(|(_, slot)| slot.clone());
                            drop(guard);
                            if let Some(wait_slot) = wait_slot {
                                wait_slot.wait_ready();
                            }
                            continue;
                        }
                    }
                    guard.put(pn, slot.clone());
                    created = true;
                    slot
                }
            };

            if !created {
                slot.wait_ready();
                if slot.is_ready() {
                    return Ok((slot, Vec::new()));
                }
                continue;
            }

            if let Some((evicted_pn, evicted_slot)) = evicted.as_ref()
                && let Err(err) = self.evict_slot(file, *evicted_pn, evicted_slot)
            {
                self.rollback_loading_slot(pn, &slot, evicted);
                return Err(err);
            }

            match self.load_page(file, in_memory, pn) {
                Ok(page) => {
                    slot.publish(page);
                    let evicted_pns = evicted.into_iter().map(|(pn, _)| pn).collect();
                    return Ok((slot, evicted_pns));
                }
                Err(err) => {
                    self.rollback_loading_slot(pn, &slot, evicted);
                    return Err(err);
                }
            }
        }
    }

    pub fn with_page<R>(&self, pn: PageIndex, f: impl FnOnce(Option<&mut PageCache>) -> R) -> R {
        let slot = self.page_cache.lock().get(&pn).cloned();
        if let Some(slot) = slot {
            if !slot.is_ready() {
                slot.wait_ready();
            }
            if slot.is_ready() {
                return slot.with_page(f);
            }
        }
        f(None)
    }

    pub fn with_page_or_insert<R>(
        &self,
        file: &FileNode,
        in_memory: bool,
        pn: PageIndex,
        f: impl FnOnce(&mut PageCache, Vec<PageIndex>) -> VfsResult<R>,
    ) -> VfsResult<R> {
        let (slot, evicted) = self.ensure_page_ready(file, in_memory, pn)?;
        slot.with_ready_page(|page| f(page, evicted))
    }

    pub fn set_len(
        &self,
        file: &FileNode,
        in_memory: bool,
        old_len: u64,
        len: u64,
    ) -> VfsResult<()> {
        let page_size_u64 = PAGE_SIZE_4K as u64;
        let old_last_page = if old_len == 0 {
            None
        } else {
            Some((old_len - 1) / page_size_u64)
        };
        let new_last_page = if len == 0 {
            None
        } else {
            Some((len - 1) / page_size_u64)
        };

        if old_len < len {
            if let Some(old_pn) = old_last_page
                && let Some(slot) = self.page_cache.lock().get(&old_pn).cloned()
                && slot.is_ready()
            {
                let page_start = Self::page_start(old_pn)?;
                let old_page_offset = (old_len - page_start) as usize;
                let new_page_offset = (len - page_start).min(page_size_u64) as usize;
                slot.with_ready_page(|page| {
                    if old_page_offset < new_page_offset {
                        page.data()[old_page_offset..new_page_offset].fill(0);
                    }
                });
            }
        } else if old_len > len {
            self.truncate_cached_pages(file, in_memory, len, new_last_page)?;
        }
        Ok(())
    }

    fn truncate_cached_pages(
        &self,
        file: &FileNode,
        in_memory: bool,
        len: u64,
        new_last_page: Option<PageIndex>,
    ) -> VfsResult<()> {
        let page_size_u64 = PAGE_SIZE_4K as u64;
        let mut guard = self.page_cache.lock();

        if let Some(new_pn) = new_last_page {
            let tail_off = (len % page_size_u64) as usize;
            if tail_off != 0
                && let Some(slot) = guard.get(&new_pn).cloned()
            {
                drop(guard);
                if slot.is_ready() {
                    slot.with_ready_page(|page| {
                        page.data()[tail_off..].fill(0);
                        if !in_memory {
                            page.dirty = true;
                        }
                    });
                }
                guard = self.page_cache.lock();
            }
        }

        let keys = guard
            .iter()
            .map(|(k, _)| *k)
            .filter(|pn| match new_last_page {
                Some(last) => *pn > last,
                None => true,
            })
            .collect::<Vec<_>>();

        for pn in keys {
            if let Some(slot) = guard.pop(&pn)
                && !in_memory
            {
                if !slot.is_ready() {
                    drop(guard);
                    slot.wait_ready();
                    guard = self.page_cache.lock();
                }
                if !slot.is_ready() {
                    continue;
                }
                // Don't write back pages since they're beyond new EOF.
                slot.with_ready_page(|page| {
                    page.dirty = false;
                });
                self.evict_slot(file, pn, &slot)?;
            }
        }
        Ok(())
    }

    pub fn flush_and_evict_from(
        &self,
        file: &FileNode,
        in_memory: bool,
        offset: u64,
    ) -> VfsResult<()> {
        if in_memory {
            return Ok(());
        }
        let start_pn = offset / PAGE_SIZE_4K as u64;
        let mut guard = self.page_cache.lock();

        let keys = guard
            .iter()
            .map(|(k, _)| *k)
            .filter(|pn| *pn >= start_pn)
            .collect::<Vec<_>>();

        for pn in keys {
            if let Some(slot) = guard.pop(&pn)
                && let Err(e) = self.evict_slot(file, pn, &slot)
            {
                guard.push(pn, slot);
                return Err(e);
            }
        }
        file.sync(false)?;
        Ok(())
    }

    pub fn sync(&self, file: &FileNode, in_memory: bool, data_only: bool) -> VfsResult<()> {
        if in_memory {
            return Ok(());
        }
        let slots = self
            .page_cache
            .lock()
            .iter()
            .map(|(pn, slot)| (*pn, slot.clone()))
            .collect::<Vec<_>>();
        for (pn, slot) in slots {
            if !slot.is_ready() {
                slot.wait_ready();
            }
            if slot.is_ready() {
                slot.with_ready_page(|page| self.writeback_page(file, pn, page))?;
            }
        }
        file.sync(data_only)?;
        Ok(())
    }
}

pub(super) struct FileMappingAddressSpaceOperations {
    mapping: Arc<FileMapping>,
    location: Location,
    in_memory: bool,
}

impl FileMappingAddressSpaceOperations {
    pub fn new(mapping: Arc<FileMapping>, location: Location, in_memory: bool) -> Self {
        Self {
            mapping,
            location,
            in_memory,
        }
    }
}

impl AddressSpaceOperations for FileMappingAddressSpaceOperations {
    fn read_page(&self, page_index: u64, page: &mut [u8]) -> VfsResult<usize> {
        let len = page.len().min(PAGE_SIZE_4K);
        let page = &mut page[..len];
        page.fill(0);
        if self.in_memory {
            return Ok(0);
        }
        self.location
            .entry()
            .as_file()?
            .read_at(page, FileMapping::page_start(page_index)?)
    }

    fn write_page(&self, page_index: u64, page: &[u8]) -> VfsResult<usize> {
        if self.in_memory {
            return Ok(0);
        }
        let page = &page[..page.len().min(PAGE_SIZE_4K)];
        self.location
            .entry()
            .as_file()?
            .write_at(page, FileMapping::page_start(page_index)?)
    }

    fn writepages(&self, data_only: bool) -> VfsResult<()> {
        let file = self.location.entry().as_file()?;
        self.mapping.sync(file, self.in_memory, data_only)
    }

    fn invalidate_from(&self, page_index: u64) -> VfsResult<()> {
        let file = self.location.entry().as_file()?;
        self.mapping.flush_and_evict_from(
            file,
            self.in_memory,
            FileMapping::page_start(page_index)?,
        )
    }
}

/// RAII registration for a file-mapping eviction listener.
#[must_use = "dropping EvictRegistration unregisters the eviction listener"]
pub struct EvictRegistration {
    mapping: Weak<FileMapping>,
    listener_ptr: NonNull<EvictListener>,
}

// SAFETY: `EvictRegistration` never dereferences `listener_ptr` directly. The
// pointer is only used while holding the owning `FileMapping` listener-list
// lock, and the listener allocation remains owned by that list until the guard
// is dropped.
unsafe impl Send for EvictRegistration {}

impl Drop for EvictRegistration {
    fn drop(&mut self) {
        if let Some(mapping) = self.mapping.upgrade() {
            mapping.remove_evict_listener(self.listener_ptr);
        }
    }
}

pub(super) struct FileMappingData {
    mapping: Arc<FileMapping>,
}

impl FileMappingData {
    pub fn new(mapping: Arc<FileMapping>) -> Self {
        Self { mapping }
    }

    pub fn mapping(&self) -> Arc<FileMapping> {
        self.mapping.clone()
    }
}

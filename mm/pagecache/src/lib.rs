// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Page-cache storage and algorithms.
//!
//! [`PageCache`] is an implementation component owned by the VFS inode
//! address space. It is not a second Linux `address_space` analogue and must
//! not be held directly by VMA or open-file runtimes.
#![no_std]

extern crate alloc;

use alloc::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    vec::Vec,
};

use kalloc::{UsageKind, global_allocator};
use kerrno::{KError, KResult};
use khal::mem::{PhysAddr, VirtAddr, v2p};
use ksync::{Mutex, MutexGuard};
use log::warn;
use memaddr::PAGE_SIZE_4K;

/// Page index within a mapping.
pub type PageIndex = u64;

type IndexedFolio = (PageIndex, Arc<Mutex<Folio>>);

/// Result counters for one writeback pass.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WritebackStats {
    /// Dirty folios successfully written and cleaned.
    pub pages_written: usize,
    /// Dirty folios left for a later writeback pass.
    pub pages_skipped: usize,
}

impl WritebackStats {
    fn wrote(&mut self, pages: usize) {
        self.pages_written = self.pages_written.saturating_add(pages);
    }

    fn skipped(&mut self, pages: usize) {
        self.pages_skipped = self.pages_skipped.saturating_add(pages);
    }
}

/// Page-cache storage class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageCacheKind {
    /// tmpfs/shmem-style in-memory object.
    InMemory,
    /// Regular inode-backed file object.
    FileBacked,
}

struct PageCacheInner {
    pages: BTreeMap<PageIndex, Arc<Mutex<Folio>>>,
    // Rust's `BTreeMap` has no per-entry marks, so this derived index carries
    // the enumeration role of Linux `PAGECACHE_TAG_DIRTY`. `Folio::dirty`
    // remains the authoritative dirty state.
    dirty_pages: BTreeSet<PageIndex>,
}

/// Cache storage owned by one inode address space.
pub struct PageCache {
    kind: PageCacheKind,
    inner: Mutex<PageCacheInner>,
}

impl PageCache {
    /// Creates cache storage with source-specific materialization operations.
    pub fn new(kind: PageCacheKind) -> Arc<Self> {
        Arc::new(Self {
            kind,
            inner: Mutex::new(PageCacheInner {
                pages: BTreeMap::new(),
                dirty_pages: BTreeSet::new(),
            }),
        })
    }

    fn warn_on_dirty_drop(&self) -> bool {
        matches!(self.kind, PageCacheKind::FileBacked)
    }

    /// Returns the mapping kind.
    pub const fn kind(&self) -> PageCacheKind {
        self.kind
    }

    /// Returns `address_space::nrpages`.
    pub fn nrpages(&self) -> u64 {
        self.inner.lock().pages.len() as u64
    }

    fn reconcile_dirty_folio(&self, index: PageIndex, folio: &Arc<Mutex<Folio>>) {
        let mut inner = self.inner.lock();
        let is_current = inner
            .pages
            .get(&index)
            .is_some_and(|current| Arc::ptr_eq(current, folio));
        if !is_current {
            return;
        }
        if folio.lock().is_dirty() {
            inner.dirty_pages.insert(index);
        } else {
            inner.dirty_pages.remove(&index);
        }
    }

    fn reconcile_dirty_folios(&self, folios: &[IndexedFolio]) {
        let mut inner = self.inner.lock();
        for (index, folio) in folios {
            let is_current = inner
                .pages
                .get(index)
                .is_some_and(|current| Arc::ptr_eq(current, folio));
            if !is_current {
                continue;
            }
            if folio.lock().is_dirty() {
                inner.dirty_pages.insert(*index);
            } else {
                inner.dirty_pages.remove(index);
            }
        }
    }

    /// Runs `f` with the cached folio at `index`, if present.
    ///
    /// Dirty-state changes made by `f` are reflected in the cache's writeback
    /// index before this method returns.
    pub fn with_folio<R>(&self, index: PageIndex, f: impl FnOnce(Option<&mut Folio>) -> R) -> R {
        let folio = self.inner.lock().pages.get(&index).cloned();
        if let Some(folio) = folio {
            let (result, needs_dirty_reconciliation) = {
                let mut locked = folio.lock();
                let was_dirty = locked.is_dirty();
                let result = f(Some(&mut locked));
                let is_dirty = locked.is_dirty();
                (result, was_dirty != is_dirty)
            };
            if needs_dirty_reconciliation {
                self.reconcile_dirty_folio(index, &folio);
            }
            result
        } else {
            f(None)
        }
    }

    /// Returns the number of contiguous cached folios starting at `start`.
    pub fn cached_run_len(&self, start: PageIndex, count: usize) -> usize {
        if count == 0 {
            return 0;
        }
        let inner = self.inner.lock();
        let mut found = 0usize;
        for index in start..start.saturating_add(count as u64) {
            if !inner.pages.contains_key(&index) {
                break;
            }
            found += 1;
        }
        found
    }

    /// Runs `f` with a folio at `index`, materializing one on demand.
    ///
    /// Dirty-state changes made by `f` are reflected in the cache's writeback
    /// index before this method returns.
    pub fn with_folio_or_create<R>(
        &self,
        index: PageIndex,
        instantiate_folio: impl FnOnce(PageIndex) -> KResult<Folio>,
        f: impl FnOnce(&mut Folio) -> KResult<R>,
    ) -> KResult<R> {
        let folio = {
            let mut inner = self.inner.lock();
            if let Some(folio) = inner.pages.get(&index) {
                folio.clone()
            } else {
                let mut folio = instantiate_folio(index)?;
                folio.set_warn_on_dirty_drop(self.warn_on_dirty_drop());
                let folio = Arc::new(Mutex::new(folio));
                inner.pages.insert(index, folio.clone());
                folio
            }
        };
        let (result, needs_dirty_reconciliation) = {
            let mut locked = folio.lock();
            let was_dirty = locked.is_dirty();
            let result = f(&mut locked);
            let is_dirty = locked.is_dirty();
            (result, was_dirty != is_dirty)
        };
        if needs_dirty_reconciliation {
            self.reconcile_dirty_folio(index, &folio);
        }
        result
    }

    /// Adds one page-cache folio from backing-store data if it is still absent.
    ///
    /// This is the page-cache insertion side used by readahead after the
    /// backing filesystem has already filled a byte range.
    pub fn filemap_add_folio(&self, index: PageIndex, offset: usize, src: &[u8]) -> KResult<bool> {
        let end = offset.checked_add(src.len()).ok_or(KError::InvalidInput)?;
        if end > PAGE_SIZE_4K {
            return Err(KError::InvalidInput);
        }
        let mut folio = Folio::new_zeroed()?;
        folio.data()[offset..end].copy_from_slice(src);
        folio.set_warn_on_dirty_drop(self.warn_on_dirty_drop());
        let mut inner = self.inner.lock();
        if inner.pages.contains_key(&index) {
            return Ok(false);
        }
        inner.pages.insert(index, Arc::new(Mutex::new(folio)));
        Ok(true)
    }

    /// Updates cached folios for a change in the owning inode's visible size.
    pub fn resize_cached_folios(&self, old_len: u64, len: u64) {
        let mut inner = self.inner.lock();
        if len == old_len {
            return;
        }

        if len >= old_len {
            if len > old_len {
                self.zero_growth_tail(&mut inner, old_len, len);
            }
            return;
        }

        let first_truncated = len.div_ceil(PAGE_SIZE_4K as u64);
        let dropped_folios = inner
            .pages
            .range(first_truncated..)
            .map(|(index, folio)| (*index, folio.clone()))
            .collect::<Vec<_>>();
        // Explicit truncation owns the data discard, so clear dirty before
        // eviction rather than requiring writeback.
        for (_, folio) in &dropped_folios {
            folio.lock().clear_dirty();
        }
        inner.pages.retain(|index, _| *index < first_truncated);
        inner.dirty_pages.retain(|index| *index < first_truncated);

        let tail_index = len / PAGE_SIZE_4K as u64;
        let tail_off = (len % PAGE_SIZE_4K as u64) as usize;
        if tail_off != 0
            && let Some(folio) = inner.pages.get(&tail_index).cloned()
        {
            let mut folio = folio.lock();
            folio.data()[tail_off..].fill(0);
            folio.mark_dirty();
            drop(folio);
            inner.dirty_pages.insert(tail_index);
        }
    }

    fn collect_dirty_folios_in_range(&self, start: u64, end: u64) -> Vec<IndexedFolio> {
        let inner = self.inner.lock();
        if start >= end {
            return Vec::new();
        }
        let first_index = start / PAGE_SIZE_4K as u64;
        let last_index = (end - 1) / PAGE_SIZE_4K as u64;
        inner
            .dirty_pages
            .range(first_index..=last_index)
            .filter_map(|index| inner.pages.get(index).map(|folio| (*index, folio.clone())))
            .collect()
    }

    // A dirty tag only selects a candidate. Mapping identity and folio state
    // must be checked in one inner -> folio critical section before I/O owns
    // the folio, otherwise truncate or replacement can detach a collected Arc.
    fn start_writeback_for_candidate<'a>(
        &self,
        index: PageIndex,
        candidate: &'a Arc<Mutex<Folio>>,
        stats: &mut WritebackStats,
    ) -> Option<MutexGuard<'a, Folio>> {
        let mut inner = self.inner.lock();
        let Some(current) = inner.pages.get(&index).cloned() else {
            inner.dirty_pages.remove(&index);
            return None;
        };

        if !Arc::ptr_eq(&current, candidate) {
            let is_current_dirty = current.lock().is_dirty();
            if is_current_dirty {
                inner.dirty_pages.insert(index);
            } else {
                inner.dirty_pages.remove(&index);
            }
            return None;
        }

        let mut folio = candidate.lock();
        if !folio.is_dirty() {
            inner.dirty_pages.remove(&index);
            return None;
        }
        if folio.is_under_writeback() {
            stats.skipped(1);
            return None;
        }

        folio.clear_dirty();
        folio.start_writeback();
        inner.dirty_pages.remove(&index);
        drop(inner);
        Some(folio)
    }

    /// Writes back dirty cached folios intersecting `[start, end)`.
    ///
    /// The callback runs with the candidate folio locked and must not re-enter
    /// this [`PageCache`].
    pub fn writeback_until(
        &self,
        visible_len: u64,
        start: u64,
        end: u64,
        max_pages: usize,
        write_folio_fn: &mut impl FnMut(PageIndex, &[u8], usize) -> KResult<()>,
    ) -> KResult<WritebackStats> {
        let mut stats = WritebackStats::default();
        if max_pages == 0 {
            return Ok(stats);
        }
        let folios = self.collect_dirty_folios_in_range(start, end);

        for (index, folio) in folios {
            if stats.pages_written >= max_pages {
                break;
            }
            let page_start = index
                .checked_mul(PAGE_SIZE_4K as u64)
                .ok_or(KError::InvalidInput)?;
            let valid_len = visible_len
                .saturating_sub(page_start)
                .min(PAGE_SIZE_4K as u64) as usize;
            let Some(mut locked) = self.start_writeback_for_candidate(index, &folio, &mut stats)
            else {
                continue;
            };
            if let Err(error) = write_folio_fn(index, &locked.data()[..valid_len], valid_len) {
                locked.mark_dirty();
                locked.end_writeback();
                drop(locked);
                self.reconcile_dirty_folio(index, &folio);
                return Err(error);
            }
            locked.end_writeback();
            drop(locked);
            stats.wrote(1);
        }
        Ok(stats)
    }

    /// Writes back dirty cached folios in contiguous batches.
    pub fn write_cache_pages(
        &self,
        visible_len: u64,
        start: u64,
        end: u64,
        max_pages: usize,
        max_bytes: usize,
        write_range_fn: &mut impl FnMut(u64, &[u8]) -> KResult<()>,
    ) -> KResult<WritebackStats> {
        let mut stats = WritebackStats::default();
        if max_pages == 0 {
            return Ok(stats);
        }
        let folios = self.collect_dirty_folios_in_range(start, end);

        let max_bytes = max_bytes.max(PAGE_SIZE_4K);
        let mut batch_start = 0u64;
        let mut next_index = None;
        let mut batch_data = Vec::new();
        let mut batch_folios = Vec::new();

        let flush_batch = |batch_start: u64,
                           batch_data: &mut Vec<u8>,
                           batch_folios: &mut Vec<IndexedFolio>,
                           write_range_fn: &mut dyn FnMut(u64, &[u8]) -> KResult<()>,
                           stats: &mut WritebackStats|
         -> KResult<()> {
            if batch_data.is_empty() {
                return Ok(());
            }
            let result = write_range_fn(batch_start, batch_data);
            if result.is_ok() {
                stats.wrote(batch_folios.len());
            }
            for (_, folio) in batch_folios.iter() {
                let mut folio = folio.lock();
                if result.is_err() {
                    folio.mark_dirty();
                }
                folio.end_writeback();
            }
            self.reconcile_dirty_folios(batch_folios);
            batch_folios.clear();
            batch_data.clear();
            result?;
            Ok(())
        };

        for (index, folio) in folios {
            if stats.pages_written >= max_pages {
                break;
            }
            let page_start = index
                .checked_mul(PAGE_SIZE_4K as u64)
                .ok_or(KError::InvalidInput)?;
            let valid_len = visible_len
                .saturating_sub(page_start)
                .min(PAGE_SIZE_4K as u64) as usize;
            if valid_len == 0 {
                continue;
            }

            let contiguous = next_index == Some(index);
            let would_overflow = batch_data.len().saturating_add(valid_len) > max_bytes;
            let would_exceed_pages =
                stats.pages_written.saturating_add(batch_folios.len()) >= max_pages;
            if !batch_data.is_empty() && (!contiguous || would_overflow || would_exceed_pages) {
                flush_batch(
                    batch_start,
                    &mut batch_data,
                    &mut batch_folios,
                    write_range_fn,
                    &mut stats,
                )?;
                if stats.pages_written >= max_pages {
                    break;
                }
            }

            let Some(mut locked) = self.start_writeback_for_candidate(index, &folio, &mut stats)
            else {
                flush_batch(
                    batch_start,
                    &mut batch_data,
                    &mut batch_folios,
                    write_range_fn,
                    &mut stats,
                )?;
                if stats.pages_written >= max_pages {
                    break;
                }
                next_index = None;
                continue;
            };

            if batch_data.is_empty() {
                batch_start = page_start;
            }
            batch_data.extend_from_slice(&locked.data()[..valid_len]);
            drop(locked);
            batch_folios.push((index, folio));
            next_index = Some(index + 1);
        }

        flush_batch(
            batch_start,
            &mut batch_data,
            &mut batch_folios,
            write_range_fn,
            &mut stats,
        )?;
        Ok(stats)
    }

    /// Drops all cached folios during final mapping teardown.
    ///
    /// Unlike ordinary invalidation, final teardown may discard dirty folios
    /// because no later backing-store writeback is possible.
    pub fn truncate_final(&self) -> KResult<()> {
        let dropped_folios = {
            let mut inner = self.inner.lock();
            let dropped = inner
                .pages
                .iter()
                .map(|(index, folio)| (*index, folio.clone()))
                .collect::<Vec<_>>();
            inner.pages.clear();
            inner.dirty_pages.clear();
            dropped
        };
        for (_, folio) in &dropped_folios {
            folio.lock().clear_dirty();
        }
        Ok(())
    }

    fn zero_growth_tail(&self, inner: &mut PageCacheInner, old_len: u64, new_len: u64) {
        if old_len == 0 {
            return;
        }
        let tail_index = (old_len - 1) / PAGE_SIZE_4K as u64;
        let Some(folio) = inner.pages.get(&tail_index).cloned() else {
            return;
        };
        let page_start = tail_index * PAGE_SIZE_4K as u64;
        let old_off = (old_len - page_start) as usize;
        let new_off = (new_len - page_start).min(PAGE_SIZE_4K as u64) as usize;
        if old_off >= new_off {
            return;
        }
        let mut folio = folio.lock();
        folio.data()[old_off..new_off].fill(0);
        folio.mark_dirty();
        drop(folio);
        inner.dirty_pages.insert(tail_index);
    }
}

/// Cached folio stored inside a [`PageCache`].
#[derive(Debug)]
pub struct Folio {
    addr: VirtAddr,
    dirty: bool,
    writeback: bool,
    warn_on_dirty_drop: bool,
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
        Ok(Self {
            addr,
            dirty: false,
            writeback: false,
            warn_on_dirty_drop: false,
        })
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

    /// Returns whether a synchronous writeback pass is writing this folio.
    pub fn is_under_writeback(&self) -> bool {
        self.writeback
    }

    fn start_writeback(&mut self) {
        self.writeback = true;
    }

    fn end_writeback(&mut self) {
        self.writeback = false;
    }

    /// Clears the dirty bit after successful writeback or truncation cleanup.
    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    fn set_warn_on_dirty_drop(&mut self, enabled: bool) {
        self.warn_on_dirty_drop = enabled;
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
        if self.dirty && self.warn_on_dirty_drop {
            warn!("dropping dirty file-backed folio without writeback");
        }
        global_allocator().dealloc_pages(self.addr.as_usize(), 1, UsageKind::PageCache);
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::{sync::Arc, vec, vec::Vec};

    use kerrno::{KError, KResult};
    use ksync::Mutex;
    use unittest::{assert_eq, def_test};

    use super::{Folio, PAGE_SIZE_4K, PageCache, PageCacheKind, PageIndex};

    fn new_cache(kind: PageCacheKind) -> Arc<PageCache> {
        PageCache::new(kind)
    }

    fn dirty_indices(cache: &PageCache) -> Vec<PageIndex> {
        cache.inner.lock().dirty_pages.iter().copied().collect()
    }

    fn write_cached(cache: &PageCache, offset: u64, src: &[u8]) -> KResult<()> {
        let mut written = 0usize;
        while written < src.len() {
            let pos = offset + written as u64;
            let index = pos / PAGE_SIZE_4K as u64;
            let page_off = (pos % PAGE_SIZE_4K as u64) as usize;
            let step = (src.len() - written).min(PAGE_SIZE_4K - page_off);
            cache.with_folio_or_create(
                index,
                |_| Folio::new_zeroed(),
                |folio| {
                    folio.data()[page_off..page_off + step]
                        .copy_from_slice(&src[written..written + step]);
                    folio.mark_dirty();
                    Ok(())
                },
            )?;
            written += step;
        }
        Ok(())
    }

    #[def_test]
    fn cached_folio_roundtrip() {
        let cache = new_cache(PageCacheKind::InMemory);
        write_cached(&cache, 0, b"hello").unwrap();

        let observed = cache.with_folio(0, |folio| {
            let folio = folio.expect("cached folio");
            let observed = folio.data()[..5].to_vec();
            folio.clear_dirty();
            observed
        });
        assert_eq!(observed.as_slice(), b"hello");
        assert!(dirty_indices(&cache).is_empty());
    }

    #[def_test]
    fn cache_storage_does_not_own_visible_length() {
        let cache = new_cache(PageCacheKind::InMemory);
        write_cached(&cache, PAGE_SIZE_4K as u64, b"x").unwrap();

        assert_eq!(cache.nrpages(), 1);
        cache.resize_cached_folios((PAGE_SIZE_4K * 2) as u64, 0);
        assert_eq!(cache.nrpages(), 0);
    }

    #[def_test]
    fn shrink_drops_full_folios_and_zeros_surviving_tail() {
        let cache = new_cache(PageCacheKind::InMemory);
        write_cached(&cache, 0, &vec![0x5a; PAGE_SIZE_4K * 2]).unwrap();

        let new_len = (PAGE_SIZE_4K / 2) as u64;
        cache.resize_cached_folios((PAGE_SIZE_4K * 2) as u64, new_len);

        assert_eq!(cache.nrpages(), 1);
        assert_eq!(dirty_indices(&cache), vec![0]);
        let tail_is_zero = cache.with_folio(0, |folio| {
            let folio = folio.expect("surviving folio");
            let tail_is_zero = folio.data()[PAGE_SIZE_4K / 2..]
                .iter()
                .all(|byte| *byte == 0);
            folio.clear_dirty();
            tail_is_zero
        });
        assert!(tail_is_zero);
        assert!(dirty_indices(&cache).is_empty());
    }

    #[def_test]
    fn growth_zeros_cached_tail_and_marks_it_dirty() {
        let cache = new_cache(PageCacheKind::FileBacked);
        cache
            .filemap_add_folio(0, 0, &vec![0x5a; PAGE_SIZE_4K])
            .unwrap();
        let old_len = (PAGE_SIZE_4K / 4) as u64;
        let new_len = (PAGE_SIZE_4K / 2) as u64;
        assert!(dirty_indices(&cache).is_empty());

        cache.resize_cached_folios(old_len, new_len);

        assert_eq!(dirty_indices(&cache), vec![0]);
        let (is_dirty, growth_range_is_zero) = cache.with_folio(0, |folio| {
            let folio = folio.expect("grown tail folio");
            let state = (
                folio.is_dirty(),
                folio.data()[old_len as usize..new_len as usize]
                    .iter()
                    .all(|byte| *byte == 0),
            );
            folio.clear_dirty();
            state
        });
        assert!(is_dirty);
        assert!(growth_range_is_zero);
        assert!(dirty_indices(&cache).is_empty());
    }

    #[def_test]
    fn writeback_uses_owner_supplied_visible_length() {
        let cache = new_cache(PageCacheKind::FileBacked);
        let visible_len = PAGE_SIZE_4K as u64 + 123;
        write_cached(&cache, 0, &vec![0x33; visible_len as usize]).unwrap();
        let mut writes = Vec::new();

        cache
            .writeback_until(
                visible_len,
                0,
                u64::MAX,
                usize::MAX,
                &mut |index, _data, valid_len| {
                    writes.push((index, valid_len));
                    Ok(())
                },
            )
            .unwrap();

        assert_eq!(writes, vec![(0, PAGE_SIZE_4K), (1, 123)]);
        assert!(dirty_indices(&cache).is_empty());
        for index in 0..2 {
            let state = cache.with_folio(index, |folio| {
                let folio = folio.expect("written folio");
                (folio.is_dirty(), folio.is_under_writeback())
            });
            assert_eq!(state, (false, false));
        }
    }

    #[def_test]
    fn failed_writeback_restores_dirty_state() {
        let cache = new_cache(PageCacheKind::FileBacked);
        write_cached(&cache, 0, b"dirty").unwrap();

        assert_eq!(
            cache.writeback_until(5, 0, u64::MAX, 1, &mut |_, _, _| {
                Err(KError::InvalidInput)
            }),
            Err(KError::InvalidInput)
        );
        assert_eq!(dirty_indices(&cache), vec![0]);
        let is_dirty = cache.with_folio(0, |folio| {
            let folio = folio.expect("failed-writeback folio");
            let is_dirty = folio.is_dirty();
            folio.clear_dirty();
            is_dirty
        });
        assert!(is_dirty);
        assert!(dirty_indices(&cache).is_empty());
    }

    #[def_test]
    fn clean_folio_with_stale_dirty_tag_is_not_written() {
        let cache = new_cache(PageCacheKind::FileBacked);
        cache.filemap_add_folio(0, 0, b"clean").unwrap();
        cache.inner.lock().dirty_pages.insert(0);
        let mut writes = 0usize;

        let stats = cache
            .writeback_until(PAGE_SIZE_4K as u64, 0, u64::MAX, 1, &mut |_, _, _| {
                writes += 1;
                Ok(())
            })
            .unwrap();

        assert_eq!(writes, 0);
        assert_eq!(stats.pages_written, 0);
        assert_eq!(stats.pages_skipped, 0);
        assert!(dirty_indices(&cache).is_empty());
    }

    #[def_test]
    fn writeback_until_respects_dirty_index_and_page_budget() {
        let cache = new_cache(PageCacheKind::FileBacked);
        write_cached(&cache, 0, &vec![1; PAGE_SIZE_4K]).unwrap();
        write_cached(&cache, PAGE_SIZE_4K as u64, &vec![2; PAGE_SIZE_4K]).unwrap();
        assert_eq!(dirty_indices(&cache), vec![0, 1]);

        let mut writes = Vec::new();
        let stats = cache
            .writeback_until(
                (PAGE_SIZE_4K * 2) as u64,
                0,
                u64::MAX,
                1,
                &mut |index, _data, valid_len| {
                    writes.push((index, valid_len));
                    Ok(())
                },
            )
            .unwrap();

        assert_eq!(stats.pages_written, 1);
        assert_eq!(writes, vec![(0, PAGE_SIZE_4K)]);
        assert_eq!(dirty_indices(&cache), vec![1]);
        cache.with_folio(1, |folio| {
            folio.expect("remaining dirty folio").clear_dirty();
        });
        assert!(dirty_indices(&cache).is_empty());
    }

    #[def_test]
    fn stale_collected_folio_is_not_written_or_tagged_as_replacement() {
        let cache = new_cache(PageCacheKind::FileBacked);
        write_cached(&cache, 0, &vec![1; PAGE_SIZE_4K]).unwrap();
        write_cached(&cache, PAGE_SIZE_4K as u64, &vec![2; PAGE_SIZE_4K]).unwrap();
        let cache_during_io = cache.clone();
        let mut write_offsets = Vec::new();

        cache
            .write_cache_pages(
                (PAGE_SIZE_4K * 2) as u64,
                0,
                u64::MAX,
                usize::MAX,
                PAGE_SIZE_4K,
                &mut |offset, _| {
                    write_offsets.push(offset);
                    if offset == 0 {
                        let replacement = Arc::new(Mutex::new(Folio::new_zeroed()?));
                        let old = cache_during_io
                            .inner
                            .lock()
                            .pages
                            .insert(1, replacement)
                            .expect("collected folio");
                        old.lock().clear_dirty();
                    }
                    Ok(())
                },
            )
            .unwrap();

        assert_eq!(write_offsets, vec![0]);
        assert!(dirty_indices(&cache).is_empty());
        let replacement_state = cache.with_folio(1, |folio| {
            let folio = folio.expect("replacement folio");
            (folio.is_dirty(), folio.is_under_writeback())
        });
        assert_eq!(replacement_state, (false, false));
    }

    #[def_test]
    fn batched_writeback_failure_restores_dirty_index() {
        let cache = new_cache(PageCacheKind::FileBacked);
        write_cached(&cache, 0, &vec![1; PAGE_SIZE_4K]).unwrap();
        write_cached(&cache, PAGE_SIZE_4K as u64, &vec![2; PAGE_SIZE_4K]).unwrap();

        let error = cache
            .write_cache_pages(
                (PAGE_SIZE_4K * 2) as u64,
                0,
                u64::MAX,
                usize::MAX,
                PAGE_SIZE_4K * 2,
                &mut |_, _| Err(KError::InvalidInput),
            )
            .unwrap_err();

        assert_eq!(error, KError::InvalidInput);
        assert_eq!(dirty_indices(&cache), vec![0, 1]);
        for index in 0..2 {
            cache.with_folio(index, |folio| {
                let folio = folio.expect("failed batch folio");
                assert!(folio.is_dirty());
                assert!(!folio.is_under_writeback());
                folio.clear_dirty();
            });
        }
        assert!(dirty_indices(&cache).is_empty());
    }

    #[def_test]
    fn batched_writeback_preserves_concurrent_redirty() {
        let cache = new_cache(PageCacheKind::FileBacked);
        write_cached(&cache, 0, b"old").unwrap();

        let cache_during_io = cache.clone();
        let mut first_snapshot = Vec::new();
        let mut dirty_indices_before_redirty = Vec::new();
        let mut state_during_io = None;
        let mut dirty_indices_after_redirty = Vec::new();
        cache
            .write_cache_pages(3, 0, u64::MAX, usize::MAX, PAGE_SIZE_4K, &mut |_, data| {
                dirty_indices_before_redirty = dirty_indices(&cache_during_io);
                state_during_io = Some(cache_during_io.with_folio(0, |folio| {
                    let folio = folio.expect("folio under writeback");
                    (folio.is_dirty(), folio.is_under_writeback())
                }));
                first_snapshot.extend_from_slice(&data[..3]);
                write_cached(&cache_during_io, 0, b"new")?;
                dirty_indices_after_redirty = dirty_indices(&cache_during_io);
                Ok(())
            })
            .unwrap();

        assert!(dirty_indices_before_redirty.is_empty());
        assert_eq!(state_during_io, Some((false, true)));
        assert_eq!(dirty_indices_after_redirty, vec![0]);
        assert_eq!(first_snapshot, b"old");
        assert_eq!(dirty_indices(&cache), vec![0]);
        let observed = cache.with_folio(0, |folio| {
            let folio = folio.expect("redirtied folio");
            (
                folio.is_dirty(),
                folio.data()[..3].to_vec(),
                folio.is_under_writeback(),
            )
        });
        assert!(observed.0);
        assert_eq!(observed.1.as_slice(), b"new");
        assert!(!observed.2);

        cache
            .write_cache_pages(3, 0, u64::MAX, usize::MAX, PAGE_SIZE_4K, &mut |_, _| Ok(()))
            .unwrap();
        assert!(dirty_indices(&cache).is_empty());
        let final_state = cache.with_folio(0, |folio| {
            let folio = folio.expect("clean folio");
            (folio.is_dirty(), folio.is_under_writeback())
        });
        assert_eq!(final_state, (false, false));
    }

    #[def_test]
    fn final_truncate_discards_dirty_folios() {
        let cache = new_cache(PageCacheKind::FileBacked);
        write_cached(&cache, 0, b"dirty").unwrap();

        cache.truncate_final().unwrap();
        assert_eq!(cache.nrpages(), 0);
        assert!(dirty_indices(&cache).is_empty());
    }
}

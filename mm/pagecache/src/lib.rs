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

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};

use kalloc::{UsageKind, global_allocator};
use kerrno::{KError, KResult};
use khal::mem::{PhysAddr, VirtAddr, v2p};
use ksync::Mutex;
use log::warn;
use memaddr::PAGE_SIZE_4K;

/// Page index within a mapping.
pub type PageIndex = u64;

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
        let mut folio = folio.lock();
        f(&mut folio)
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

        let tail_index = len / PAGE_SIZE_4K as u64;
        let tail_off = (len % PAGE_SIZE_4K as u64) as usize;
        if tail_off != 0
            && let Some(folio) = inner.pages.get(&tail_index)
        {
            let mut folio = folio.lock();
            folio.data()[tail_off..].fill(0);
            folio.mark_dirty();
        }
    }

    /// Writes back dirty cached folios intersecting `[start, end)`.
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
        let folios = {
            let inner = self.inner.lock();
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
                .collect::<Vec<_>>()
        };

        for (index, folio) in folios {
            if stats.pages_written >= max_pages {
                break;
            }
            let mut folio = folio.lock();
            if !folio.is_dirty() {
                continue;
            }
            if folio.is_under_writeback() {
                stats.skipped(1);
                continue;
            }
            let page_start = index
                .checked_mul(PAGE_SIZE_4K as u64)
                .ok_or(KError::InvalidInput)?;
            let valid_len = visible_len
                .saturating_sub(page_start)
                .min(PAGE_SIZE_4K as u64) as usize;
            folio.clear_dirty();
            folio.start_writeback();
            let result = write_folio_fn(index, &folio.data()[..valid_len], valid_len);
            if result.is_err() {
                folio.mark_dirty();
            }
            folio.end_writeback();
            result?;
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
        let folios = {
            let inner = self.inner.lock();
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
                .collect::<Vec<_>>()
        };

        let max_bytes = max_bytes.max(PAGE_SIZE_4K);
        let mut batch_start = 0u64;
        let mut next_index = None;
        let mut batch_data = Vec::new();
        let mut batch_folios = Vec::new();

        let flush_batch = |batch_start: u64,
                           batch_data: &mut Vec<u8>,
                           batch_folios: &mut Vec<(PageIndex, Arc<Mutex<Folio>>)>,
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
            for (_, folio) in batch_folios.drain(..) {
                let mut folio = folio.lock();
                if result.is_err() {
                    folio.mark_dirty();
                }
                folio.end_writeback();
            }
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

            let mut locked = folio.lock();
            if !locked.is_dirty() {
                drop(locked);
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
            }
            if locked.is_under_writeback() {
                drop(locked);
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
                stats.skipped(1);
                next_index = None;
                continue;
            }

            let contiguous = next_index == Some(index);
            let would_overflow = batch_data.len().saturating_add(valid_len) > max_bytes;
            let would_exceed_pages =
                stats.pages_written.saturating_add(batch_folios.len()) >= max_pages;
            if !batch_data.is_empty() && (!contiguous || would_overflow || would_exceed_pages) {
                drop(locked);
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
                locked = folio.lock();
                if !locked.is_dirty() || locked.is_under_writeback() {
                    if locked.is_under_writeback() {
                        stats.skipped(1);
                    }
                    next_index = None;
                    continue;
                }
            }

            if batch_data.is_empty() {
                batch_start = page_start;
            }
            batch_data.extend_from_slice(&locked.data()[..valid_len]);
            locked.clear_dirty();
            locked.start_writeback();
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
    use unittest::{assert_eq, def_test};

    use super::{Folio, PAGE_SIZE_4K, PageCache, PageCacheKind};

    fn new_cache(kind: PageCacheKind) -> Arc<PageCache> {
        PageCache::new(kind)
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
        let tail_is_zero = cache.with_folio(0, |folio| {
            let folio = folio.expect("surviving folio");
            let tail_is_zero = folio.data()[PAGE_SIZE_4K / 2..]
                .iter()
                .all(|byte| *byte == 0);
            folio.clear_dirty();
            tail_is_zero
        });
        assert!(tail_is_zero);
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
        let is_dirty = cache.with_folio(0, |folio| {
            let folio = folio.expect("failed-writeback folio");
            let is_dirty = folio.is_dirty();
            folio.clear_dirty();
            is_dirty
        });
        assert!(is_dirty);
    }

    #[def_test]
    fn final_truncate_discards_dirty_folios() {
        let cache = new_cache(PageCacheKind::FileBacked);
        write_cached(&cache, 0, b"dirty").unwrap();

        cache.truncate_final().unwrap();
        assert_eq!(cache.nrpages(), 0);
    }
}

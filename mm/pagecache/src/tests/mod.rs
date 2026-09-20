// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared fixtures for cache algorithm tests.

use alloc::{sync::Arc, vec::Vec};

use kerrno::KResult;
use memaddr::PAGE_SIZE_4K;

use crate::{Folio, PageCache, PageCacheKind, PageIndex};

mod mapping_pages;
mod truncate;
mod writeback;

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

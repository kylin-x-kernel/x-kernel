// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Resident storage tests.

use memaddr::PAGE_SIZE_4K;
use unittest::{assert_eq, def_test};

use super::{dirty_indices, new_cache, write_cached};
use crate::PageCacheKind;

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

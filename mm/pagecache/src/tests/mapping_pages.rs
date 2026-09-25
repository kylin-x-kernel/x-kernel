// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Resident storage tests.

use memaddr::PAGE_SIZE_4K;
use unittest::{assert_eq, def_test};

use super::{dirty_indices, new_cache, write_cached};

#[def_test]
fn cached_folio_roundtrip() {
    let cache = new_cache();
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
    let cache = new_cache();
    write_cached(&cache, PAGE_SIZE_4K as u64, b"x").unwrap();

    assert_eq!(cache.nrpages(), 1);
    cache.resize_cached_folios((PAGE_SIZE_4K * 2) as u64, 0);
    assert_eq!(cache.nrpages(), 0);
}

#[def_test]
fn materialization_rejects_a_different_folio_index() {
    let cache = new_cache();
    let result = cache.with_folio_or_create(
        7,
        |_| crate::Folio::new_zeroed(8),
        |_| panic!("a mismatched folio must not be published"),
    );
    assert_eq!(result, Err::<(), _>(kerrno::KError::InvalidInput));
    assert_eq!(cache.nrpages(), 0);
    assert!(dirty_indices(&cache).is_empty());
}

#[def_test]
fn folio_index_survives_detach_and_replacement() {
    let cache = new_cache();
    let index = 7;
    write_cached(&cache, index * PAGE_SIZE_4K as u64, b"old").unwrap();
    let old = cache.state.lock().entries[&index].clone();
    cache.resize_cached_folios((index + 1) * PAGE_SIZE_4K as u64, 0);
    cache.filemap_add_folio(index, 0, b"new").unwrap();
    let replacement = cache.state.lock().entries[&index].clone();
    assert!(!alloc::sync::Arc::ptr_eq(&old, &replacement));
    assert_eq!(old.lock().index(), index);
    assert!(!old.lock().is_dirty());
    assert_eq!(replacement.lock().index(), index);
    cache.with_folio(index, |folio| {
        assert!(folio.unwrap().data()[..3] == *b"new")
    });
}

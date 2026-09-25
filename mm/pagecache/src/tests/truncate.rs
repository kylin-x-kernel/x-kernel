// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Resize and final teardown tests.

use alloc::vec;

use memaddr::PAGE_SIZE_4K;
use unittest::{assert_eq, def_test};

use super::{dirty_indices, new_cache, write_cached};

#[def_test]
fn shrink_drops_full_folios_and_zeros_surviving_tail() {
    let cache = new_cache();
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
    let cache = new_cache();
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
fn final_truncate_discards_dirty_folios() {
    let cache = new_cache();
    write_cached(&cache, 0, b"dirty").unwrap();

    cache.truncate_final().unwrap();
    assert_eq!(cache.nrpages(), 0);
    assert!(dirty_indices(&cache).is_empty());
}

#[def_test]
fn shrink_and_final_teardown_remove_multiple_sparse_batches() {
    let cache = new_cache();
    let count = crate::FolioBatch::CAPACITY * 3 + 2;
    for index in 0..count {
        write_cached(&cache, (index * 2 * PAGE_SIZE_4K) as u64, b"dirty").unwrap();
    }
    // Teardown uses page indices directly and must handle the maximum index
    // without wrapping a traversal cursor back to zero.
    cache.filemap_add_folio(u64::MAX, 0, b"last").unwrap();
    let retained = cache.state.lock().entries[&((count - 1) as u64 * 2)].clone();
    let new_len = (crate::FolioBatch::CAPACITY * 2 * PAGE_SIZE_4K + 1) as u64;
    cache.resize_cached_folios((count * 2 * PAGE_SIZE_4K) as u64, new_len);
    assert_eq!(cache.nrpages(), (crate::FolioBatch::CAPACITY + 1) as u64);
    assert!(!retained.lock().is_dirty());
    cache.with_folio((crate::FolioBatch::CAPACITY * 2) as u64, |folio| {
        assert!(folio.unwrap().data()[1..].iter().all(|byte| *byte == 0));
    });
    cache.truncate_final().unwrap();
    assert_eq!(cache.nrpages(), 0);
    assert!(dirty_indices(&cache).is_empty());
}

#[def_test]
fn mapping_drop_cleans_retained_folios_across_batches() {
    let retained = {
        let cache = new_cache();
        let count = crate::FolioBatch::CAPACITY * 2 + 1;
        write_cached(&cache, 0, &vec![0x5a; count * PAGE_SIZE_4K]).unwrap();
        let retained = cache.state.lock().entries[&(crate::FolioBatch::CAPACITY as u64)].clone();
        assert!(retained.lock().is_dirty());
        retained
    };
    let folio = retained.lock();
    assert_eq!(folio.index(), crate::FolioBatch::CAPACITY as u64);
    assert!(!folio.is_dirty());
    assert!(!folio.is_under_writeback());
}

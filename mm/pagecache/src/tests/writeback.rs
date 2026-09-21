// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Writeback identity, completion and redirty tests.

use alloc::{sync::Arc, vec, vec::Vec};

use kerrno::KError;
use ksync::Mutex;
use memaddr::PAGE_SIZE_4K;
use unittest::{assert_eq, def_test};

use super::{dirty_indices, new_cache, write_cached};
use crate::{Folio, PageCacheKind, WritebackRangeOutcome};

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
    cache.state.lock().dirty_marks.insert(0);
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
    let cache_during_io = &cache;
    let mut write_offsets = Vec::new();

    cache
        .write_cache_pages(
            (PAGE_SIZE_4K * 2) as u64,
            0,
            u64::MAX,
            usize::MAX,
            PAGE_SIZE_4K,
            &mut |offset, data| {
                write_offsets.push(offset);
                if offset == 0 {
                    let replacement = Arc::new(Mutex::new(Folio::new_zeroed()?));
                    let old = cache_during_io
                        .state
                        .lock()
                        .entries
                        .insert(1, replacement)
                        .expect("collected folio");
                    old.lock().clear_dirty();
                }
                Ok(WritebackRangeOutcome::complete(data.len()))
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

    let outcome = cache
        .write_cache_pages(
            (PAGE_SIZE_4K * 2) as u64,
            0,
            u64::MAX,
            usize::MAX,
            PAGE_SIZE_4K * 2,
            &mut |_, _| Ok(WritebackRangeOutcome::failed(0, KError::InvalidInput)),
        )
        .unwrap();

    assert_eq!(outcome.error(), Some(KError::InvalidInput));
    assert_eq!(outcome.stats().pages_written, 0);
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
fn failed_writeback_does_not_redirty_detached_folio() {
    let cache = new_cache(PageCacheKind::FileBacked);
    write_cached(&cache, 0, b"dirty").unwrap();
    let detached = cache
        .state
        .lock()
        .entries
        .get(&0)
        .cloned()
        .expect("cached folio");
    let cache_during_io = &cache;

    let outcome = cache
        .write_cache_pages(
            PAGE_SIZE_4K as u64,
            0,
            u64::MAX,
            usize::MAX,
            PAGE_SIZE_4K,
            &mut |_, _| {
                cache_during_io.resize_cached_folios(PAGE_SIZE_4K as u64, 0);
                Ok(WritebackRangeOutcome::failed(0, KError::InvalidInput))
            },
        )
        .unwrap();

    assert_eq!(outcome.error(), Some(KError::InvalidInput));
    assert!(dirty_indices(&cache).is_empty());
    let detached_state = {
        let folio = detached.lock();
        (folio.is_dirty(), folio.is_under_writeback())
    };
    assert_eq!(detached_state, (false, false));
}

#[def_test]
fn batched_writeback_failure_cleans_only_complete_prefix_folios() {
    let cache = new_cache(PageCacheKind::FileBacked);
    write_cached(&cache, 0, &vec![1; PAGE_SIZE_4K]).unwrap();
    write_cached(&cache, PAGE_SIZE_4K as u64, &vec![2; PAGE_SIZE_4K]).unwrap();
    write_cached(&cache, (PAGE_SIZE_4K * 2) as u64, &vec![3; PAGE_SIZE_4K]).unwrap();

    let completed_bytes = PAGE_SIZE_4K + PAGE_SIZE_4K / 2;
    let outcome = cache
        .write_cache_pages(
            (PAGE_SIZE_4K * 3) as u64,
            0,
            u64::MAX,
            usize::MAX,
            PAGE_SIZE_4K * 3,
            &mut |_, _| {
                Ok(WritebackRangeOutcome::failed(
                    completed_bytes,
                    KError::InvalidInput,
                ))
            },
        )
        .unwrap();

    assert_eq!(outcome.error(), Some(KError::InvalidInput));
    assert_eq!(outcome.stats().pages_written, 1);
    assert_eq!(dirty_indices(&cache), vec![1, 2]);
    for (index, expected_dirty) in [(0, false), (1, true), (2, true)] {
        let state = cache.with_folio(index, |folio| {
            let folio = folio.expect("batched folio remains cached");
            let state = (folio.is_dirty(), folio.is_under_writeback());
            folio.clear_dirty();
            state
        });
        assert_eq!(state, (expected_dirty, false));
    }
    assert!(dirty_indices(&cache).is_empty());
}

#[def_test]
fn batched_writeback_preserves_concurrent_redirty() {
    let cache = new_cache(PageCacheKind::FileBacked);
    write_cached(&cache, 0, b"old").unwrap();

    let cache_during_io = &cache;
    let mut first_snapshot = Vec::new();
    let mut dirty_indices_before_redirty = Vec::new();
    let mut state_during_io = None;
    let mut dirty_indices_after_redirty = Vec::new();
    cache
        .write_cache_pages(3, 0, u64::MAX, usize::MAX, PAGE_SIZE_4K, &mut |_, data| {
            dirty_indices_before_redirty = dirty_indices(cache_during_io);
            state_during_io = Some(cache_during_io.with_folio(0, |folio| {
                let folio = folio.expect("folio under writeback");
                (folio.is_dirty(), folio.is_under_writeback())
            }));
            first_snapshot.extend_from_slice(&data[..3]);
            write_cached(cache_during_io, 0, b"new")?;
            dirty_indices_after_redirty = dirty_indices(cache_during_io);
            Ok(WritebackRangeOutcome::complete(data.len()))
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
        .write_cache_pages(3, 0, u64::MAX, usize::MAX, PAGE_SIZE_4K, &mut |_, data| {
            Ok(WritebackRangeOutcome::complete(data.len()))
        })
        .unwrap();
    assert!(dirty_indices(&cache).is_empty());
    let final_state = cache.with_folio(0, |folio| {
        let folio = folio.expect("clean folio");
        (folio.is_dirty(), folio.is_under_writeback())
    });
    assert_eq!(final_state, (false, false));
}

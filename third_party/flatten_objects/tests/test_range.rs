// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSE for license details.

use std::{cell::Cell, rc::Rc};

use flatten_objects::FlattenObjects;

#[test]
fn every_small_occupancy_and_range_matches_lowest_free_id() {
    for mask in 0u16..256 {
        for minimum in 0..=9 {
            for end in 0..=9 {
                let mut objects = FlattenObjects::<usize, 8>::new();
                for id in 0..8 {
                    if mask & (1 << id) != 0 {
                        assert_eq!(objects.add_at(id, id), Ok(id));
                    }
                }
                let before = objects.count();
                let expected = (minimum..end.min(8)).find(|id| mask & (1 << id) == 0);
                let actual = objects.add_in_range(minimum, end, 99);
                assert_eq!(
                    actual,
                    expected.ok_or(99),
                    "mask={mask} min={minimum} end={end}"
                );
                assert_eq!(objects.count(), before + usize::from(expected.is_some()));
                for id in 0..8 {
                    let value = if Some(id) == expected {
                        Some(&99)
                    } else if mask & (1 << id) != 0 {
                        Some(&id)
                    } else {
                        None
                    };
                    assert_eq!(objects.get(id), value);
                }
            }
        }
    }
}

#[test]
fn range_crosses_bitmap_words_and_reuses_inclusive_minimum() {
    let mut objects = FlattenObjects::<u32, 1024>::new();
    for id in 0..1000 {
        assert_eq!(objects.add_at(id, 1), Ok(id));
    }
    assert_eq!(objects.add_in_range(0, 1000, 2), Err(2));
    assert_eq!(objects.add_in_range(0, 1001, 2), Ok(1000));
    assert_eq!(objects.remove(127), Some(1));
    assert_eq!(objects.remove(128), Some(1));
    assert_eq!(objects.add_in_range(127, 129, 3), Ok(127));
    assert_eq!(objects.add_in_range(127, 129, 4), Ok(128));
    assert_eq!(objects.add_in_range(1023, usize::MAX, 5), Ok(1023));
    assert_eq!(objects.add_in_range(1023, usize::MAX, 6), Err(6));
}

#[test]
fn invalid_bounds_and_single_slot_never_overflow() {
    let mut objects = FlattenObjects::<u32, 1>::new();
    for (minimum, end) in [
        (0, 0),
        (1, 1),
        (1, usize::MAX),
        (usize::MAX, usize::MAX),
        (usize::MAX, 0),
    ] {
        assert_eq!(objects.add_in_range(minimum, end, 7), Err(7));
        assert_eq!(objects.count(), 0);
    }
    assert_eq!(objects.add_in_range(0, usize::MAX, 8), Ok(0));
    assert_eq!(objects.add_in_range(0, 1, 9), Err(9));
}

#[derive(Debug)]
struct DropValue(Rc<Cell<usize>>);

impl Drop for DropValue {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

#[test]
fn failed_insertion_returns_ownership_and_preserves_live_slots() {
    let drops = Rc::new(Cell::new(0));
    let mut objects = FlattenObjects::<DropValue, 8>::new();
    assert!(objects.add_at(4, DropValue(drops.clone())).is_ok());
    let value = objects
        .add_in_range(4, 5, DropValue(drops.clone()))
        .expect_err("full range");
    assert_eq!(drops.get(), 0);
    assert_eq!(objects.count(), 1);
    assert_eq!(objects.ids().collect::<Vec<_>>(), vec![4]);
    drop(value);
    assert_eq!(drops.get(), 1);
    drop(objects);
    assert_eq!(drops.get(), 2);
}

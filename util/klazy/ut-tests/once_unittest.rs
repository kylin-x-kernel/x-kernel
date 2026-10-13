// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::Once;

#[def_test]
fn initialized_and_from_start_ready() {
    let from_initialized = Once::initialized(7u32);
    assert!(from_initialized.is_completed());
    assert_eq!(from_initialized.get().copied(), Some(7));

    let from_trait: Once<u32> = 9u32.into();
    assert!(from_trait.is_completed());
    assert_eq!(from_trait.get().copied(), Some(9));
}

#[def_test]
fn try_call_once_error_allows_retry() {
    let once = Once::<u32>::new();

    let result = once.try_call_once(|| Err::<u32, &'static str>("retry"));
    assert_eq!(result, Err("retry"));
    assert!(!once.is_completed());
    assert!(once.get().is_none());
    assert!(once.poll().is_none());

    let value = once.call_once(|| 11);
    assert_eq!(*value, 11);
    assert!(once.is_completed());
}

#[def_test]
fn get_mut_and_as_mut_ptr_update_ready_value() {
    let mut once = Once::initialized(5usize);
    assert_eq!(
        once.get_mut().map(|value| {
            *value += 2;
            *value
        }),
        Some(7)
    );

    // SAFETY: `once` is a local, fully owned `Once::initialized` value, so
    // this is the only reference to it; `as_mut_ptr` yields a valid,
    // exclusive pointer to the stored `usize`, which is initialized.
    unsafe {
        *once.as_mut_ptr() = 13;
    }

    assert_eq!(once.get().copied(), Some(13));
}

#[def_test]
fn extracting_value_transfers_drop_ownership() {
    use core::sync::atomic::{AtomicUsize, Ordering};

    struct CountDrop<'a>(&'a AtomicUsize);

    impl Drop for CountDrop<'_> {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    let drops = AtomicUsize::new(0);
    let value = Once::initialized(CountDrop(&drops))
        .try_into_inner()
        .unwrap();
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    drop(value);
    assert_eq!(drops.load(Ordering::Relaxed), 1);

    let once = Once::initialized(CountDrop(&drops));
    // SAFETY: this owned cell was initialized immediately above.
    let value = unsafe { once.into_inner_unchecked() };
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    drop(value);
    assert_eq!(drops.load(Ordering::Relaxed), 2);
}

#[def_test]
fn try_into_inner_matches_ready_state() {
    assert_eq!(Once::<u32>::new().try_into_inner(), None);
    assert_eq!(Once::initialized(23u32).try_into_inner(), Some(23));
}

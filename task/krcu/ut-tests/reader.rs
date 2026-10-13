// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[cfg(not(test))]
use unittest::{assert, assert_eq};

use super::*;

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn nested_scopes_balance_on_error_return() {
    let reader = TaskReader::new();
    let (outer, inner, after_inner, result) = reader.with_read(|_| {
        let outer = reader.nesting.load(Ordering::Acquire);
        let inner = reader.with_read(|_| reader.nesting.load(Ordering::Acquire));
        let after_inner = reader.nesting.load(Ordering::Acquire);
        (outer, inner, after_inner, Err::<(), _>("scope result"))
    });
    assert_eq!((outer, inner, after_inner), (1, 2, 1));
    assert_eq!(result, Err("scope result"));
    assert!(!reader.is_reading());
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn nesting_never_wraps() {
    let reader = TaskReader::new();
    assert_eq!(reader.try_leave(), None);
    assert_eq!(reader.nesting.load(Ordering::Acquire), 0);
    reader.nesting.store(usize::MAX - 1, Ordering::Relaxed);
    assert!(reader.try_enter());
    assert!(!reader.try_enter());
    assert_eq!(reader.nesting.load(Ordering::Acquire), usize::MAX);
    assert_eq!(reader.try_leave(), Some(usize::MAX - 1));
    assert_eq!(reader.nesting.load(Ordering::Acquire), usize::MAX - 1);
}

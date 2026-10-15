// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::*;

#[test]
fn nonempty_array_elements_are_isolated() {
    let values = [
        CacheAligned::new([0u8; SMP_CACHE_BYTES + 1]),
        CacheAligned::new([0u8; SMP_CACHE_BYTES + 1]),
    ];
    let first = core::ptr::from_ref(&values[0]).addr();
    let second = core::ptr::from_ref(&values[1]).addr();
    assert_eq!(first % SMP_CACHE_BYTES, 0);
    assert_eq!(second % SMP_CACHE_BYTES, 0);
    assert_eq!(second - first, size_of_val(&values[0]));
    assert!((first + size_of_val(&values[0]) - 1) / SMP_CACHE_BYTES < second / SMP_CACHE_BYTES);
}

#[test]
fn stronger_alignment_and_value_access_are_preserved() {
    #[repr(align(256))]
    struct Strong(u32);
    let mut value = CacheAligned::new(Strong(7));
    assert_eq!(align_of_val(&value), 256.max(SMP_CACHE_BYTES));
    value.deref_mut().0 += 1;
    assert_eq!(value.deref().0, 8);
}

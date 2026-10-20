// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::{DynPageIter, PageIter};
use crate::{PAGE_SIZE_4K, VirtAddr};

#[def_test]
fn test_page_iter_steps() {
    let start = VirtAddr::from(0x1000usize);
    let end = VirtAddr::from(0x3000usize);
    let mut iter = PageIter::<PAGE_SIZE_4K, _>::new(start, end).unwrap();
    assert_eq!(iter.next().unwrap(), start);
    assert_eq!(iter.next().unwrap(), VirtAddr::from(0x2000usize));
    assert!(iter.next().is_none());
}

#[def_test]
fn test_dyn_page_iter_steps() {
    let start = VirtAddr::from(0x0usize);
    let end = VirtAddr::from(0x2000usize);
    let mut iter = DynPageIter::new(start, end, PAGE_SIZE_4K).unwrap();
    assert_eq!(iter.next().unwrap(), start);
    assert_eq!(iter.next().unwrap(), VirtAddr::from(0x1000usize));
    assert!(iter.next().is_none());
}

#[def_test]
fn test_page_iter_invalid_alignment() {
    let start = VirtAddr::from(0x1001usize);
    let end = VirtAddr::from(0x2000usize);
    assert!(PageIter::<PAGE_SIZE_4K, _>::new(start, end).is_none());
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Transfer of independent holds, from an exact range or an exclusive compound split.

use core::iter::FusedIterator;

use super::{PageRef, global_meta};

/// Iterator transferring one already-owned independent PageRef per split page.
/// Taking a page does not increment its count. Drop releases unconsumed pages.
/// This is a transfer guard, not another reference-counter or sharing protocol.
#[derive(Debug)]
pub struct SplitPages {
    next_pfn: usize,
    end_pfn: usize,
}

impl SplitPages {
    pub(super) fn new(first_pfn: usize, num_pages: usize) -> Self {
        Self {
            next_pfn: first_pfn,
            end_pfn: first_pfn + num_pages,
        }
    }
}

impl Iterator for SplitPages {
    type Item = PageRef;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next_pfn == self.end_pfn {
            return None;
        }
        let meta = global_meta(self.next_pfn);
        self.next_pfn += 1;
        Some(PageRef {
            first: meta,
            num_pages: 1,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.end_pfn - self.next_pfn;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for SplitPages {}
impl FusedIterator for SplitPages {}

impl Drop for SplitPages {
    fn drop(&mut self) {
        for page in self.by_ref() {
            drop(page);
        }
    }
}

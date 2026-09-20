// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Resident-run lookup for readahead windows.

use crate::{PageCache, PageIndex};

impl PageCache {
    /// Returns the number of contiguous cached folios starting at `start`.
    pub fn cached_run_len(&self, start: PageIndex, count: usize) -> usize {
        if count == 0 {
            return 0;
        }
        let inner = self.inner.lock();
        let mut found = 0usize;
        for index in start..start.saturating_add(count as u64) {
            if !inner.pages.contains_key(&index) {
                break;
            }
            found += 1;
        }
        found
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Open-file readahead history and page-window selection.

use core::ops::Range;

use memaddr::PAGE_SIZE_4K;
use pagecache::PageIndex;

/// Policy state belonging to one open file description, not to its mapping.
/// Window lengths are in pages; `prev_pos_bytes` is the completed byte position.
/// No EOF, resident-page ownership, or in-flight I/O is stored here.
pub(crate) struct FileRaState {
    start: PageIndex,
    size_pages: u64,
    async_size_pages: u64,
    prev_pos_bytes: Option<u64>,
    ra_pages: u64,
}

impl FileRaState {
    pub(crate) fn new(ra_pages: usize) -> Self {
        Self {
            start: 0,
            size_pages: 0,
            async_size_pages: 0,
            prev_pos_bytes: None,
            ra_pages: ra_pages as u64,
        }
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.ra_pages != 0
    }

    pub(crate) fn record_position(&mut self, pos: u64) {
        self.prev_pos_bytes = Some(pos);
    }

    pub(crate) fn shrink_after_io_error(&mut self) {
        self.ra_pages /= 4;
    }

    // Linux get_init_ra_size: amplify small requests, then cap large windows.
    fn initial_size(request_pages: u64, max: u64) -> u64 {
        let size_pages = request_pages.min(max).next_power_of_two();
        if size_pages <= max / 32 {
            size_pages * 4
        } else if size_pages <= max / 4 {
            size_pages * 2
        } else {
            max
        }
    }

    // Linux get_next_ra_size: grow by four early, then double up to the budget.
    fn next_size(size_pages: u64, max: u64) -> u64 {
        if size_pages < max / 16 {
            size_pages * 4
        } else if size_pages <= max / 2 {
            size_pages * 2
        } else {
            max
        }
    }

    fn window(&self) -> (Range<PageIndex>, Option<PageIndex>) {
        let end = self.start.saturating_add(self.size_pages);
        (self.start..end, Some(end - self.async_size_pages))
    }

    pub(crate) fn on_miss(
        &mut self,
        index: PageIndex,
        request_pages: u64,
        history: impl FnOnce(u64) -> u64,
    ) -> (Range<PageIndex>, Option<PageIndex>) {
        let max = self.ra_pages;
        if max == 0 {
            return (index..index + 1, None);
        }
        let sequential = self.prev_pos_bytes.is_some_and(|pos| {
            index
                .checked_sub(pos / PAGE_SIZE_4K as u64)
                .is_some_and(|delta| delta <= 1)
        });
        if index == 0 || request_pages > max || sequential {
            self.start = index;
            self.size_pages = Self::initial_size(request_pages, max);
            self.async_size_pages = if self.size_pages > request_pages {
                self.size_pages - request_pages
            } else {
                self.size_pages / 2
            };
        } else {
            let mut contiguous = history(max);
            if contiguous <= request_pages {
                // A small random miss must not overwrite a sequential stream's history.
                return (index..index.saturating_add(request_pages.min(max)), None);
            }
            if contiguous == index {
                contiguous *= 2;
            }
            self.start = index;
            self.size_pages = contiguous.saturating_add(request_pages).min(max);
            self.async_size_pages = 1;
        }
        self.window()
    }

    pub(crate) fn on_marker(
        &mut self,
        index: PageIndex,
        request_pages: u64,
        next_miss: impl FnOnce(u64) -> Option<PageIndex>,
    ) -> Option<(Range<PageIndex>, Option<PageIndex>)> {
        let max = self.ra_pages;
        if max == 0 {
            return None;
        }
        if self.size_pages != 0 && index == self.start + self.size_pages - self.async_size_pages {
            self.start += self.size_pages;
            self.size_pages = Self::next_size(self.size_pages, max);
        } else {
            // A different open or interleaved stream may consume the shared marker.
            let start = next_miss(max)?;
            self.start = start;
            self.size_pages = Self::next_size((start - index).saturating_add(request_pages), max);
        }
        self.async_size_pages = self.size_pages;
        Some(self.window())
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn windows_grow_on_markers_and_random_misses_preserve_history() {
        let mut ra = FileRaState::new(block::BackingDevInfo::DEFAULT_RA_PAGES);
        assert_eq!(ra.on_miss(0, 1, |_| 0), (0..4, Some(1)));
        assert_eq!(ra.on_miss(100, 1, |_| 0), (100..101, None));
        assert_eq!(ra.on_marker(1, 1, |_| None), Some((4..12, Some(4))));
        assert_eq!(ra.on_marker(4, 1, |_| None), Some((12..28, Some(12))));
        assert_eq!(ra.on_marker(12, 1, |_| None), Some((28..60, Some(28))));
        // Another open's marker reconstructs a window from shared cache history.
        let mut other = FileRaState::new(block::BackingDevInfo::DEFAULT_RA_PAGES);
        assert_eq!(other.on_marker(1, 1, |_| Some(4)), Some((4..12, Some(4))));
        assert_eq!(other.on_miss(100, 1, |_| 8), (100..109, Some(108)));
        ra.shrink_after_io_error();
        assert_eq!(ra.on_miss(0, 32, |_| 0).0, 0..8);
        ra.shrink_after_io_error();
        ra.shrink_after_io_error();
        assert_eq!(ra.on_miss(0, 32, |_| 0), (0..1, None));
        assert_eq!(ra.on_marker(1, 1, |_| Some(4)), None);
    }
}

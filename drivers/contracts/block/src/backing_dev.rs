// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Backing-device policy shared by files on one disk.

use core::sync::atomic::{AtomicUsize, Ordering};

use memaddr::PAGE_SIZE_4K;

/// The readahead policy portion of Linux `backing_dev_info`.
///
/// A disk owns one policy; newly opened files snapshot its budget into their
/// own readahead state. This object owns neither cached pages nor read history.
pub struct BackingDevInfo {
    ra_pages: AtomicUsize,
}

impl BackingDevInfo {
    /// Default readahead budget, in base pages (128 KiB).
    pub const DEFAULT_RA_PAGES: usize = 128 * 1024 / PAGE_SIZE_4K;

    pub(crate) const fn new() -> Self {
        Self {
            ra_pages: AtomicUsize::new(Self::DEFAULT_RA_PAGES),
        }
    }

    /// Returns the default budget for newly opened files, in base pages.
    pub fn readahead_pages(&self) -> usize {
        self.ra_pages.load(Ordering::Relaxed)
    }

    /// Sets the default budget for future file opens, in base pages.
    /// Existing file descriptions retain their own window and budget.
    pub(crate) fn set_readahead_pages(&self, pages: usize) {
        self.ra_pages.store(pages, Ordering::Relaxed);
    }
}

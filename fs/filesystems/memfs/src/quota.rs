// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-mapping-group page accounting for in-memory page cache.

use core::sync::atomic::{AtomicUsize, Ordering};

use kerrno::{KError, KResult};
use ksync::RwLock;

/// Limit and usage for pages owned by one quota group.
///
/// A quota group is typically one in-memory filesystem superblock. `None`
/// means unlimited; `Some(0)` means the group can no longer materialize pages.
/// Charging follows mapping ownership: pages are released when a folio exits the
/// cache identity, so truncate and eviction do not wait for external references.
#[derive(Debug)]
pub(crate) struct PageQuota {
    max_pages: RwLock<Option<usize>>,
    used_pages: AtomicUsize,
}

impl PageQuota {
    /// Creates a quota with the supplied maximum page count.
    pub(crate) fn new(max_pages: Option<usize>) -> Self {
        Self {
            max_pages: RwLock::new(max_pages),
            used_pages: AtomicUsize::new(0),
        }
    }

    /// Returns the current maximum page count.
    #[cfg(unittest)]
    pub(crate) fn max_pages(&self) -> Option<usize> {
        *self.max_pages.read()
    }

    /// Replaces the maximum page count after Linux-compatible validation.
    ///
    /// A finite limit may be raised, lowered to the current usage, or replaced by
    /// unlimited. It cannot be lowered below the current usage, and an unlimited
    /// quota cannot be retroactively limited. The write lock serializes the check
    /// and update against new charges.
    ///
    /// Returns [`KError::InvalidInput`] for a finite limit below current usage
    /// or for retroactively limiting an unlimited quota, like Linux tmpfs remount.
    pub(crate) fn try_set_max_pages(&self, max_pages: Option<usize>) -> KResult<()> {
        let mut current_max = self.max_pages.write();
        if let Some(max_pages) = max_pages {
            if current_max.is_none() {
                return Err(KError::InvalidInput);
            }
            if self.used_pages.load(Ordering::Acquire) > max_pages {
                return Err(KError::InvalidInput);
            }
        }
        *current_max = max_pages;
        Ok(())
    }

    /// Returns the number of currently charged pages.
    pub(crate) fn used_pages(&self) -> usize {
        self.used_pages.load(Ordering::Acquire)
    }

    /// Returns statfs total and available pages from the same limit snapshot.
    /// Unlimited instances report zero for both fields, matching Linux tmpfs.
    pub(crate) fn statfs_pages(&self) -> (u64, u64) {
        let max_pages = self.max_pages.read();
        match *max_pages {
            Some(max_pages) => (
                max_pages as u64,
                max_pages.saturating_sub(self.used_pages()) as u64,
            ),
            None => (0, 0),
        }
    }

    pub(crate) fn try_charge_page(&self) -> KResult<()> {
        let max_pages = self.max_pages.read();
        if let Some(max_pages) = *max_pages {
            let mut used_pages = self.used_pages.load(Ordering::Acquire);
            loop {
                if used_pages >= max_pages {
                    return Err(KError::StorageFull);
                }
                match self.used_pages.compare_exchange(
                    used_pages,
                    used_pages.checked_add(1).ok_or(KError::InvalidInput)?,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return Ok(()),
                    Err(actual) => used_pages = actual,
                }
            }
        }

        // The read lock admits concurrent unlimited charges, and the page count
        // cannot approach `usize::MAX` on a machine whose page count fits in
        // `usize`.
        self.used_pages.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn release_pages(&self, count: usize) {
        let previous = self.used_pages.fetch_sub(count, Ordering::AcqRel);
        debug_assert!(
            previous >= count,
            "page quota release must match resident additions"
        );
    }
}

impl pagecache::CacheResidency for PageQuota {
    fn try_add_folio(&self) -> KResult<()> {
        self.try_charge_page()
    }

    fn remove_folios(&self, count: usize) {
        self.release_pages(count);
    }
}

#[unittest::mod_test]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::*;

    #[def_test]
    fn remount_below_usage_returns_einval_without_changing_limit() {
        let quota = PageQuota::new(Some(4));
        quota.try_charge_page().unwrap();
        quota.try_charge_page().unwrap();
        assert_eq!(quota.try_set_max_pages(Some(1)), Err(KError::InvalidInput));
        assert_eq!(quota.max_pages(), Some(4));
        assert_eq!(quota.used_pages(), 2);
        assert_eq!(quota.try_set_max_pages(Some(2)), Ok(()));
        assert_eq!(quota.try_charge_page(), Err(KError::StorageFull));
        quota.release_pages(1);
        quota.release_pages(1);
    }

    #[def_test]
    fn capacity_snapshot_uses_one_limit_and_zero_for_unlimited() {
        let quota = PageQuota::new(Some(4));
        quota.try_charge_page().unwrap();
        assert_eq!(quota.statfs_pages(), (4, 3));
        quota.try_set_max_pages(Some(8)).unwrap();
        assert_eq!(quota.statfs_pages(), (8, 7));
        quota.try_set_max_pages(None).unwrap();
        assert_eq!(quota.statfs_pages(), (0, 0));
        quota.release_pages(1);
    }
}

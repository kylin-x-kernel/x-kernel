// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Superblock-owned tmpfs page and inode-space capacity.

use core::sync::atomic::{AtomicUsize, Ordering};

use kerrno::{KError, KResult};
use ksync::RwLock;
use kvfs::StatFs;

/// Inode-space charge for an inode or an additional hard link.
const INODE_SPACE_BYTES: usize = 1024;

pub(super) const MAX_INODES: usize = usize::MAX / INODE_SPACE_BYTES;

/// Limits and remaining inode space protected together during reconfiguration.
#[derive(Debug)]
struct TmpfsStats {
    max_pages: usize,
    max_inodes: usize,
    free_inode_space: usize,
}

/// Capacity shared by all inodes of one tmpfs superblock.
///
/// Zero means unlimited in either dimension. Pages are released
/// when the folio's physical allocation is freed, including after retained
/// mapping tokens are released, and inode space when an inode is destroyed
/// or an additional hard link is removed.
#[derive(Debug)]
pub(crate) struct TmpfsAccounting {
    stats: RwLock<TmpfsStats>,
    used_pages: AtomicUsize,
}

impl TmpfsAccounting {
    /// Creates accounting from validated limits; zero selects unlimited.
    pub(crate) fn new(max_pages: usize, max_inodes: usize) -> Self {
        Self {
            stats: RwLock::new(TmpfsStats {
                max_pages,
                max_inodes,
                free_inode_space: max_inodes * INODE_SPACE_BYTES,
            }),
            used_pages: AtomicUsize::new(0),
        }
    }

    /// Applies explicitly requested limits as one remount transaction.
    ///
    /// `None` preserves the current limit; `Some(0)` selects unlimited.
    /// Rejects limits below current use and a finite limit on an unlimited
    /// instance. Failure leaves both old limits intact.
    pub(crate) fn reconfigure(
        &self,
        page_limit: Option<usize>,
        inode_limit: Option<usize>,
    ) -> KResult<()> {
        let mut stats = self.stats.write();
        let used_inode_space = stats.max_inodes * INODE_SPACE_BYTES - stats.free_inode_space;
        if let Some(max_pages) = page_limit
            && max_pages != 0
            && (stats.max_pages == 0 || self.used_pages() > max_pages)
        {
            return Err(KError::InvalidInput);
        }
        if let Some(max_inodes) = inode_limit
            && max_inodes != 0
            && (stats.max_inodes == 0 || max_inodes * INODE_SPACE_BYTES < used_inode_space)
        {
            return Err(KError::InvalidInput);
        }
        if let Some(max_pages) = page_limit {
            stats.max_pages = max_pages;
        }
        if let Some(max_inodes) = inode_limit {
            stats.max_inodes = max_inodes;
            // Unlimited instances stop inode accounting and cannot be limited
            // again. Their unused space field stays zero instead of underflowing.
            stats.free_inode_space = if max_inodes == 0 {
                0
            } else {
                max_inodes * INODE_SPACE_BYTES - used_inode_space
            };
        }
        Ok(())
    }

    /// Reserves an inode or an additional hard link before namespace publication.
    pub(crate) fn reserve_inode(&self) -> KResult<()> {
        let mut stats = self.stats.write();
        if stats.max_inodes != 0 {
            if stats.free_inode_space < INODE_SPACE_BYTES {
                return Err(KError::StorageFull);
            }
            stats.free_inode_space -= INODE_SPACE_BYTES;
        }
        Ok(())
    }

    /// Returns a destroyed inode's charge or a removed additional hard link.
    pub(crate) fn free_inode(&self) {
        let mut stats = self.stats.write();
        if stats.max_inodes != 0 {
            stats.free_inode_space += INODE_SPACE_BYTES;
            debug_assert!(stats.free_inode_space <= stats.max_inodes * INODE_SPACE_BYTES);
        }
    }

    fn used_pages(&self) -> usize {
        self.used_pages.load(Ordering::Acquire)
    }

    /// Reports both capacities from one snapshot, with zero for unlimited.
    pub(crate) fn statfs(&self, stat: &mut StatFs) {
        let stats = self.stats.read();
        if stats.max_pages != 0 {
            stat.blocks = stats.max_pages as u64;
            stat.blocks_free = stats.max_pages.saturating_sub(self.used_pages()) as u64;
            stat.blocks_available = stat.blocks_free;
        }
        if stats.max_inodes != 0 {
            stat.file_count = stats.max_inodes as u64;
            stat.free_file_count = (stats.free_inode_space / INODE_SPACE_BYTES) as u64;
        }
    }

    fn try_charge_page(&self) -> KResult<()> {
        let stats = self.stats.read();
        if stats.max_pages != 0 {
            let mut used_pages = self.used_pages.load(Ordering::Acquire);
            loop {
                if used_pages >= stats.max_pages {
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

    fn release_page(&self) {
        let previous = self.used_pages.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(
            previous >= 1,
            "page quota release must match admitted physical pages"
        );
    }
}

impl pagecache::CacheResidency for TmpfsAccounting {
    fn try_add_folio(&self) -> KResult<()> {
        self.try_charge_page()
    }

    fn remove_folio(&self) {
        self.release_page();
    }
}

unittest::path_mod_test!(tests, "../ut-tests/quota.rs");

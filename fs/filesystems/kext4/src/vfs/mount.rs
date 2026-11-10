// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Mounted core access and its safe metadata-cache maintenance boundary.

use alloc::sync::Arc;
use core::ops::{Deref, DerefMut};

use kvfs::{SuperBlock, TimestampLimits};

use crate::{
    Ext4Error, Ext4Result, Ext4SbInfo,
    journal::MountedJournal,
    sync::{RwLock, RwLockReadGuard, RwLockWriteGuard},
};

/// Per-access inspection budget, independent of journal checkpoint cadence.
const METADATA_CACHE_SCAN_LIMIT: usize = 64;

/// Serializes core mutations and services cache demand after each core access.
///
/// Allocators run under a write guard. Reclaim runs before either guard releases
/// mount protection, after the operation's allocator locks and prefetch windows
/// have ended. Thus read-only traffic can advance reclaim without allowing an
/// eviction between an allocator's prefetch and authoritative in-lock re-read.
/// Without cache demand, maintenance only reads a hint and skips the cache lock.
pub(crate) struct Ext4Mount {
    state: RwLock<Ext4SbInfo>,
    // Geometry and journal identity are frozen before mount publication.
    // Buffered writes must not wait for persistence to read these capabilities.
    bitmap_max_file_size: u64,
    timestamp_limits: TimestampLimits,
    has_huge_file: bool,
    journal: Option<Arc<MountedJournal>>,
}

impl Ext4Mount {
    /// Installs one core state with its mount access and maintenance boundary.
    pub(crate) fn new(state: Ext4SbInfo) -> Self {
        Self {
            bitmap_max_file_size: state.bitmap_max_file_size(),
            timestamp_limits: state.timestamp_limits(),
            has_huge_file: state.superblock().features().has_huge_file(),
            journal: state.journal.clone(),
            state: RwLock::new(state),
        }
    }

    /// Checks the retained journal's live abort state without mount exclusion.
    /// VFS mount writability remains a separate check at write admission.
    pub(crate) fn ensure_writable(&self) -> Ext4Result<()> {
        if self
            .journal
            .as_ref()
            .is_some_and(|journal| journal.is_aborted())
        {
            return Err(Ext4Error::JournalAborted);
        }
        Ok(())
    }

    pub(crate) const fn bitmap_max_file_size(&self) -> u64 {
        self.bitmap_max_file_size
    }

    pub(crate) const fn timestamp_limits(&self) -> TimestampLimits {
        self.timestamp_limits
    }

    pub(crate) const fn has_huge_file(&self) -> bool {
        self.has_huge_file
    }

    /// Enters a shared core operation; may sleep until mutations complete.
    pub(crate) fn read(&self) -> Ext4MountReadGuard<'_> {
        Ext4MountReadGuard(self.state.read())
    }

    /// Enters an exclusive core operation, including allocator prefetch windows.
    ///
    /// `super_block` must own this mount. Journal abort publishes its read-only
    /// flag before the guard releases mutation protection.
    pub(crate) fn write<'a>(&'a self, super_block: &'a SuperBlock) -> Ext4MountWriteGuard<'a> {
        Ext4MountWriteGuard(self.state.write(), super_block)
    }
}

/// Shared core access; drop performs bounded, I/O-free cache maintenance.
pub(crate) struct Ext4MountReadGuard<'a>(RwLockReadGuard<'a, Ext4SbInfo>);

impl Deref for Ext4MountReadGuard<'_> {
    type Target = Ext4SbInfo;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for Ext4MountReadGuard<'_> {
    fn drop(&mut self) {
        let _ = self.0.metadata_io.reclaim_cache(METADATA_CACHE_SCAN_LIMIT);
    }
}

/// Exclusive core access; drop occurs after the operation's allocator work.
pub(crate) struct Ext4MountWriteGuard<'a>(RwLockWriteGuard<'a, Ext4SbInfo>, &'a SuperBlock);

impl Deref for Ext4MountWriteGuard<'_> {
    type Target = Ext4SbInfo;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Ext4MountWriteGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for Ext4MountWriteGuard<'_> {
    fn drop(&mut self) {
        // Mounted abort paths, including writeback metadata completion, hold
        // this write guard until abort has published its atomic projection.
        // The same mount cannot have another write guard dropping in between.
        // Any future abort outside mount protection must publish superblock
        // read-only state itself, rather than relying on an unrelated drop.
        if self
            .0
            .journal
            .as_ref()
            .is_some_and(|journal| journal.is_aborted())
        {
            // Publish the write ban before releasing mount mutation protection,
            // including for writes which would otherwise only dirty PageCache.
            self.1.mark_read_only();
        }
        let _ = self.0.metadata_io.reclaim_cache(METADATA_CACHE_SCAN_LIMIT);
    }
}

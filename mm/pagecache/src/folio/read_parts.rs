// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Disjoint read ownership with whole-folio completion.

use alloc::sync::Arc;
use core::{
    ops::Range,
    sync::atomic::{AtomicUsize, Ordering},
};

use kerrno::KResult;
use memaddr::PAGE_SIZE_4K;

use super::FolioRead;
use crate::PageIndex;

struct ReadCompletion {
    read: FolioRead,
    uninitialized_bytes: AtomicUsize,
}

impl Drop for ReadCompletion {
    fn drop(&mut self) {
        self.read
            .publish_result(*self.uninitialized_bytes.get_mut() == 0);
    }
}

impl FolioRead {
    /// Splits initialization into two exclusively owned, nonempty byte ranges.
    ///
    /// Only split reads allocate shared completion state. Each part can move
    /// into a separate Bio or split again. The original folio stays locked
    /// until every part completes or drops; only success for every byte
    /// publishes uptodate. A failed or abandoned part makes the whole read fail.
    ///
    /// # Errors
    /// Returns the original token if `offset` is zero or outside the page.
    pub fn split_at(self, offset: usize) -> Result<(FolioReadPart, FolioReadPart), Self> {
        if offset == 0 || offset >= PAGE_SIZE_4K {
            return Err(self);
        }
        let completion = Arc::new(ReadCompletion {
            read: self,
            uninitialized_bytes: AtomicUsize::new(PAGE_SIZE_4K),
        });
        Ok((
            FolioReadPart {
                completion: completion.clone(),
                range: 0..offset,
            },
            FolioReadPart {
                completion,
                range: offset..PAGE_SIZE_4K,
            },
        ))
    }
}

/// Exclusive initialization ownership for a disjoint part of a locked folio.
///
/// This token is not cloneable. Splitting consumes it and partitions its range,
/// so independent I/O owners never expose overlapping bytes. The last part's
/// destruction publishes the aggregate result and releases the folio lock.
/// Dropping without completion retains failure for that range.
#[must_use = "complete every part after I/O or drop it to fail initialization"]
pub struct FolioReadPart {
    completion: Arc<ReadCompletion>,
    range: Range<usize>,
}

impl FolioReadPart {
    /// Returns the original folio's immutable index.
    pub fn index(&self) -> PageIndex {
        self.completion.read.index()
    }

    /// Returns this part's starting byte offset within the folio.
    pub fn offset(&self) -> usize {
        self.range.start
    }

    /// Borrows only this part's bytes; other parts may be in device ownership.
    pub fn data(&mut self) -> &mut [u8] {
        let address = self.completion.read.folio.addr() + self.range.start;
        // SAFETY: consuming splits partition the page into disjoint ranges.
        // This token retains the allocation and folio lock; its exclusive
        // borrow prevents splitting or completing while this slice lives.
        unsafe { core::slice::from_raw_parts_mut(address.as_mut_ptr(), self.range.len()) }
    }

    /// Splits this part at a byte offset relative to its start.
    ///
    /// # Errors
    /// Returns the unchanged owner unless `offset` lies strictly inside it.
    pub fn split_at(mut self, offset: usize) -> Result<(Self, Self), Self> {
        if offset == 0 || offset >= self.range.len() {
            return Err(self);
        }
        let middle = self.range.start + offset;
        let tail = Self {
            completion: self.completion.clone(),
            range: middle..self.range.end,
        };
        self.range.end = middle;
        Ok((self, tail))
    }

    /// Completes this part after all device access and EOF zeroing have ended.
    ///
    /// Success requires initialization of every byte in this part, including
    /// holes. Completion consumes the owner and never sleeps; it may run in
    /// ordinary I/O completion context, but not NMI. Other parts retain the lock.
    ///
    /// # Errors
    /// Returns the supplied error unchanged; failure leaves the folio stale
    /// once the last owner is released, regardless of other parts' results.
    pub fn complete(self, result: KResult<()>) -> KResult<()> {
        if result.is_ok() {
            // Arc's final Release/Acquire destruction orders all part writes
            // before publication. This counter only accounts successful bytes.
            self.completion
                .uninitialized_bytes
                .fetch_sub(self.range.len(), Ordering::Relaxed);
        }
        result
    }
}

// SAFETY: each unique token retains a stable allocation and the folio lock.
// Consuming splits create disjoint ranges, and both views expose only this
// token's range. A borrow prevents its completion or further split. Final
// destruction publishes/unlocks without sleeping after device access ends.
unsafe impl block::BioMemory for FolioReadPart {
    fn bytes(&self) -> &[u8] {
        let address = self.completion.read.folio.addr() + self.range.start;
        // SAFETY: the retained page is initialized storage, and this token's
        // exclusive range cannot be mutated through any other safe owner.
        unsafe { core::slice::from_raw_parts(address.as_ptr(), self.range.len()) }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        self.data()
    }
}

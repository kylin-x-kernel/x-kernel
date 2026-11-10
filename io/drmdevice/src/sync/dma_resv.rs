// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `dma_resv`: the implicit ordering of one dma-buf backing.
//!
//! A reservation records which outstanding work may still read or write the
//! memory. It is what makes implicit synchronization work without anyone
//! handing a fence around:
//!
//! * a producer adds its fence to the reservation when it hands the buffer out
//!   ([`DmaResv::set_write_fence`] for exclusive work,
//!   [`DmaResv::add_read_fence`] for shared work);
//! * a consumer reads the reservation with the usage it intends
//!   ([`DmaResv::snapshot`]) and chains that snapshot: the fence it creates is
//!   signaled only after every recorded fence completed.
//!
//! The two lists are published immutably ([`FenceList`]), so a reader always
//! gets a complete set, and a writer that keeps a reservation object across
//! signalings — a display card — replaces the whole list instead of mutating
//! one whose fences a consumer may still be waiting on.
//!
//! Errors propagate through the fences themselves: a chain keeps the outcome of
//! its premise, so a consumer that waits for the reservation sees the producer's
//! failure. A replaced fence list is released as a whole; there is no separate
//! error slot that a newer reservation could overwrite.

use alloc::sync::Arc;

use kspin::SpinNoIrq;

use super::{dma_fence::DmaFence, fence_list::FenceList};

/// Which outstanding work a consumer must wait for.
///
/// Mirrors `enum dma_resv_usage`: a write access conflicts with every fence,
/// a read access only with writers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResvUsage {
    /// The memory will be written: wait for every fence of the reservation.
    Write,
    /// The memory will only be read: wait for the exclusive (write) fences.
    Read,
}

/// The fences of one reservation, with their usage.
pub(crate) struct DmaResv {
    /// Exclusive work: a fence there completes only after it no longer writes.
    write: SpinNoIrq<Arc<FenceList>>,
    /// Shared read-only work, parallel among themselves.
    read: SpinNoIrq<Arc<FenceList>>,
}

impl DmaResv {
    /// An empty reservation.
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            write: SpinNoIrq::new(FenceList::empty()),
            read: SpinNoIrq::new(FenceList::empty()),
        })
    }

    /// Records `fence` as exclusive work and returns the previous list.
    ///
    /// The caller owns what the previous list meant: a replaced commit's fences
    /// are released when the new list can no longer be observed by the work
    /// that replaced them.
    pub(crate) fn set_write_fence(&self, fence: Arc<DmaFence>) -> Arc<FenceList> {
        let list = FenceList::new(alloc::vec![fence]);
        core::mem::replace(&mut *self.write.lock(), list)
    }

    /// Restores a list returned by [`Self::set_write_fence`].
    ///
    /// A submission that never reached the host has no exclusive work of its
    /// own, so the reservation must not keep the fence the failed attempt
    /// installed.
    pub(crate) fn restore_write_fence(&self, list: Arc<FenceList>) {
        *self.write.lock() = list;
    }

    /// Records `fence` as additional exclusive work and returns the previous list.
    ///
    /// Importing a write fence adds an external writer; it must not replace
    /// writers that are still pending.
    pub(crate) fn add_write_fence(&self, fence: Arc<DmaFence>) -> Arc<FenceList> {
        let mut write = self.write.lock();
        let next = write.clone().push(fence);
        core::mem::replace(&mut *write, next)
    }

    /// Records `fence` as additional shared work and returns the previous list.
    pub(crate) fn add_read_fence(&self, fence: Arc<DmaFence>) -> Arc<FenceList> {
        let mut read = self.read.lock();
        let next = read.clone().push(fence);
        core::mem::replace(&mut *read, next)
    }

    /// The exclusive work of the reservation.
    pub(crate) fn write_list(&self) -> Arc<FenceList> {
        self.write.lock().clone()
    }

    /// The shared work of the reservation.
    pub(crate) fn read_list(&self) -> Arc<FenceList> {
        self.read.lock().clone()
    }

    /// The fences a consumer with `usage` must wait for.
    pub(crate) fn snapshot(&self, usage: ResvUsage) -> Arc<FenceList> {
        let write = self.write_list();
        let mut fences = write.fences().to_vec();
        if usage != ResvUsage::Read {
            fences.extend_from_slice(self.read_list().fences());
        }
        FenceList::new(fences)
    }
}

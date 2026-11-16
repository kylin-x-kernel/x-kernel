// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! MM-owned mapping-attribute capabilities, independent of allocation and device ownership.

#![no_std]

extern crate alloc;

use alloc::boxed::Box;

use memaddr::VirtAddr;

/// The two kernel mapping attribute sets used by coherent DMA allocations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DmaMappingMode {
    /// Read/write, uncached and platform-shared DMA pages.
    Coherent,
    /// Ordinary read/write kernel pages before returning them to the allocator.
    Normal,
}

/// Pending kernel-invalidation work returned by [`DmaMappingIf::set_mode`].
///
/// The caller completes it after releasing any lock it holds around the mode
/// change. A CPU that needs such a lock spins with interrupts disabled and
/// cannot acknowledge a TLB shootdown, so completing while the lock is held
/// can stall the initiator until its bounded give-up.
pub struct DmaMappingCompletion {
    complete: Option<Box<dyn FnOnce() -> bool + Send>>,
}

impl DmaMappingCompletion {
    /// Wraps completion work that reports whether the invalidation succeeded.
    pub fn new(complete: impl FnOnce() -> bool + Send + 'static) -> Self {
        Self {
            complete: Some(Box::new(complete)),
        }
    }

    /// Runs the pending invalidation and reports whether it completed.
    ///
    /// Must be called after releasing every lock held around the mode change.
    /// The result must not be treated as success when it is `false`.
    pub fn complete(mut self) -> bool {
        self.run()
    }

    fn run(&mut self) -> bool {
        match self.complete.take() {
            Some(complete) => complete(),
            None => true,
        }
    }
}

impl Drop for DmaMappingCompletion {
    fn drop(&mut self) {
        // Safety net: a caller that forgets to complete must still attempt the
        // invalidation. A drop site cannot report the result.
        let _ = self.run();
    }
}

/// MM-owned mapping-attribute transition, not DMA allocation or platform sharing.
#[kiface::interface]
pub trait DmaMappingIf {
    /// Applies one DMA mapping mode to an existing kernel virtual range.
    ///
    /// Requires initialized kernel address-space management and an exclusively
    /// owned, page-aligned allocation. The caller quiesces device access before
    /// restoring Normal and retains ownership until this operation completes.
    /// This does not allocate pages, share them with firmware or synchronize
    /// device DMA. The provider owns page-table locking and flag selection.
    ///
    /// Returns a completion when the provider detached remote invalidation from
    /// the page-table lock. The caller must complete it after releasing its own
    /// lock; dropping the completion also performs it, at the drop site.
    ///
    /// # Errors
    /// Propagates range validation and page-table protection failures.
    fn set_mode(
        vaddr: VirtAddr,
        size_bytes: usize,
        mode: DmaMappingMode,
    ) -> kerrno::KResult<Option<DmaMappingCompletion>>;
}

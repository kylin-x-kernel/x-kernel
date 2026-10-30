// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Resident page identities and the reference-state protocol used by page owners.

use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use strum::{IntoStaticStr, VariantArray};

use crate::PageMetaError;

/// Classification assigned to an allocation for diagnostic usage accounting.
///
/// The original class is retained until the allocation's last hold ends. It
/// does not describe the page's current users, hardware type, reclaimability,
/// content-access rights, or the strategy for returning it to inventory.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, VariantArray, IntoStaticStr)]
pub enum UsageKind {
    /// Heap allocations made by kernel Rust code.
    RustHeap,
    /// Virtual memory, usually used for user space.
    VirtMem,
    /// Page cache for file systems.
    PageCache,
    /// Page tables.
    PageTable,
    /// DMA memory.
    Dma,
    /// General kernel page allocations.
    Global,
}

/// Metadata for a physical base-page identity.
///
/// Identity survives allocation and release of its page contents. Optional
/// lifetime references coexist with raw allocator protocols. Neither lookup nor
/// an Unreferenced snapshot grants ownership. Representation remains private.
pub struct PageMeta {
    references: AtomicUsize,
    usage_kind: AtomicU8,
    is_permanent: bool,
}

impl PageMeta {
    pub(crate) const fn new() -> Self {
        Self {
            references: AtomicUsize::new(0),
            usage_kind: AtomicU8::new(UsageKind::Global as u8),
            is_permanent: false,
        }
    }

    pub(crate) fn mark_permanent(&mut self) {
        self.is_permanent = true;
    }

    /// Reports a permanent reservation installed before table publication.
    /// False does not establish that the page is free, including allocator-owned
    /// bitmap, heap or PCP pages whose ownership remains with that allocator.
    pub fn is_permanent(&self) -> bool {
        self.is_permanent
    }
}

const RETIRING: usize = usize::MAX;
const INITIALIZING: usize = usize::MAX - 1;
const MAX_REFERENCES: usize = usize::MAX - 2;

/// Snapshot of the optional page-lifetime reference protocol.
///
/// Unreferenced pages may still belong to a raw allocator user, heap or PCP.
/// A snapshot neither acquires ownership nor authorizes content access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReferenceState {
    /// No reference-protocol holders; this does not mean the page is free.
    Unreferenced,
    /// Metadata backing permanently excluded from allocator inventory.
    Permanent,
    /// Live holders of one allocation incarnation.
    Referenced {
        /// Number of outstanding holds, including raw transfers.
        references: usize,
    },
    /// An exclusive allocator owner is publishing allocation attributes.
    Initializing,
    /// The unique last holder is returning storage to its allocator.
    Retiring,
}

impl PageMeta {
    /// Returns a diagnostic snapshot without acquiring a page reference.
    pub fn reference_state(&self) -> ReferenceState {
        if self.is_permanent() {
            return ReferenceState::Permanent;
        }
        match self.references.load(Ordering::Acquire) {
            0 => ReferenceState::Unreferenced,
            INITIALIZING => ReferenceState::Initializing,
            RETIRING => ReferenceState::Retiring,
            references => ReferenceState::Referenced { references },
        }
    }

    /// Starts reference tracking for one exclusively allocated base page.
    ///
    /// # Errors
    /// Returns InvalidState if reference tracking or a permanent reservation
    /// already exists. The counter is unchanged on failure.
    ///
    /// # Safety
    /// The caller exclusively owns a newly allocated page described by self,
    /// excludes raw release, and has not published it or any reference to it.
    /// The caller must return storage only after its last reference retires.
    pub unsafe fn claim_reference(&self, kind: UsageKind) -> Result<(), PageMetaError> {
        if self.is_permanent() {
            return Err(PageMetaError::InvalidState);
        }
        self.references
            .compare_exchange(0, INITIALIZING, Ordering::Acquire, Ordering::Relaxed)
            .map_err(|_| PageMetaError::InvalidState)?;
        // The allocation category is initialized before the first hold is published.
        // The allocator's exclusive inventory ownership excludes old holders.
        self.usage_kind.store(kind as u8, Ordering::Relaxed);
        self.references.store(1, Ordering::Release);
        Ok(())
    }

    /// Adds a hold without allocating a separate control block.
    ///
    /// # Errors
    /// Returns ReferenceOverflow at the count limit, or InvalidState when no
    /// live reference exists; failure does not change the counter.
    ///
    /// # Safety
    /// The caller borrows an existing live hold to this allocation incarnation
    /// throughout this call, and must eventually release the new hold once.
    pub unsafe fn clone_reference(&self) -> Result<(), PageMetaError> {
        self.references
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |references| {
                (1..MAX_REFERENCES)
                    .contains(&references)
                    .then(|| references + 1)
            })
            .map(|_| ())
            .map_err(|references| {
                if references == MAX_REFERENCES {
                    PageMetaError::ReferenceOverflow
                } else {
                    PageMetaError::InvalidState
                }
            })
    }

    /// Consumes one hold, returning true only for the unique last holder.
    ///
    /// The last holder leaves the descriptor Retiring and must finish retirement
    /// before publishing the page to allocator inventory.
    ///
    /// # Safety
    /// Consume exactly one live hold to this allocation, after all accesses
    /// authorized by it and any required TLB/device completion have ended.
    ///
    /// # Panics
    /// Panics if the counter is not in a live-reference state.
    pub unsafe fn release_reference(&self) -> bool {
        let mut previous = self.references.load(Ordering::Relaxed);
        loop {
            assert!((1..=MAX_REFERENCES).contains(&previous));
            let next = if previous == 1 {
                RETIRING
            } else {
                previous - 1
            };
            // The last release acquires all prior holders' releases before
            // allocator inventory can expose this physical storage again.
            let ordering = if previous == 1 {
                Ordering::AcqRel
            } else {
                Ordering::Release
            };
            match self
                .references
                .compare_exchange_weak(previous, next, ordering, Ordering::Relaxed)
            {
                Ok(_) => return previous == 1,
                Err(current) => previous = current,
            }
        }
    }

    /// Ends retirement and returns the allocation's original accounting class.
    ///
    /// The caller must return storage to its inventory after this operation and
    /// must not modify the descriptor after publishing that storage for reuse.
    ///
    /// # Safety
    /// Only the unique holder that obtained true from release_reference may
    /// call this, exactly once, before making the page allocatable again.
    /// No page access or reference creation may race with retirement.
    ///
    /// # Panics
    /// Panics if the descriptor is not Retiring.
    pub unsafe fn finish_retirement(&self) -> UsageKind {
        assert_eq!(self.references.load(Ordering::Relaxed), RETIRING);
        // claim_reference publishes a valid class before the first hold. The
        // retiring owner snapshots it before inventory can start another round.
        let kind = UsageKind::VARIANTS[self.usage_kind.load(Ordering::Relaxed) as usize];
        self.references.store(0, Ordering::Release);
        kind
    }
}

unittest::path_mod_test!(tests, "../ut-tests/page.rs");

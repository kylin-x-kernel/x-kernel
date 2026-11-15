// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Resident page identities and the reference-state protocol used by page owners.

use core::sync::atomic::{AtomicU8, AtomicU16, AtomicU32, AtomicUsize, Ordering};

use strum::{IntoStaticStr, VariantArray};

use crate::PageMetaError;

mod group;
pub use group::AllocationInfo;

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
/// lifetime references coexist with raw allocator protocols. Head offsets and
/// allocation order describe the current reference-owned group. Neither lookup nor
/// an Unreferenced snapshot grants ownership. Representation remains private.
pub struct PageMeta {
    references: AtomicUsize,
    /// Offset from the allocation head, in pages. The page-meta table covers
    /// guest memory in 4 KiB units; 16 bits bound a single allocation to
    /// 65535 tail pages, which exceeds the allocator's supported order.
    head_offset_pages: AtomicU16,
    usage_kind: AtomicU8,
    /// Allocation order in the low 7 bits; bit 7 is the permanent flag.
    allocation_order_and_flags: AtomicU8,
    /// Owning memory cgroup. The full 32-bit id is retained so a valid cgroup
    /// cannot alias the root or another live cgroup.
    memcg: AtomicU32,
}

const ALLOCATION_ORDER_MASK: u8 = (1 << 7) - 1;
const PERMANENT_FLAG: u8 = 1 << 7;

impl PageMeta {
    pub(crate) const fn new() -> Self {
        Self {
            references: AtomicUsize::new(0),
            head_offset_pages: AtomicU16::new(0),
            usage_kind: AtomicU8::new(UsageKind::Global as u8),
            allocation_order_and_flags: AtomicU8::new(0),
            memcg: AtomicU32::new(0),
        }
    }

    pub(crate) fn mark_permanent(&mut self) {
        self.allocation_order_and_flags
            .fetch_or(PERMANENT_FLAG, Ordering::Relaxed);
    }

    /// Reports a permanent reservation installed before table publication.
    /// False does not establish that the page is free, including allocator-owned
    /// bitmap, heap or PCP pages whose ownership remains with that allocator.
    pub fn is_permanent(&self) -> bool {
        self.allocation_order_and_flags.load(Ordering::Relaxed) & PERMANENT_FLAG != 0
    }

    /// Records the memory cgroup that owns this page's charge.
    ///
    /// The allocator's exclusive ownership of a fresh page serializes this
    /// store against any concurrent lookup.
    pub fn set_memcg(&self, id: u32) {
        self.memcg.store(id, Ordering::Release);
    }

    /// Returns the recorded memory cgroup, or zero when uncharged.
    pub fn memcg(&self) -> u32 {
        self.memcg.load(Ordering::Acquire)
    }

    /// Returns the recorded allocation order.
    pub fn allocation_order(&self) -> u8 {
        self.allocation_order_and_flags.load(Ordering::Relaxed) & ALLOCATION_ORDER_MASK
    }

    /// Records the allocation order. Callers own exclusivity of the head page.
    pub fn set_allocation_order(&self, order: u8) {
        debug_assert!(order <= ALLOCATION_ORDER_MASK);
        let flags = self.allocation_order_and_flags.load(Ordering::Relaxed) & PERMANENT_FLAG;
        self.allocation_order_and_flags
            .store(flags | order, Ordering::Relaxed);
    }

    /// Returns the allocation class recorded for this page incarnation.
    pub fn usage_kind(&self) -> UsageKind {
        UsageKind::VARIANTS[self.usage_kind.load(Ordering::Relaxed) as usize]
    }

    /// Records the allocation class for this page incarnation.
    ///
    /// The allocator's exclusive ownership of a fresh page serializes this
    /// store against concurrent readers. The class remains stable until the
    /// page is returned to inventory and a new allocation publishes its own.
    pub fn set_usage_kind(&self, kind: UsageKind) {
        self.usage_kind.store(kind as u8, Ordering::Relaxed);
    }

    /// Claims the recorded memory cgroup for a release.
    ///
    /// The page's exclusive owner calls this exactly once before returning the
    /// page to inventory, so the charge cannot be released twice.
    pub fn take_memcg(&self) -> u32 {
        self.memcg.swap(0, Ordering::AcqRel)
    }
}

const RETIRING: usize = usize::MAX;
const INITIALIZING: usize = usize::MAX - 1;
const SPLITTING: usize = usize::MAX - 2;
const MAX_REFERENCES: usize = usize::MAX - 3;

#[cfg(target_pointer_width = "64")]
const _: () = assert!(core::mem::size_of::<PageMeta>() == 16);

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
    /// A member of a multi-page allocation; its head carries the count.
    Tail,
    /// An exclusive holder is converting an allocation into independent pages.
    Splitting,
    /// Live holders counted at a single page or allocation head.
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
    /// Returns a best-effort diagnostic snapshot without acquiring a reference.
    /// Concurrent admission, splitting or retirement can change the role; this
    /// is not a coherent multi-field snapshot or authority to access contents.
    pub fn reference_state(&self) -> ReferenceState {
        if self.is_permanent() {
            return ReferenceState::Permanent;
        }
        if self.head_offset_pages.load(Ordering::Acquire) != 0 {
            return ReferenceState::Tail;
        }
        match self.references.load(Ordering::Acquire) {
            0 => ReferenceState::Unreferenced,
            INITIALIZING => ReferenceState::Initializing,
            RETIRING => ReferenceState::Retiring,
            SPLITTING => ReferenceState::Splitting,
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
    unsafe fn claim_reference(&self, kind: UsageKind) -> Result<(), PageMetaError> {
        self.begin_reference()?;
        self.usage_kind.store(kind as u8, Ordering::Relaxed);
        // Publishes this independent page's attributes to holders.
        self.references.store(1, Ordering::Release);
        Ok(())
    }

    fn begin_reference(&self) -> Result<(), PageMetaError> {
        if !self.is_unreferenced() {
            return Err(PageMetaError::InvalidState);
        }
        self.references
            .compare_exchange(0, INITIALIZING, Ordering::Acquire, Ordering::Relaxed)
            .map_err(|_| PageMetaError::InvalidState)?;
        Ok(())
    }

    fn is_unreferenced(&self) -> bool {
        // Admission observes Release stores that publish or clear tail links,
        // and the Release count reset in finish_retirement. These Acquire loads
        // do not acquire ownership or form an atomic snapshot. The caller owns
        // exclusive unpublished backing, so no geometry writer can race the
        // Relaxed order check after inventory handoff.
        !self.is_permanent()
            && self.head_offset_pages.load(Ordering::Acquire) == 0
            && self.references.load(Ordering::Acquire) == 0
            && self.allocation_order() == 0
    }

    /// Adds a hold at an allocation head without another control block.
    ///
    /// # Errors
    /// Returns ReferenceOverflow at the count limit, or InvalidState when no
    /// live reference exists; failure does not change the counter.
    ///
    /// # Safety
    /// self must be the allocation head. The caller borrows an existing live
    /// hold to this allocation incarnation
    /// throughout this call, and must eventually release the new hold once.
    pub unsafe fn clone_reference(&self) -> Result<(), PageMetaError> {
        debug_assert_eq!(self.head_offset_pages.load(Ordering::Relaxed), 0);
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
    /// self must be the allocation head. Consume exactly one live hold, after all accesses
    /// authorized by it and any required TLB/device completion have ended.
    ///
    /// # Panics
    /// Panics if the counter is not in a live-reference state.
    pub unsafe fn release_reference(&self) -> bool {
        debug_assert_eq!(self.head_offset_pages.load(Ordering::Relaxed), 0);
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
    /// self must be an independent page (order zero, no tail relationship).
    /// Multi-page holders must use PageMetaTable::finish_reference_retirement.
    /// Only the unique holder that obtained true from release_reference may
    /// call this, exactly once, before making the page allocatable again.
    /// No page access or reference creation may race with retirement.
    ///
    /// # Panics
    /// Panics if the descriptor is not Retiring.
    unsafe fn finish_retirement(&self) -> UsageKind {
        assert_eq!(self.head_offset_pages.load(Ordering::Relaxed), 0);
        assert_eq!(self.allocation_order(), 0);
        assert_eq!(self.references.load(Ordering::Relaxed), RETIRING);
        // claim_reference publishes a valid class before the first hold. The
        // retiring owner snapshots it before inventory can start another round.
        let kind = UsageKind::VARIANTS[self.usage_kind.load(Ordering::Relaxed) as usize];
        self.references.store(0, Ordering::Release);
        kind
    }
}

unittest::path_mod_test!(tests, "../ut-tests/page.rs");

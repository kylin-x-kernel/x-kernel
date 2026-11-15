// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Contiguous reference ranges and explicit exact/compound allocation helpers.

use core::{fmt, mem::ManuallyDrop};

use alloc_engine::{AllocError, AllocResult};
use memaddr::{PAGE_SIZE_4K, PhysAddr, PhysAddrRange};
use page_meta::{AllocationInfo, PageMeta, PageMetaError, PageMetaTable};

use crate::{UsageKind, global_allocator};

mod split;
pub use split::SplitPages;

/// Owned lifetime protection for a nonempty contiguous range of physical pages.
///
/// Exact ranges hold one independent reference per base page. Compound views
/// hold one reference on the whole compound, even when selecting a subrange.
/// Every range is entirely independent or contained in a single compound;
/// resident metadata selects the protocol, while this handle records its bounds.
/// A one-page compound request is the same order-zero unit as an exact page.
/// Clone allocates no array/control block. This grants no mapping, exclusive
/// content access, PTE rights or hardware retirement. Holders must outlive all
/// required content access and TLB/device completion.
///
/// Admission/release requires initialized global metadata, inventory, direct
/// mapping and per-CPU storage. Operations do not sleep and support ordinary/IRQ
/// context, never NMI. Independent clone/drop and compound admission/retirement
/// visit each relevant descriptor; IRQ callers must bound the range size.
pub struct PageRef {
    first: &'static PageMeta,
    num_pages: usize,
}

const _: () = assert!(core::mem::size_of::<PageRef>() == 2 * core::mem::size_of::<usize>());

impl PageRef {
    /// Admits a complete new exclusive unpublished range from global inventory.
    ///
    /// # Errors
    /// Returns geometry/coverage errors without acquiring any hold.
    ///
    /// # Panics
    /// Panics if the allocator supplied referenced or permanently reserved pages.
    ///
    /// # Safety
    /// The caller owns exactly num_pages exclusive physical base pages beginning
    /// at paddr. No prior holder/raw user may access or release them. Compound
    /// counts must be a power of two. The original kind follows raw accounting.
    /// Success transfers every admitted hold; ordinary errors retain raw ownership.
    unsafe fn from_allocated(
        paddr: PhysAddr,
        num_pages: usize,
        kind: UsageKind,
        is_compound: bool,
    ) -> Result<Self, PageMetaError> {
        if !paddr.as_usize().is_multiple_of(PAGE_SIZE_4K) {
            return Err(PageMetaError::InvalidRange);
        }
        let table = page_meta::global_table().ok_or(PageMetaError::NotCovered)?;
        let first_pfn = paddr.as_usize() / PAGE_SIZE_4K;
        let claimed = if is_compound {
            if !num_pages.is_power_of_two() {
                return Err(PageMetaError::InvalidRange);
            }
            // SAFETY: the caller supplied this complete exclusive unpublished
            // power-of-two allocation, with no prior holders or raw users.
            unsafe {
                table.claim_reference_group(first_pfn, num_pages.trailing_zeros() as u8, kind)
            }
        } else {
            // SAFETY: the caller supplied the exact exclusive unpublished range;
            // admission transfers one independent hold for every requested page.
            unsafe { table.claim_reference_range(first_pfn, num_pages, kind) }
        };
        assert!(
            !matches!(claimed, Err(PageMetaError::InvalidState)),
            "allocator returned referenced/reserved backing"
        );
        Ok(Self {
            first: claimed?,
            num_pages,
        })
    }

    /// Returns the first selected physical base-page number.
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: admission and restoration select resident \
                  descriptors from the immutable global table"
    )]
    pub fn pfn(&self) -> usize {
        page_meta::pfn_of(self.first).expect("live range must have a global PFN")
    }

    /// Returns the beginning of this handle's selected range without content access.
    pub fn phys_addr(&self) -> PhysAddr {
        (self.pfn() * PAGE_SIZE_4K).into()
    }

    /// Returns this handle's selected page count, not its compound backing size.
    pub fn page_count(&self) -> usize {
        self.num_pages
    }

    /// Returns the complete physical range this hold keeps alive.
    /// Independent holds retain exactly their selected range; compound views can
    /// retain a larger allocation. The returned addresses grant no content access.
    pub fn retained_range(&self) -> PhysAddrRange {
        let unit = self.reference_unit();
        if unit.order() == 0 {
            PhysAddrRange::from_start_size(self.phys_addr(), self.num_pages * PAGE_SIZE_4K)
        } else {
            PhysAddrRange::from_start_size(
                (unit.head_pfn() * PAGE_SIZE_4K).into(),
                unit.page_count() * PAGE_SIZE_4K,
            )
        }
    }

    /// Clones the holds protecting the same selected range without a heap allocation.
    /// Independent ranges increment every page; compound views increment one head.
    ///
    /// # Errors
    /// Returns ReferenceOverflow at any count limit. Successfully cloned prefix
    /// holds are released on failure, leaving all original holds unchanged.
    pub fn try_clone_ref(&self) -> Result<Self, PageMetaError> {
        // SAFETY: self retains every hold in an admitted/restored valid range.
        // Metadata shape remains stable until final release or consuming split.
        let result = unsafe { metadata_table().clone_reference_range(self.first, self.num_pages) };
        assert_ne!(
            result,
            Err(PageMetaError::InvalidState),
            "live range lost its state"
        );
        result?;
        Ok(Self {
            first: self.first,
            num_pages: self.num_pages,
        })
    }

    /// Clones lifetime protection for a nonempty subrange of this selected range.
    ///
    /// Independent subranges retain only their own pages; compound subranges
    /// still retain the whole backing. offset_pages is relative to this handle.
    ///
    /// # Errors
    /// Returns InvalidRange for an empty/out-of-bounds view or ReferenceOverflow.
    pub fn try_clone_range(
        &self,
        offset_pages: usize,
        num_pages: usize,
    ) -> Result<Self, PageMetaError> {
        if num_pages == 0
            || offset_pages
                .checked_add(num_pages)
                .is_none_or(|end| end > self.num_pages)
        {
            return Err(PageMetaError::InvalidRange);
        }
        // A non-dropping view borrows existing holds from self. Only the result
        // of try_clone_ref owns new counts; the view never consumes our holds.
        let view = ManuallyDrop::new(Self {
            first: global_meta(self.pfn() + offset_pages),
            num_pages,
        });
        view.try_clone_ref()
    }

    /// Clones a one-page view indexed from this handle's first page.
    ///
    /// # Errors
    /// Returns InvalidRange for an out-of-bounds index or ReferenceOverflow.
    pub fn try_clone_page(&self, index: usize) -> Result<Self, PageMetaError> {
        self.try_clone_range(index, 1)
    }

    /// Consumes this range and yields independently held base pages.
    ///
    /// Exact ranges transfer existing holds without changing counts, including
    /// when other independent holds exist. Multi-page compounds must be exclusive:
    /// the whole backing is split and yielded, including pages outside the view.
    /// No content is copied and no allocation charge is repeated. The iterator
    /// releases every child not taken by the caller.
    ///
    /// # Errors
    /// Returns (original_owner, NotExclusive) when a compound has other holds.
    /// Any error returns the unchanged original owner and bounds.
    ///
    /// # Safety
    /// Before physically splitting a compound, end all content accesses, mappings,
    /// device use and required translation/device retirement for its full backing.
    /// The count check cannot prove these facts. Keep every yielded child alive
    /// through the uses it protects. Exact transfer alone changes no page geometry.
    pub unsafe fn try_split(self) -> Result<SplitPages, (Self, PageMetaError)> {
        let unit = self.reference_unit();
        if unit.order() == 0 {
            let split = SplitPages::new(self.pfn(), self.num_pages);
            core::mem::forget(self);
            return Ok(split);
        }
        // SAFETY: self retains the compound hold. The caller ended external use;
        // success transfers all independent child holds into the output iterator.
        let result = unsafe { metadata_table().split_reference(self.first) };
        match result {
            Ok(allocation) => {
                core::mem::forget(self);
                Ok(SplitPages::new(
                    allocation.head_pfn(),
                    allocation.page_count(),
                ))
            }
            Err(error) => Err((self, error)),
        }
    }

    /// Transfers all this handle's holds as its selected PA and page count.
    /// No count changes occur. Restore the exact pair once with from_raw;
    /// a bare PA cannot recover an independent range's length.
    pub fn into_raw(self) -> (PhysAddr, usize) {
        let owner = ManuallyDrop::new(self);
        (owner.phys_addr(), owner.num_pages)
    }

    /// Restores one exact transferred reference range without changing counts.
    /// Metadata selects independent or compound handling; num_pages preserves
    /// the selected bounds, not necessarily the compound's complete size.
    ///
    /// # Safety
    /// The pair must be one unconsumed into_raw transfer from the same allocation
    /// incarnation. It owns one hold per independent page or one compound hold
    /// covering that view. Keep these holds through all required access and
    /// retirement. A descriptor lookup/fresh raw allocation cannot mint ownership.
    ///
    /// # Panics
    /// Panics for invalid geometry, missing metadata, mixed units or dead state.
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: the exact outstanding transfer supplies \
                  its validated nonempty bounds and live, stable reference units"
    )]
    pub unsafe fn from_raw(paddr: PhysAddr, num_pages: usize) -> Self {
        assert!(paddr.as_usize().is_multiple_of(PAGE_SIZE_4K));
        assert_eq!(checked_page_count(num_pages), Ok(()));
        let first = page_meta::lookup(paddr.as_usize() / PAGE_SIZE_4K)
            .expect("raw range requires global metadata");
        // SAFETY: the transferred holds retain all selected pages/compound and
        // exclude physical retirement/splitting until this restoration finishes.
        unsafe { metadata_table().reference_range(first, num_pages) }
            .expect("raw transfer requires a valid live range");
        Self { first, num_pages }
    }

    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: this live range retains its first \
                  reference unit and excludes retirement or consuming compound split"
    )]
    fn reference_unit(&self) -> AllocationInfo {
        // SAFETY: self retains its admitted/restored first unit throughout this
        // query. The independent/compound shape is immutable while held.
        unsafe { metadata_table().reference_allocation(self.first) }
            .expect("live range lost reference geometry")
    }
}

impl fmt::Debug for PageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PageRef")
            .field("first_pfn", &self.pfn())
            .field("num_pages", &self.num_pages)
            .field("reference_unit", &self.reference_unit())
            .finish()
    }
}

impl Drop for PageRef {
    fn drop(&mut self) {
        let unit = self.reference_unit();
        if unit.order() != 0 || self.num_pages == 1 {
            release_unit(unit);
            return;
        }
        // Capture the base before any release; the first page can be reused as
        // soon as its hold ends. Later pages remain protected by our own holds.
        let first_pfn = self.pfn();
        for index in 0..self.num_pages {
            drop(Self {
                first: global_meta(first_pfn + index),
                num_pages: 1,
            });
        }
    }
}

fn release_unit(unit: AllocationInfo) {
    let head = global_meta(unit.head_pfn());
    // SAFETY: the caller transfers one existing hold after all protected use.
    // The unit geometry was saved before decrementing its sole head counter.
    if unsafe { head.release_reference() } {
        memacct::uncharge_frame(
            PhysAddr::from(unit.head_pfn() * PAGE_SIZE_4K),
            unit.page_count() * PAGE_SIZE_4K,
        );
        // SAFETY: true selected the unique retiring holder; all links are cleared
        // before storage becomes reusable and no old metadata write follows.
        let kind = unsafe { metadata_table().finish_reference_retirement(unit.head_pfn()) };
        global_allocator().dealloc_pages(
            kaddr_layout::p2v(unit.head_pfn() * PAGE_SIZE_4K),
            unit.page_count(),
            kind,
        );
    }
}

#[expect(
    clippy::expect_used,
    reason = "CG-021-no-panic-on-reachable-failure: every range constructor requires the \
              boot-published immutable table, whose backing is retained forever"
)]
fn metadata_table() -> &'static PageMetaTable {
    page_meta::global_table().expect("live range requires global metadata")
}

#[expect(
    clippy::expect_used,
    reason = "CG-021-no-panic-on-reachable-failure: admission/restoration validates coverage and \
              every live or transferred hold keeps its reference unit described"
)]
fn global_meta(pfn: usize) -> &'static PageMeta {
    page_meta::lookup(pfn).expect("live range must remain fully described")
}

fn checked_page_count(num_pages: usize) -> AllocResult<()> {
    let size_bytes = num_pages
        .checked_mul(PAGE_SIZE_4K)
        .ok_or(AllocError::InvalidInput)?;
    if num_pages == 0 || size_bytes > isize::MAX as usize {
        return Err(AllocError::InvalidInput);
    }
    Ok(())
}

/// Allocates an uninitialized independently referenced base page.
///
/// # Errors
/// Returns the initialization, geometry or inventory errors of alloc_pages_exact_ref.
pub fn alloc_page_ref(kind: UsageKind) -> AllocResult<PageRef> {
    alloc_pages_exact_ref(1, PAGE_SIZE_4K, kind)
}

/// Allocates exactly num_pages contiguous, independently referenced base pages.
///
/// One PageRef owns the complete selected range without a heap array. Every page
/// has its own counter. A subrange hold does not retain other pages. The complete
/// rounded raw backing is obtained first, then unused tail pages are returned
/// before publication; accounting charges only retained pages.
///
/// # Errors
/// Returns InvalidInput for empty/overflowing counts, invalid alignment/mapping,
/// NotInitialized for missing inventory/metadata, or the inventory allocation error.
/// Admission errors return the complete unpublished raw backing.
pub fn alloc_pages_exact_ref(
    num_pages: usize,
    align_pow2: usize,
    kind: UsageKind,
) -> AllocResult<PageRef> {
    allocate_range(num_pages, align_pow2, kind, false)
}

/// Explicitly allocates one shared reference unit of contiguous physical pages.
///
/// num_pages must be a power of two. Cloning any subrange retains the whole
/// compound; no per-page independent holds exist until an exclusive split.
/// The returned handle initially selects the complete uninitialized backing.
///
/// # Errors
/// Returns InvalidInput for non-power-of-two/overflowing counts or invalid
/// alignment/mapping, NotInitialized for missing inventory/metadata, or the
/// inventory allocation error. Admission failure returns all raw backing.
pub fn alloc_compound_pages_ref(
    num_pages: usize,
    align_pow2: usize,
    kind: UsageKind,
) -> AllocResult<PageRef> {
    if !num_pages.is_power_of_two() {
        return Err(AllocError::InvalidInput);
    }
    allocate_range(num_pages, align_pow2, kind, true)
}

fn allocate_range(
    num_pages: usize,
    align_pow2: usize,
    kind: UsageKind,
    is_compound: bool,
) -> AllocResult<PageRef> {
    checked_page_count(num_pages)?;
    if !align_pow2.is_power_of_two() {
        return Err(AllocError::InvalidInput);
    }
    let backing_pages = num_pages
        .checked_next_power_of_two()
        .ok_or(AllocError::InvalidInput)?;
    checked_page_count(backing_pages)?;
    if is_compound {
        u32::try_from(backing_pages - 1).map_err(|_| AllocError::InvalidInput)?;
    }
    let allocator = global_allocator();
    if !allocator.is_page_allocator_ready() || page_meta::global_table().is_none() {
        return Err(AllocError::NotInitialized);
    }
    let va = allocator.alloc_pages(backing_pages, align_pow2.max(PAGE_SIZE_4K), kind)?;
    let paddr = PhysAddr::from(kaddr_layout::v2p(va));
    // Charge before constructing PageRef: failure after publication would make
    // the drop path account a deallocation that was never allocated for
    // RustHeap and would leave spare-page usage behind.
    let is_charged = kind != UsageKind::PageTable;
    if is_charged && memacct::charge_frame(paddr, num_pages * PAGE_SIZE_4K, kind).is_err() {
        if kind != UsageKind::RustHeap {
            allocator.record_usage_dealloc(kind, backing_pages * PAGE_SIZE_4K);
        }
        allocator.release_pages_to_inventory(va, backing_pages);
        return Err(AllocError::NoMemory);
    }
    let result = if kaddr_layout::p2v(paddr.as_usize()) != va {
        Err(PageMetaError::InvalidRange)
    } else {
        // SAFETY: raw inventory supplied this complete exclusive unpublished
        // backing. Requested pages are a prefix, under the raw accounting class.
        unsafe { PageRef::from_allocated(paddr, num_pages, kind, is_compound) }
    };
    match result {
        Ok(page) => {
            let spare_pages = backing_pages - num_pages;
            if kind == UsageKind::RustHeap {
                // Raw heap backing skips this charge; reference ranges do not.
                allocator.record_usage_alloc(kind, num_pages * PAGE_SIZE_4K);
            } else if spare_pages != 0 {
                allocator.record_usage_dealloc(kind, spare_pages * PAGE_SIZE_4K);
            }
            // Every spare page is unreferenced raw inventory. One-page returns
            // avoid rounding an arbitrary tail length beyond the owned backing.
            for index in num_pages..backing_pages {
                allocator.release_pages_to_inventory(va + index * PAGE_SIZE_4K, 1);
            }
            Ok(page)
        }
        Err(error) => {
            if is_charged {
                memacct::uncharge_frame(paddr, num_pages * PAGE_SIZE_4K);
            }
            if kind != UsageKind::RustHeap {
                allocator.record_usage_dealloc(kind, backing_pages * PAGE_SIZE_4K);
            }
            allocator.release_pages_to_inventory(va, backing_pages);
            Err(match error {
                PageMetaError::NotCovered => AllocError::NotInitialized,
                _ => AllocError::InvalidInput,
            })
        }
    }
}

/// Allocates a zeroed independently referenced base page before publication.
///
/// # Errors
/// Returns the errors of alloc_zeroed_pages_exact_ref.
pub fn alloc_zeroed_page_ref(kind: UsageKind) -> AllocResult<PageRef> {
    alloc_zeroed_pages_exact_ref(1, PAGE_SIZE_4K, kind)
}

/// Allocates exactly the requested contiguous pages and clears their contents.
///
/// Uses alloc_pages_exact_ref's independent lifecycle. Zeroing completes before
/// the first range holder escapes; unused raw backing was already returned.
///
/// # Errors
/// Returns the initialization, geometry and inventory errors of alloc_pages_exact_ref.
pub fn alloc_zeroed_pages_exact_ref(
    num_pages: usize,
    align_pow2: usize,
    kind: UsageKind,
) -> AllocResult<PageRef> {
    let page = alloc_pages_exact_ref(num_pages, align_pow2, kind)?;
    let address = kaddr_layout::p2v(page.phys_addr().as_usize());
    // SAFETY: the new selected range is exclusive, unpublished, fully writable
    // direct-map RAM; no aliases or cloned holds escaped, and size was checked.
    unsafe { super::zero::zero_owned_memory(address as *mut u8, page.num_pages * PAGE_SIZE_4K) };
    Ok(page)
}

unittest::path_mod_test!(tests, "../ut-tests/page_reference.rs");
unittest::path_mod_test!(tests_allocation, "../ut-tests/page_allocation.rs");

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Independent reference ranges and compound head/tail lifetime transitions.

use core::sync::atomic::Ordering;

use memaddr::PAGE_SIZE_4K;

use super::{MAX_REFERENCES, PageMeta, RETIRING, SPLITTING, UsageKind};
use crate::{PageMetaError, PageMetaTable};

/// Geometry of one allocation, not a page hold or content-access capability.
/// The snapshot is valid for lifetime operations only while a hold is retained.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AllocationInfo {
    head_pfn: usize,
    order: u8,
}

impl AllocationInfo {
    /// Returns the first physical base-page number in the allocation.
    pub fn head_pfn(self) -> usize {
        self.head_pfn
    }

    /// Returns the base-two allocation order; zero denotes an independent page.
    pub fn order(self) -> u8 {
        self.order
    }

    /// Returns the actual number of backing pages, including rounding slack.
    pub fn page_count(self) -> usize {
        1usize << self.order
    }
}

impl PageMetaTable {
    /// Establishes one independent reference on every page in a new range.
    ///
    /// # Errors
    /// Returns InvalidRange for empty/overflowing geometry, NotCovered for a
    /// hole, or InvalidState for reserved/already-owned backing. Every ordinary
    /// failure leaves the complete range unchanged.
    ///
    /// # Safety
    /// The caller owns the complete exclusive unpublished physical range and
    /// excludes prior holders/raw release. Success transfers one lifetime hold
    /// per page; the caller must eventually consume every hold exactly once.
    pub unsafe fn claim_reference_range(
        &self,
        first_pfn: usize,
        num_pages: usize,
        kind: UsageKind,
    ) -> Result<&PageMeta, PageMetaError> {
        self.validate_unclaimed_range(first_pfn, num_pages)?;
        for index in 0..num_pages {
            let meta = self.group_meta(first_pfn + index);
            // SAFETY: validation covered all independent unclaimed descriptors,
            // and the caller excludes publication or competing inventory users.
            let claimed = unsafe { meta.claim_reference(kind) };
            assert_eq!(claimed, Ok(()), "exclusive reference admission changed");
        }
        Ok(self.group_meta(first_pfn))
    }

    /// Validates the shape of a range already protected by transferred holds.
    /// Returns the first page's reference unit, not a new hold.
    ///
    /// # Errors
    /// Returns InvalidRange for invalid geometry or a view crossing its compound
    /// boundary, NotCovered for missing metadata, or InvalidState for mixed units.
    ///
    /// # Safety
    /// Retain one existing hold per independent page, or one compound hold
    /// covering the entire range, throughout validation. Bare lookup supplies
    /// no ownership evidence, and atomic observations cannot acquire holds.
    pub unsafe fn reference_range(
        &self,
        first: &PageMeta,
        num_pages: usize,
    ) -> Result<AllocationInfo, PageMetaError> {
        let first_pfn = self.pfn_of(first).ok_or(PageMetaError::NotCovered)?;
        range_geometry(first_pfn, num_pages)?;
        // SAFETY: the caller retains the transferred hold on the first page.
        let unit = unsafe { self.reference_allocation(first) }?;
        if unit.order != 0 {
            let offset = first_pfn - unit.head_pfn;
            if num_pages > unit.page_count() - offset {
                return Err(PageMetaError::InvalidRange);
            }
        } else {
            for index in 1..num_pages {
                let meta = self
                    .lookup(first_pfn + index)
                    .ok_or(PageMetaError::NotCovered)?;
                // SAFETY: the caller retains a separate hold for each member of
                // this independent range until the complete check finishes.
                let next = unsafe { self.reference_allocation(meta) }?;
                if next.order != 0 {
                    return Err(PageMetaError::InvalidState);
                }
            }
        }
        Ok(unit)
    }

    /// Adds the holds protecting the same already-validated reference range.
    /// Compound views add one head hold; independent ranges add one per page.
    ///
    /// # Errors
    /// Returns ReferenceOverflow if any count is full, or a first-unit geometry/
    /// state error. An increment failure releases the successful prefix before
    /// return. Existing holds and allocation accounting remain unchanged.
    ///
    /// # Safety
    /// The caller retains the complete range's existing holds and previously
    /// validated its shape with admission/reference_range. It is either wholly
    /// independent or contained in one compound. Consume every new hold once.
    pub unsafe fn clone_reference_range(
        &self,
        first: &PageMeta,
        num_pages: usize,
    ) -> Result<(), PageMetaError> {
        let first_pfn = self.pfn_of(first).ok_or(PageMetaError::NotCovered)?;
        range_geometry(first_pfn, num_pages)?;
        // SAFETY: the caller retains its first unit's original hold throughout.
        let unit = unsafe { self.reference_allocation(first) }?;
        if unit.order != 0 {
            if num_pages > unit.page_count() - (first_pfn - unit.head_pfn) {
                return Err(PageMetaError::InvalidRange);
            }
            // SAFETY: this retained compound hold covers the validated view;
            // the successful increment supplies one additional whole hold.
            return unsafe { self.group_meta(unit.head_pfn).clone_reference() };
        }
        for index in 0..num_pages {
            let meta = self.group_meta(first_pfn + index);
            debug_assert_eq!(meta.head_offset_pages.load(Ordering::Relaxed), 0);
            debug_assert_eq!(meta.allocation_order(), 0);
            // SAFETY: the caller retains one original independent hold per page.
            let result = unsafe { meta.clone_reference() };
            if let Err(error) = result {
                for rollback in 0..index {
                    let acquired = self.group_meta(first_pfn + rollback);
                    // SAFETY: consumes exactly the added prefix hold. The caller
                    // still retains its original hold, so retirement is impossible.
                    let is_last = unsafe { acquired.release_reference() };
                    assert!(!is_last, "clone rollback lost the original hold");
                }
                return Err(error);
            }
        }
        Ok(())
    }

    /// Starts one whole-allocation hold on exclusive unpublished backing.
    ///
    /// Head state is published only after every tail offset is established.
    /// No allocator, mapping or accounting operation is performed here.
    ///
    /// # Errors
    /// Returns InvalidRange for unencodable/overflowing geometry, NotCovered
    /// for a physical hole, or InvalidState for reserved/already-owned backing.
    /// Every ordinary error leaves all descriptors unchanged.
    ///
    /// # Safety
    /// The caller exclusively owns exactly 2^order newly allocated base pages
    /// starting at head_pfn. Exclude old holders, raw release and publication
    /// until success; on success return storage only after final retirement.
    pub unsafe fn claim_reference_group(
        &self,
        head_pfn: usize,
        order: u8,
        kind: UsageKind,
    ) -> Result<&PageMeta, PageMetaError> {
        let allocation = geometry(head_pfn, order)?;
        if allocation.page_count() > usize::from(u16::MAX) + 1 {
            return Err(PageMetaError::InvalidRange);
        }
        self.validate_unclaimed_range(head_pfn, allocation.page_count())?;
        let head = self.group_meta(head_pfn);
        if order == 0 {
            // SAFETY: the admission pass validated this exclusive independent
            // page, and the caller has not published the new allocation.
            unsafe { head.claim_reference(kind) }?;
            return Ok(head);
        }
        head.begin_reference()?;
        head.set_allocation_order(order);
        head.usage_kind.store(kind as u8, Ordering::Relaxed);
        for index in 1..allocation.page_count() {
            let tail = self.group_meta(head_pfn + index);
            // geometry proved every offset fits u16; exclusive admission keeps
            // it stable until the last hold retires or an exclusive split.
            tail.head_offset_pages
                .store(index as u16, Ordering::Release);
        }
        // Publishes head attributes and all tail links. Raw lookup cannot claim
        // ownership; the allocator returns its first holder only after this.
        head.references.store(1, Ordering::Release);
        Ok(head)
    }

    /// Resolves a selected base page to its stable allocation geometry.
    ///
    /// # Errors
    /// Returns NotCovered for a foreign descriptor or missing head, InvalidRange
    /// for invalid geometry, or InvalidState outside a live allocation.
    ///
    /// # Safety
    /// The caller retains an existing hold on this allocation incarnation for
    /// the whole operation. Lookup alone cannot establish that hold. Such a hold
    /// excludes both final retirement and the exclusive splitting transition.
    pub unsafe fn reference_allocation(
        &self,
        meta: &PageMeta,
    ) -> Result<AllocationInfo, PageMetaError> {
        let pfn = self.pfn_of(meta).ok_or(PageMetaError::NotCovered)?;
        // A resolving holder acquires claim's Release head count, which
        // publishes attributes and tail links. Split children instead acquire
        // the Release tail-link clear or head count that publishes their
        // independent state. The retained hold excludes retirement and further
        // splitting, so the published allocation_order stays stable for the
        // Relaxed read below.
        let distance = usize::from(meta.head_offset_pages.load(Ordering::Acquire));
        let head_pfn = pfn
            .checked_sub(distance)
            .ok_or(PageMetaError::InvalidRange)?;
        let head = self.lookup(head_pfn).ok_or(PageMetaError::NotCovered)?;
        if head.is_permanent()
            || head.head_offset_pages.load(Ordering::Acquire) != 0
            || !(1..=MAX_REFERENCES).contains(&head.references.load(Ordering::Acquire))
        {
            return Err(PageMetaError::InvalidState);
        }
        let allocation = geometry(head_pfn, head.allocation_order())?;
        if distance >= allocation.page_count() {
            return Err(PageMetaError::InvalidRange);
        }
        Ok(allocation)
    }

    /// Clears a uniquely retired allocation before the allocator reuses it.
    /// Returns the original accounting class; no inventory is returned here.
    ///
    /// # Safety
    /// head_pfn must be the head whose release_reference just returned true to
    /// this caller. Consume that retirement exactly once, after all content and
    /// hardware use. Preserve the geometry before calling, and return storage
    /// only after this function completes; never write old metadata afterward.
    ///
    /// # Panics
    /// Panics if the head is not Retiring or no longer has its validated geometry.
    pub unsafe fn finish_reference_retirement(&self, head_pfn: usize) -> UsageKind {
        let head = self.group_meta(head_pfn);
        assert_eq!(head.references.load(Ordering::Relaxed), RETIRING);
        assert_eq!(head.head_offset_pages.load(Ordering::Relaxed), 0);
        let count = 1usize << head.allocation_order();
        for index in 1..count {
            let tail = self.group_meta(head_pfn + index);
            assert_eq!(tail.references.load(Ordering::Relaxed), 0);
            // All holders have ended. Removing membership precedes inventory
            // publication; a later incarnation cannot race these stores.
            tail.head_offset_pages.store(0, Ordering::Release);
        }
        head.set_allocation_order(0);
        // SAFETY: this caller uniquely owns Retiring, and all group links/order
        // have been cleared while storage is still outside allocator inventory.
        unsafe { head.finish_retirement() }
    }

    /// Converts the caller's sole whole-allocation hold into one hold per page.
    /// Returns the old geometry so its owner can transfer every new hold once.
    ///
    /// # Errors
    /// Returns NotExclusive if another hold exists, or a resolution error. On
    /// error the caller retains its unchanged original hold. After freezing the
    /// head, this operation has no fallible step and publishes every child.
    ///
    /// # Safety
    /// The caller owns the hold represented by meta, and must end content use,
    /// mappings, device use and required retirement before splitting. On success
    /// consume the old holder without releasing it and take responsibility for
    /// exactly one new hold on every page, including unclaimed output pages.
    pub unsafe fn split_reference(&self, meta: &PageMeta) -> Result<AllocationInfo, PageMetaError> {
        // SAFETY: the caller retains its current allocation hold up to freezing.
        let allocation = unsafe { self.reference_allocation(meta) }?;
        let head = self.group_meta(allocation.head_pfn);
        // Acquire joins completed earlier holders' releases; the frozen state
        // prevents any counter operation from crossing the geometry transition.
        head.references
            .compare_exchange(1, SPLITTING, Ordering::AcqRel, Ordering::Relaxed)
            .map_err(|count| {
                if (2..=MAX_REFERENCES).contains(&count) {
                    PageMetaError::NotExclusive
                } else {
                    PageMetaError::InvalidState
                }
            })?;
        let kind = head.usage_kind.load(Ordering::Relaxed);
        for index in 1..allocation.page_count() {
            let tail = self.group_meta(allocation.head_pfn + index);
            tail.usage_kind.store(kind, Ordering::Relaxed);
            tail.references.store(1, Ordering::Relaxed);
            // Publishes each independent page's count/class. No child owner is
            // exposed until this whole operation returns to the unique caller.
            tail.head_offset_pages.store(0, Ordering::Release);
        }
        head.set_allocation_order(0);
        head.references.store(1, Ordering::Release);
        Ok(allocation)
    }

    fn validate_unclaimed_range(
        &self,
        first_pfn: usize,
        num_pages: usize,
    ) -> Result<(), PageMetaError> {
        range_geometry(first_pfn, num_pages)?;
        for index in 0..num_pages {
            let meta = self
                .lookup(first_pfn + index)
                .ok_or(PageMetaError::NotCovered)?;
            if !meta.is_unreferenced() {
                return Err(PageMetaError::InvalidState);
            }
        }
        Ok(())
    }

    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: group admission validates full coverage; \
                  live holds exclude retirement, and exclusive transitions retain all backing"
    )]
    fn group_meta(&self, pfn: usize) -> &PageMeta {
        self.lookup(pfn)
            .expect("owned allocation lost metadata coverage")
    }
}

fn geometry(head_pfn: usize, order: u8) -> Result<AllocationInfo, PageMetaError> {
    let count = 1usize
        .checked_shl(u32::from(order))
        .ok_or(PageMetaError::InvalidRange)?;
    u32::try_from(count - 1).map_err(|_| PageMetaError::InvalidRange)?;
    range_geometry(head_pfn, count)?;
    Ok(AllocationInfo { head_pfn, order })
}

fn range_geometry(first_pfn: usize, num_pages: usize) -> Result<(), PageMetaError> {
    if num_pages == 0 {
        return Err(PageMetaError::InvalidRange);
    }
    first_pfn
        .checked_add(num_pages)
        .and_then(|end| end.checked_mul(PAGE_SIZE_4K))
        .ok_or(PageMetaError::InvalidRange)?;
    Ok(())
}

unittest::path_mod_test!(tests, "../../ut-tests/group.rs");

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AArch64 user-ASID lifetime management bound to user address spaces.

use core::{
    ptr::NonNull,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

use kerrno::{KResult, k_bail};
use kspin::SpinNoIrq;
use memaddr::PhysAddr;

const USER_ASID_FIRST_VERSION: u64 = 1_u64 << karch::USER_ASID_BITS;
const USER_ASID_MASK: u64 = USER_ASID_FIRST_VERSION - 1;
const USER_ASID_MAX: u16 = USER_ASID_MASK as u16;
const USER_ASID_BITMAP_WORDS: usize = (USER_ASID_MAX as usize + 1).div_ceil(64);
const MAX_PINNED_ASIDS: usize = USER_ASID_MAX as usize - kbuild_config::NR_CPUS - 1;

type RuntimeUserAsidAllocator = UserAsidAllocator<{ kbuild_config::NR_CPUS }>;

static ACTIVE_ASID_CONTEXT_IDS: [AtomicU64; kbuild_config::NR_CPUS] =
    [const { AtomicU64::new(0) }; kbuild_config::NR_CPUS];

static USER_ASID_ALLOCATOR: SpinNoIrq<RuntimeUserAsidAllocator> =
    SpinNoIrq::new(UserAsidAllocator::new());

/// AArch64 user address-space ASID context.
#[derive(Debug)]
pub struct Aarch64UserAsidContext {
    root_paddr: PhysAddr,
    context_id: AtomicU64,
    pin_count: AtomicUsize,
}

impl Aarch64UserAsidContext {
    pub(crate) fn new(root_paddr: PhysAddr) -> Self {
        Self {
            root_paddr,
            context_id: AtomicU64::new(
                USER_ASID_ALLOCATOR
                    .lock()
                    .alloc_fresh_context_id(&ACTIVE_ASID_CONTEXT_IDS),
            ),
            pin_count: AtomicUsize::new(0),
        }
    }

    #[inline]
    fn context_id(&self) -> u64 {
        self.context_id.load(Ordering::Acquire)
    }

    #[inline]
    pub fn current_asid(&self) -> u16 {
        context_id_asid(self.context_id())
    }

    #[inline]
    pub fn hardware_root(&self) -> karch::HwPageTableRoot {
        karch::encode_user_page_table_root(self.root_paddr, self.current_asid())
    }

    pub fn prepare_switch_root(&self) -> karch::HwPageTableRoot {
        let cpu_index = khal::percpu::this_cpu_id().as_usize();
        let (context_id, needs_flush) =
            USER_ASID_ALLOCATOR
                .lock()
                .prepare_switch(self, cpu_index, &ACTIVE_ASID_CONTEXT_IDS);
        if needs_flush {
            karch::flush_tlb(None);
        }
        karch::encode_user_page_table_root(self.root_paddr, context_id_asid(context_id))
    }

    /// Pins the current ASID so generation rollover cannot recycle it while an
    /// external user still depends on this address-space context.
    pub fn pin(&self) -> KResult<u16> {
        USER_ASID_ALLOCATOR
            .lock()
            .pin_context(self, &ACTIVE_ASID_CONTEXT_IDS)
    }

    /// Releases one pin reference acquired via [`Self::pin`].
    pub fn unpin(&self) {
        USER_ASID_ALLOCATOR.lock().unpin_context(self);
    }

    /// Returns the currently pinned ASID, if any.
    pub fn pinned_asid(&self) -> Option<u16> {
        (self.pin_count.load(Ordering::Acquire) != 0).then(|| self.current_asid())
    }

    pub(crate) fn install_page_table_asid_provider(&self, pgtbl: &mut khal::paging::PageTable) {
        let ctx = NonNull::from(self).cast::<()>();
        // SAFETY:
        // - `ctx` points at the `Aarch64UserAsidContext` allocation owned by the
        //   enclosing address space;
        // - that context outlives the page table stored in the same address
        //   space object;
        // - the callback only performs an atomic read of the current ASID.
        unsafe { pgtbl.set_user_asid_provider(ctx, page_table_user_asid) };
    }
}

#[derive(Debug)]
struct UserAsidAllocator<const CPU_COUNT: usize> {
    generation: u64,
    asid_bitmap: [u64; USER_ASID_BITMAP_WORDS],
    pinned_asid_bitmap: [u64; USER_ASID_BITMAP_WORDS],
    nr_pinned_asids: usize,
    next_hint: u16,
    reserved_asids: [u64; CPU_COUNT],
    tlb_flush_pending: [bool; CPU_COUNT],
}

impl<const CPU_COUNT: usize> UserAsidAllocator<CPU_COUNT> {
    const fn new() -> Self {
        let mut asid_bitmap = [0; USER_ASID_BITMAP_WORDS];
        asid_bitmap[0] = 1;
        Self {
            generation: USER_ASID_FIRST_VERSION,
            asid_bitmap,
            pinned_asid_bitmap: [0; USER_ASID_BITMAP_WORDS],
            nr_pinned_asids: 0,
            next_hint: 1,
            reserved_asids: [0; CPU_COUNT],
            tlb_flush_pending: [false; CPU_COUNT],
        }
    }

    fn alloc_fresh_context_id(&mut self, active_context_ids: &[AtomicU64; CPU_COUNT]) -> u64 {
        self.new_context_id(0, active_context_ids)
    }

    fn pin_context(
        &mut self,
        mm_ctx: &Aarch64UserAsidContext,
        active_context_ids: &[AtomicU64; CPU_COUNT],
    ) -> KResult<u16> {
        let mut context_id = mm_ctx.context_id();
        let pin_count = mm_ctx.pin_count.load(Ordering::Acquire);

        if pin_count != 0 {
            if !self.generation_matches(context_id) {
                context_id = self.new_context_id(context_id, active_context_ids);
                mm_ctx.context_id.store(context_id, Ordering::Release);
                self.pin_asid(context_id_asid(context_id));
            }
            mm_ctx.pin_count.store(pin_count + 1, Ordering::Release);
            return Ok(context_id_asid(context_id));
        }

        if self.nr_pinned_asids >= MAX_PINNED_ASIDS {
            k_bail!(ResourceBusy, "AArch64 pinned ASID space exhausted");
        }

        if !self.generation_matches(context_id) {
            context_id = self.new_context_id(context_id, active_context_ids);
            mm_ctx.context_id.store(context_id, Ordering::Release);
        }

        self.pin_asid(context_id_asid(context_id));
        mm_ctx.pin_count.store(1, Ordering::Release);
        Ok(context_id_asid(context_id))
    }

    fn unpin_context(&mut self, mm_ctx: &Aarch64UserAsidContext) {
        let pin_count = mm_ctx.pin_count.load(Ordering::Acquire);
        if pin_count <= 1 {
            mm_ctx.pin_count.store(0, Ordering::Release);
            self.unpin_asid(context_id_asid(mm_ctx.context_id()));
            return;
        }
        mm_ctx.pin_count.store(pin_count - 1, Ordering::Release);
    }

    fn prepare_switch(
        &mut self,
        mm_ctx: &Aarch64UserAsidContext,
        cpu_index: usize,
        active_context_ids: &[AtomicU64; CPU_COUNT],
    ) -> (u64, bool) {
        let mut context_id = mm_ctx.context_id();
        if !self.generation_matches(context_id) {
            context_id = self.new_context_id(context_id, active_context_ids);
            mm_ctx.context_id.store(context_id, Ordering::Release);
        }

        let needs_flush = self.tlb_flush_pending[cpu_index];
        self.tlb_flush_pending[cpu_index] = false;
        self.reserved_asids[cpu_index] = context_id;
        active_context_ids[cpu_index].store(context_id, Ordering::Relaxed);
        (context_id, needs_flush)
    }

    fn new_context_id(
        &mut self,
        old_context_id: u64,
        active_context_ids: &[AtomicU64; CPU_COUNT],
    ) -> u64 {
        if old_context_id != 0 {
            let new_context_id =
                compose_context_id(context_id_asid(old_context_id), self.generation);
            if self.refresh_reserved_asids(old_context_id, new_context_id) {
                return new_context_id;
            }
            if self.is_asid_pinned(context_id_asid(old_context_id)) {
                return new_context_id;
            }
            let old_asid = context_id_asid(old_context_id);
            if !self.is_asid_reserved(old_asid) {
                self.reserve_asid(old_asid);
                return new_context_id;
            }
        }

        let mut asid = self.find_free_asid(self.next_hint);
        if asid.is_none() {
            self.generation = self.generation.wrapping_add(USER_ASID_FIRST_VERSION);
            self.rollover_generation(active_context_ids);
            asid = self.find_free_asid(1);
        }
        let asid = asid.expect("AArch64 ASID allocator exhausted after generation rollover");
        self.reserve_asid(asid);
        self.next_hint = if asid == USER_ASID_MAX { 1 } else { asid + 1 };
        compose_context_id(asid, self.generation)
    }

    fn rollover_generation(&mut self, active_context_ids: &[AtomicU64; CPU_COUNT]) {
        self.asid_bitmap.copy_from_slice(&self.pinned_asid_bitmap);
        self.reserve_asid(0);
        for (cpu_index, active_context_id) in active_context_ids.iter().enumerate() {
            let active = active_context_id.swap(0, Ordering::Relaxed);
            let reserved = if active != 0 {
                active
            } else {
                self.reserved_asids[cpu_index]
            };
            self.reserved_asids[cpu_index] = reserved;
            if reserved != 0 {
                self.reserve_asid(context_id_asid(reserved));
            }
            self.tlb_flush_pending[cpu_index] = true;
        }
        self.next_hint = 1;
    }

    fn generation_matches(&self, context_id: u64) -> bool {
        context_id != 0 && (context_id & !USER_ASID_MASK) == self.generation
    }

    fn refresh_reserved_asids(&mut self, old_context_id: u64, new_context_id: u64) -> bool {
        let mut hit = false;
        for reserved in &mut self.reserved_asids {
            if *reserved == old_context_id {
                *reserved = new_context_id;
                hit = true;
            }
        }
        hit
    }

    fn find_free_asid(&self, start: u16) -> Option<u16> {
        for asid in start..=USER_ASID_MAX {
            if !self.is_asid_reserved(asid) {
                return Some(asid);
            }
        }
        if start > 1 {
            for asid in 1..start {
                if !self.is_asid_reserved(asid) {
                    return Some(asid);
                }
            }
        }
        None
    }

    fn is_asid_reserved(&self, asid: u16) -> bool {
        let bit = asid as usize;
        let word = bit / 64;
        let mask = 1_u64 << (bit % 64);
        (self.asid_bitmap[word] & mask) != 0
    }

    fn reserve_asid(&mut self, asid: u16) {
        let bit = asid as usize;
        let word = bit / 64;
        let mask = 1_u64 << (bit % 64);
        self.asid_bitmap[word] |= mask;
    }

    fn pin_asid(&mut self, asid: u16) {
        if !self.is_asid_pinned(asid) {
            self.nr_pinned_asids += 1;
        }
        let bit = asid as usize;
        let word = bit / 64;
        let mask = 1_u64 << (bit % 64);
        self.pinned_asid_bitmap[word] |= mask;
        self.asid_bitmap[word] |= mask;
    }

    fn unpin_asid(&mut self, asid: u16) {
        if !self.is_asid_pinned(asid) {
            return;
        }
        let bit = asid as usize;
        let word = bit / 64;
        let mask = 1_u64 << (bit % 64);
        self.pinned_asid_bitmap[word] &= !mask;
        self.nr_pinned_asids -= 1;
    }

    fn is_asid_pinned(&self, asid: u16) -> bool {
        let bit = asid as usize;
        let word = bit / 64;
        let mask = 1_u64 << (bit % 64);
        (self.pinned_asid_bitmap[word] & mask) != 0
    }
}

#[inline]
const fn context_id_asid(context_id: u64) -> u16 {
    (context_id & USER_ASID_MASK) as u16
}

#[inline]
const fn compose_context_id(asid: u16, generation: u64) -> u64 {
    generation | asid as u64
}

unsafe fn page_table_user_asid(ctx: NonNull<()>) -> u16 {
    // SAFETY:
    // - callers pass the `Aarch64UserAsidContext` pointer originally registered
    //   by `install_page_table_asid_provider`;
    // - the context allocation remains live for the duration of page-table use.
    let ctx = unsafe { ctx.cast::<Aarch64UserAsidContext>().as_ref() };
    ctx.current_asid()
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;

    type TestAllocator = UserAsidAllocator<2>;

    fn test_active_context_ids() -> [AtomicU64; 2] {
        [const { AtomicU64::new(0) }; 2]
    }

    #[def_test]
    fn test_new_context_reuses_reserved_asid_across_generation() {
        let mut allocator = TestAllocator::new();
        let active_context_ids = test_active_context_ids();
        let old_context_id = compose_context_id(5, USER_ASID_FIRST_VERSION);

        allocator.generation = USER_ASID_FIRST_VERSION.wrapping_add(USER_ASID_FIRST_VERSION);
        allocator.reserved_asids[1] = old_context_id;

        let new_context_id = allocator.new_context_id(old_context_id, &active_context_ids);

        assert_eq!(
            new_context_id,
            compose_context_id(5, allocator.generation),
            "reserved ASIDs should be carried into the new generation"
        );
        assert_eq!(allocator.reserved_asids[1], new_context_id);
    }

    #[def_test]
    fn test_rollover_preserves_active_asids_and_marks_pending_flush() {
        let mut allocator = TestAllocator::new();
        let active_context_ids = test_active_context_ids();
        let active_context_id = compose_context_id(9, allocator.generation);

        active_context_ids[0].store(active_context_id, Ordering::Relaxed);
        allocator.rollover_generation(&active_context_ids);

        assert_eq!(allocator.reserved_asids[0], active_context_id);
        assert!(allocator.is_asid_reserved(9));
        assert!(allocator.tlb_flush_pending[0]);
        assert!(allocator.tlb_flush_pending[1]);
        assert_eq!(active_context_ids[0].load(Ordering::Relaxed), 0);
    }

    #[def_test]
    fn test_prepare_switch_updates_stale_context_and_consumes_flush() {
        let mut allocator = TestAllocator::new();
        let active_context_ids = test_active_context_ids();
        let stale_context_id = compose_context_id(7, USER_ASID_FIRST_VERSION);
        let mm_ctx = Aarch64UserAsidContext {
            root_paddr: PhysAddr::from(0usize),
            context_id: AtomicU64::new(stale_context_id),
            pin_count: AtomicUsize::new(0),
        };

        allocator.generation = USER_ASID_FIRST_VERSION.wrapping_add(USER_ASID_FIRST_VERSION);
        allocator.tlb_flush_pending[0] = true;

        let (context_id, needs_flush) = allocator.prepare_switch(&mm_ctx, 0, &active_context_ids);

        assert!(needs_flush);
        assert_eq!(context_id, compose_context_id(7, allocator.generation));
        assert_eq!(mm_ctx.context_id(), context_id);
        assert_eq!(allocator.reserved_asids[0], context_id);
        assert_eq!(active_context_ids[0].load(Ordering::Relaxed), context_id);
        assert!(!allocator.tlb_flush_pending[0]);
    }

    #[def_test]
    fn test_pin_context_reuses_same_asid_after_rollover() {
        let mut allocator = TestAllocator::new();
        let active_context_ids = test_active_context_ids();
        let stale_context_id = compose_context_id(11, USER_ASID_FIRST_VERSION);
        let mm_ctx = Aarch64UserAsidContext {
            root_paddr: PhysAddr::from(0usize),
            context_id: AtomicU64::new(stale_context_id),
            pin_count: AtomicUsize::new(1),
        };

        allocator.generation = USER_ASID_FIRST_VERSION.wrapping_add(USER_ASID_FIRST_VERSION);
        allocator.pin_asid(11);

        let asid = allocator
            .pin_context(&mm_ctx, &active_context_ids)
            .expect("repinning a stale pinned context should succeed");

        assert_eq!(asid, 11);
        assert_eq!(
            mm_ctx.context_id(),
            compose_context_id(11, allocator.generation)
        );
        assert_eq!(mm_ctx.pin_count.load(Ordering::Acquire), 2);
        assert_eq!(allocator.nr_pinned_asids, 1);
    }

    #[def_test]
    fn test_rollover_keeps_pinned_asids_reserved() {
        let mut allocator = TestAllocator::new();
        let active_context_ids = test_active_context_ids();

        allocator.pin_asid(13);
        allocator.rollover_generation(&active_context_ids);

        assert!(allocator.is_asid_reserved(13));
        assert!(allocator.is_asid_pinned(13));
    }

    #[def_test]
    fn test_unpin_context_releases_last_pin() {
        let mut allocator = TestAllocator::new();
        let mm_ctx = Aarch64UserAsidContext {
            root_paddr: PhysAddr::from(0usize),
            context_id: AtomicU64::new(compose_context_id(17, USER_ASID_FIRST_VERSION)),
            pin_count: AtomicUsize::new(1),
        };

        allocator.pin_asid(17);
        allocator.unpin_context(&mm_ctx);

        assert_eq!(mm_ctx.pin_count.load(Ordering::Acquire), 0);
        assert_eq!(allocator.nr_pinned_asids, 0);
        assert!(!allocator.is_asid_pinned(17));
    }
}

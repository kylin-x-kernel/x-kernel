// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Randomized invariant stress for [`BuddyAllocator`].
//!
//! The in-crate `#[def_test]` cases cover targeted transitions. These host
//! tests add long randomized operation sequences and assert three invariants
//! that no single operation can establish on its own:
//!
//! - no two live allocations overlap;
//! - capacity is conserved, so every drain returns `free_pages()` to its
//!   initial value;
//! - draining fully coalesces, so the largest aligned block that fits the
//!   usable window can be allocated in a single request afterwards.
//!
//! They run on the host, where the `debug_assert!` invariants inside
//! `buddy_alloc` are live; kernel images build release, which compiles those
//! assertions out. Sequences use fixed xorshift seeds, so a failure is
//! reproducible without a debugger.
//!
//! Coverage spans both free-head bitmap modes (inline for regions of at most
//! `INLINE_BITMAP_WORDS * WORD_BITS` pages, reserved prefix above that),
//! non-power-of-two region sizes, fixed-address allocation through
//! `allocate_pages_at`, several regions in one allocator, and repeated
//! `init_region`/`reset` re-initialization.

use std::{
    alloc::{Layout, alloc_zeroed, dealloc},
    collections::BTreeMap,
};

use alloc_engine::BuddyAllocator;

const PAGE: usize = 4096;

/// Regions of at most this many pages keep their bitmap inline.
const INLINE_BITMAP_PAGES: usize = 4 * usize::BITS as usize;

/// Page-aligned, exclusively owned backing storage for one allocator region.
struct BackingStorage {
    ptr: *mut u8,
    layout: Layout,
}

impl BackingStorage {
    fn new(pages: usize) -> Self {
        let layout =
            Layout::from_size_align(pages * PAGE, pages.next_power_of_two() * PAGE).unwrap();
        // SAFETY: The layout has nonzero size and a power-of-two alignment.
        // The allocation stays owned by this fixture until `Drop`.
        let ptr = unsafe { alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "host backing allocation failed");
        Self { ptr, layout }
    }

    fn base(&self) -> usize {
        self.ptr as usize
    }

    fn size(&self) -> usize {
        self.layout.size()
    }
}

impl Drop for BackingStorage {
    fn drop(&mut self) {
        // SAFETY: The nested allocator and every block taken from it are
        // retired before this unique allocation is released with the layout it
        // was created from.
        unsafe { dealloc(self.ptr, self.layout) };
    }
}

/// Deterministic xorshift source so failures replay exactly.
struct XorShift(u64);

impl XorShift {
    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next_u64() % bound as u64) as usize
    }
}

/// Live allocations keyed by start address, checked against every new block.
struct LiveBlocks {
    blocks: BTreeMap<usize, usize>,
}

impl LiveBlocks {
    fn new() -> Self {
        Self {
            blocks: BTreeMap::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    fn len(&self) -> usize {
        self.blocks.len()
    }

    fn nth_addr(&self, index: usize) -> usize {
        *self
            .blocks
            .keys()
            .nth(index)
            .expect("index within live blocks")
    }

    /// Records a new allocation, rejecting overlap and address reuse.
    fn insert(&mut self, addr: usize, pages: usize, context: &str) {
        let end = addr + pages * PAGE;
        for (&live_addr, &live_pages) in &self.blocks {
            let live_end = live_addr + live_pages * PAGE;
            assert!(
                end <= live_addr || addr >= live_end,
                "{context}: overlapping allocation [{addr:#x},{end:#x}) vs \
                 [{live_addr:#x},{live_end:#x})"
            );
        }
        assert!(
            self.blocks.insert(addr, pages).is_none(),
            "{context}: address {addr:#x} handed out twice"
        );
    }

    fn remove(&mut self, addr: usize) -> usize {
        self.blocks
            .remove(&addr)
            .expect("freeing an untracked address")
    }

    /// Releases every remaining block and verifies the allocator is full again.
    fn drain(&mut self, allocator: &mut BuddyAllocator<PAGE>, expected_free: usize, ctx: &str) {
        for (&addr, &pages) in self.blocks.iter() {
            allocator.deallocate_pages(addr, pages);
        }
        self.blocks.clear();
        assert_eq!(
            allocator.free_pages(),
            expected_free,
            "{ctx}: capacity not conserved after drain"
        );
    }
}

/// Pages reserved for the free-head bitmap of a region of `region_pages`.
///
/// Mirrors the reservation rule in `add_region`; the caller asserts the
/// resulting usable page count so a rule change fails loudly here.
fn reserved_bitmap_pages(region_pages: usize) -> usize {
    if region_pages <= INLINE_BITMAP_PAGES {
        0
    } else {
        region_pages.div_ceil(8).div_ceil(PAGE)
    }
}

/// Largest power-of-two block that has an aligned slot inside the window.
fn largest_aligned_block(heap_base: usize, heap_end: usize, usable_pages: usize) -> usize {
    let mut pages = 1usize << (usize::BITS as usize - 1 - usable_pages.leading_zeros() as usize);
    loop {
        let span = pages * PAGE;
        let start = (heap_base + span - 1) & !(span - 1);
        if start + span <= heap_end {
            return pages;
        }
        assert!(pages > 1, "no aligned block fits the usable window");
        pages /= 2;
    }
}

/// Runs `rounds` of randomized allocate / allocate-at / free sequences.
fn stress(region_pages: usize, rounds: usize, seed: u64, label: &str) {
    let storage = BackingStorage::new(region_pages);
    let mut allocator = BuddyAllocator::<PAGE>::new();
    allocator
        .init_region(storage.base(), storage.size())
        .unwrap();

    let usable_pages = allocator.free_pages();
    let reserved = reserved_bitmap_pages(region_pages);
    assert_eq!(
        usable_pages,
        region_pages - reserved,
        "{label}: usable page count does not match the documented reservation"
    );
    let heap_base = storage.base() + reserved * PAGE;
    let heap_end = heap_base + usable_pages * PAGE;
    let biggest = largest_aligned_block(heap_base, heap_end, usable_pages);

    let mut rng = XorShift(seed);
    for round in 0..rounds {
        let mut live = LiveBlocks::new();
        for step in 0..400 {
            let context = format!("{label} round {round} step {step}");
            match rng.below(100) {
                // Ordinary allocation of a random order.
                pick if pick < 45 || live.is_empty() => {
                    let pages = 1usize << rng.below(7);
                    if allocator.free_pages() < pages {
                        continue;
                    }
                    if let Ok(addr) = allocator.allocate_pages(pages, PAGE) {
                        assert!(
                            addr >= heap_base && addr + pages * PAGE <= heap_end,
                            "{context}: {addr:#x} + {pages} pages outside \
                             [{heap_base:#x},{heap_end:#x})"
                        );
                        // SAFETY: The block is exclusively owned by this test
                        // and at least one page long; the payload proves that
                        // later header reads cannot come from stale content.
                        unsafe { std::ptr::write_bytes(addr as *mut u8, 0xA5, PAGE) };
                        live.insert(addr, pages, &context);
                    }
                }
                // Fixed-address allocation at a random aligned target.
                pick if pick < 65 => {
                    let pages = 1usize << rng.below(5);
                    let span = pages * PAGE;
                    let first = (heap_base + span - 1) & !(span - 1);
                    if first + span > heap_end {
                        continue;
                    }
                    let slots = (heap_end - first) / span;
                    let target = first + rng.below(slots) * span;
                    if let Ok(addr) = allocator.allocate_pages_at(target, pages, PAGE) {
                        assert_eq!(addr, target, "{context}: allocate_pages_at moved the block");
                        live.insert(addr, pages, &context);
                    }
                }
                // Release a random live block.
                _ => {
                    let addr = live.nth_addr(rng.below(live.len()));
                    let pages = live.remove(addr);
                    allocator.deallocate_pages(addr, pages);
                }
            }
        }

        live.drain(
            &mut allocator,
            usable_pages,
            &format!("{label} round {round}"),
        );
        // A fully coalesced region must still satisfy the largest aligned
        // request; fragmentation or a lost block shows up here.
        let addr = allocator.allocate_pages(biggest, PAGE).unwrap_or_else(|e| {
            panic!(
                "{label} round {round}: not fully coalesced, free={} but {biggest} pages failed: \
                 {e:?}",
                allocator.free_pages()
            )
        });
        allocator.deallocate_pages(addr, biggest);
        assert_eq!(
            allocator.free_pages(),
            usable_pages,
            "{label} round {round}: capacity changed across the coalescing probe"
        );
    }
}

#[test]
fn inline_bitmap_regions_survive_randomized_stress() {
    stress(INLINE_BITMAP_PAGES, 12, 0x1234_5678, "inline-256");
    stress(64, 8, 0x0F1E_2D3C, "inline-64");
}

#[test]
fn reserved_bitmap_regions_survive_randomized_stress() {
    stress(4096, 8, 0x9E37_79B9, "reserved-4096");
    stress(1024, 8, 0xDEAD_BEEF, "reserved-1024");
}

#[test]
fn non_power_of_two_regions_survive_randomized_stress() {
    stress(1000, 6, 0xCAFE_F00D, "reserved-1000");
    stress(4095, 4, 0x0BAD_C0DE, "reserved-4095");
    stress(257, 6, 0x51ED_2701, "reserved-257");
}

/// Every region size down to a single page must stay fully allocatable.
#[test]
fn small_regions_stay_fully_allocatable() {
    for pages in [1usize, 2, 3, 7, 8, 17, 64, 255, 256, 257] {
        let storage = BackingStorage::new(pages);
        let mut allocator = BuddyAllocator::<PAGE>::new();
        allocator
            .init_region(storage.base(), storage.size())
            .unwrap();
        let usable_pages = allocator.free_pages();
        assert_eq!(
            usable_pages,
            pages - reserved_bitmap_pages(pages),
            "{pages}-page region reports {usable_pages} usable pages"
        );

        let mut addrs = Vec::new();
        while let Ok(addr) = allocator.allocate_pages(1, PAGE) {
            addrs.push(addr);
        }
        assert_eq!(
            addrs.len(),
            usable_pages,
            "{pages}-page region handed out {} of {usable_pages} pages",
            addrs.len()
        );
        for addr in addrs {
            allocator.deallocate_pages(addr, 1);
        }
        assert_eq!(
            allocator.free_pages(),
            usable_pages,
            "{pages}-page region lost capacity across a full sweep"
        );
    }
}

/// Several regions in one allocator, plus repeated re-initialization.
#[test]
fn multiple_regions_and_reinit_conserve_capacity() {
    let first = BackingStorage::new(512);
    let second = BackingStorage::new(1024);
    let third = BackingStorage::new(64);
    let mut allocator = BuddyAllocator::<PAGE>::new();
    for storage in [&first, &second, &third] {
        allocator
            .add_region(storage.base(), storage.size())
            .unwrap();
    }
    let total = allocator.free_pages();
    assert_eq!(
        total,
        [512usize, 1024, 64]
            .iter()
            .map(|&pages| pages - reserved_bitmap_pages(pages))
            .sum(),
        "three regions do not add up to the expected usable capacity"
    );

    let mut rng = XorShift(0x2545_F491_4F6C_DD1D);
    let mut live = LiveBlocks::new();
    for step in 0..600 {
        let context = format!("multi-region step {step}");
        match rng.below(3) {
            0 => {
                let pages = 1usize << rng.below(6);
                if let Ok(addr) = allocator.allocate_pages(pages, PAGE) {
                    live.insert(addr, pages, &context);
                }
            }
            1 => {
                let pages = 1usize << rng.below(4);
                let span = pages * PAGE;
                for storage in [&first, &second, &third] {
                    let begin = storage.base();
                    let end = begin + storage.size();
                    let first_fit = (begin + span - 1) & !(span - 1);
                    if first_fit + span > end {
                        continue;
                    }
                    let slots = (end - first_fit) / span;
                    let target = first_fit + rng.below(slots) * span;
                    if let Ok(addr) = allocator.allocate_pages_at(target, pages, PAGE) {
                        assert_eq!(addr, target, "{context}: allocate_pages_at moved the block");
                        live.insert(addr, pages, &context);
                        break;
                    }
                }
            }
            _ => {
                if live.is_empty() {
                    continue;
                }
                let addr = live.nth_addr(rng.below(live.len()));
                let pages = live.remove(addr);
                allocator.deallocate_pages(addr, pages);
            }
        }
    }
    live.drain(&mut allocator, total, "multi-region");

    // `init_region` resets and rebuilds the region set; the allocator must stay
    // usable across repeated re-initialization.
    for round in 0..5 {
        allocator.init_region(second.base(), second.size()).unwrap();
        let usable_pages = allocator.free_pages();
        let mut addrs = Vec::new();
        while let Ok(addr) = allocator.allocate_pages(1, PAGE) {
            addrs.push(addr);
        }
        assert_eq!(
            addrs.len(),
            usable_pages,
            "reinit round {round}: handed out {} of {usable_pages} pages",
            addrs.len()
        );
        for addr in addrs {
            allocator.deallocate_pages(addr, 1);
        }
        assert_eq!(
            allocator.free_pages(),
            usable_pages,
            "reinit round {round}: capacity not conserved"
        );
    }
}

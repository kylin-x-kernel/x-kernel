// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

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

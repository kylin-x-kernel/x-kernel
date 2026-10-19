// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[cfg(all(feature = "smp", not(target_arch = "aarch64")))]
use core::ptr::NonNull;
#[cfg(all(feature = "smp", not(target_arch = "aarch64")))]
use core::sync::atomic::AtomicBool;
use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicUsize, Ordering},
};

#[cfg(all(feature = "smp", not(target_arch = "aarch64")))]
use kcpu_id_map::KCpuMask;
use memaddr::{PAGE_SIZE_4K, PhysAddr, VirtAddr};
use unittest::def_test;

use crate::{
    PageSize, PageTable64, PageTableEntry, PagingFlags, PagingHandler, PagingMetaData,
    PteReplaceError,
};

#[derive(Clone, Copy)]
#[repr(transparent)]
struct TestEntry(usize);

impl PageTableEntry for TestEntry {
    const EMPTY: Self = Self(0);

    fn new_page(paddr: PhysAddr, flags: PagingFlags, is_huge: bool) -> Self {
        let mut bits = paddr.as_usize() | (flags.bits() << Self::FLAGS_SHIFT) | Self::PRESENT;
        if is_huge {
            bits |= Self::HUGE;
        }
        Self(bits)
    }

    fn new_table(paddr: PhysAddr) -> Self {
        Self(paddr.as_usize() | Self::PRESENT | Self::TABLE)
    }

    fn paddr(&self) -> PhysAddr {
        PhysAddr::from(self.0 & Self::PADDR_MASK)
    }

    fn flags(&self) -> PagingFlags {
        PagingFlags::from_bits_truncate((self.0 & Self::FLAGS_MASK) >> Self::FLAGS_SHIFT)
    }

    fn set_paddr(&mut self, paddr: PhysAddr) {
        self.0 = (self.0 & !Self::PADDR_MASK) | paddr.as_usize();
    }

    fn set_flags(&mut self, flags: PagingFlags, is_huge: bool) {
        self.0 = (self.0 & !(Self::FLAGS_MASK | Self::HUGE))
            | (flags.bits() << Self::FLAGS_SHIFT)
            | Self::PRESENT;
        if is_huge {
            self.0 |= Self::HUGE;
        }
    }

    fn bits(self) -> usize {
        self.0
    }

    fn is_present(&self) -> bool {
        self.0 & Self::PRESENT != 0
    }

    fn is_huge(&self) -> bool {
        self.0 & Self::HUGE != 0
    }
}

impl TestEntry {
    const FLAGS_MASK: usize = 0x7f << 3;
    const FLAGS_SHIFT: usize = 3;
    const HUGE: usize = 1 << 2;
    const PADDR_MASK: usize = !0xfff;
    const PRESENT: usize = 1 << 0;
    const TABLE: usize = 1 << 1;
}

impl core::fmt::Debug for TestEntry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TestEntry")
            .field("paddr", &self.paddr())
            .field("flags", &self.flags())
            .finish()
    }
}

struct TestMeta;

impl PagingMetaData for TestMeta {
    type VirtAddr = VirtAddr;

    const LEVELS: usize = 4;
    const PA_MAX_BITS: usize = usize::BITS as usize - 1;
    const VA_MAX_BITS: usize = usize::BITS as usize - 1;

    fn vaddr_is_valid(_vaddr: usize) -> bool {
        true
    }

    fn paddr_is_valid(_paddr: usize) -> bool {
        true
    }

    fn flush_tlb(_vaddr: Option<Self::VirtAddr>) {}

    #[cfg(all(feature = "smp", not(target_arch = "aarch64")))]
    fn flush_tlb_process_mask(_vaddr: Option<Self::VirtAddr>, _target_mask: KCpuMask) {}
}

/// Per-instance provider context owned by a single test case.
///
/// Moving the flush bookkeeping off module-level statics makes parallel
/// test cases independent: each test instantiates its own `ProviderCtx`,
/// so concurrent `PageTableMut::drop` → `flush_tlb_process_mask` paths in
/// sibling tests can no longer pollute each other's counters. The
/// previous global-static design raced under the parallel scheduler,
/// inflating `flush_calls` with cross-test flushes.
#[cfg(all(feature = "smp", not(target_arch = "aarch64")))]
struct ProviderCtx {
    /// Input residency mask that `test_user_cpu_mask_provider` snapshots.
    provider_mask_bits: [AtomicBool; kbuild_config::NR_CPUS],
    /// Number of times `flush_tlb_process_mask` was invoked.
    flush_calls: AtomicUsize,
    /// Last mask observed by `flush_tlb_process_mask`.
    observed_mask_bits: [AtomicBool; kbuild_config::NR_CPUS],
}

#[cfg(all(feature = "smp", not(target_arch = "aarch64")))]
impl ProviderCtx {
    const fn new() -> Self {
        Self {
            provider_mask_bits: [const { AtomicBool::new(false) }; kbuild_config::NR_CPUS],
            flush_calls: AtomicUsize::new(0),
            observed_mask_bits: [const { AtomicBool::new(false) }; kbuild_config::NR_CPUS],
        }
    }
}

/// Provider callback that both returns the residency mask and records the
/// observation into `ctx`. Counting here (rather than in
/// `TestMeta::flush_tlb_process_mask`, which has no access to the test
/// instance) makes each parallel test case self-contained: the counter
/// lives on the test-owned `ProviderCtx`, not a shared module static.
///
/// # Safety
///
/// `ctx` must point to the initialized, aligned `ProviderCtx` registered
/// by the test. The test must keep it alive for every provider invocation;
/// concurrent reads and updates must use its atomic fields.
#[cfg(all(feature = "smp", not(target_arch = "aarch64")))]
unsafe fn test_user_cpu_mask_provider(ctx: NonNull<()>) -> KCpuMask {
    // SAFETY: the caller (PageTable64::set_user_cpu_mask_provider) upholds
    // the provider contract; `ctx` points to a live `ProviderCtx`.
    let ctx = unsafe { ctx.cast::<ProviderCtx>().as_ref() };
    let mut mask = KCpuMask::new();
    for (cpu, provider_bit) in ctx.provider_mask_bits.iter().enumerate() {
        mask.set(cpu, provider_bit.load(Ordering::Relaxed));
    }
    // Record this observation. `finish()` calls `current_user_cpu_mask()`
    // (this provider) and `flush_tlb_process_mask` in lockstep, so a
    // provider invocation corresponds one-to-one with a process-mask
    // TLB flush.
    ctx.flush_calls.fetch_add(1, Ordering::Relaxed);
    for (cpu, observed_bit) in ctx.observed_mask_bits.iter().enumerate() {
        observed_bit.store(mask.get(cpu), Ordering::Relaxed);
    }
    mask
}

#[repr(align(4096))]
#[derive(Clone, Copy)]
struct TestFrame {
    _bytes: [u8; PAGE_SIZE_4K],
}

const TEST_FRAME_COUNT: usize = 128;
struct FramePool(UnsafeCell<[TestFrame; TEST_FRAME_COUNT]>);

// SAFETY: The unit-test frame allocator publishes frames by monotonically
// advancing `NEXT_FRAME`; each successful allocation receives a distinct
// slot and `dealloc_frame` never returns slots to the pool. Concurrent test
// allocations can therefore race only on `NEXT_FRAME`, not on the same
// `UnsafeCell` element.
unsafe impl Sync for FramePool {}

static NEXT_FRAME: AtomicUsize = AtomicUsize::new(0);
static FRAME_POOL: FramePool = FramePool(UnsafeCell::new(
    [TestFrame {
        _bytes: [0; PAGE_SIZE_4K],
    }; TEST_FRAME_COUNT],
));

struct TestHandler;

impl PagingHandler for TestHandler {
    fn alloc_frame() -> Option<PhysAddr> {
        let index = NEXT_FRAME.fetch_add(1, Ordering::Relaxed);
        if index >= TEST_FRAME_COUNT {
            return None;
        }
        // SAFETY: Unit tests allocate each frame at most once by advancing
        // `NEXT_FRAME`. `FramePool` is 4K-aligned and each slot is exactly
        // one page, so the returned address satisfies `PagingHandler`.
        let ptr = unsafe { FRAME_POOL.0.get().cast::<TestFrame>().add(index) };
        Some(PhysAddr::from(ptr as usize))
    }

    fn dealloc_frame(_paddr: PhysAddr) {}

    fn p2v(paddr: PhysAddr) -> VirtAddr {
        VirtAddr::from(paddr.as_usize())
    }
}

// Encode test-owned frames as low physical offsets, because real
// LoongArch PTEs mask off the guest kernel's high virtual-address bits.
#[cfg(any(target_arch = "aarch64", target_arch = "loongarch64"))]
struct OffsetTestHandler;
#[cfg(any(target_arch = "aarch64", target_arch = "loongarch64"))]
impl PagingHandler for OffsetTestHandler {
    fn alloc_frame() -> Option<PhysAddr> {
        TestHandler::alloc_frame()
            .map(|frame| paddr(frame.as_usize() - FRAME_POOL.0.get() as usize + PAGE_SIZE_4K))
    }

    fn dealloc_frame(frame: PhysAddr) {
        assert!(
            (PAGE_SIZE_4K..=(TEST_FRAME_COUNT * PAGE_SIZE_4K)).contains(&frame.as_usize()),
            "must not deallocate a mapped data frame"
        );
    }

    fn p2v(frame: PhysAddr) -> VirtAddr {
        assert!(
            (PAGE_SIZE_4K..=(TEST_FRAME_COUNT * PAGE_SIZE_4K)).contains(&frame.as_usize()),
            "must not descend into a mapped data frame"
        );
        vaddr(FRAME_POOL.0.get() as usize + frame.as_usize() - PAGE_SIZE_4K)
    }
}

type TestPageTable = PageTable64<TestMeta, TestEntry, TestHandler>;

fn paddr(value: usize) -> PhysAddr {
    PhysAddr::from(value)
}

fn vaddr(value: usize) -> VirtAddr {
    VirtAddr::from(value)
}

#[def_test]
fn query_entry_returns_leaf_snapshot() {
    let mut table = TestPageTable::try_new().expect("test page table");
    let page = paddr(0x20_0000);
    table
        .modify()
        .map(
            vaddr(0x4000),
            page,
            PageSize::Size4K,
            PagingFlags::READ | PagingFlags::WRITE,
        )
        .expect("map test page");

    let snapshot = table.query_entry(vaddr(0x4123)).expect("query entry");

    assert_eq!(snapshot.paddr(), page);
    assert_eq!(snapshot.page_size(), PageSize::Size4K);
    assert_eq!(snapshot.flags(), PagingFlags::READ | PagingFlags::WRITE);
}

#[def_test]
fn replace_if_same_commits_only_matching_snapshot() {
    let mut table = TestPageTable::try_new().expect("test page table");
    let old_page = paddr(0x30_0000);
    let new_page = paddr(0x31_0000);
    table
        .modify()
        .map(vaddr(0x8000), old_page, PageSize::Size4K, PagingFlags::READ)
        .expect("map test page");
    let expected = table.query_entry(vaddr(0x8000)).expect("query entry");

    let old_snapshot = table
        .modify()
        .replace_if_same(
            vaddr(0x8000),
            expected,
            new_page,
            PagingFlags::READ | PagingFlags::WRITE,
        )
        .expect("replace matching entry");

    assert_eq!(old_snapshot.paddr(), old_page);
    assert_eq!(
        table.query(vaddr(0x8000)).expect("query replaced mapping"),
        (
            new_page,
            PagingFlags::READ | PagingFlags::WRITE,
            PageSize::Size4K,
        )
    );
}

#[def_test]
fn replace_if_same_reports_changed_without_overwrite() {
    let mut table = TestPageTable::try_new().expect("test page table");
    let original_page = paddr(0x40_0000);
    let competing_page = paddr(0x41_0000);
    let prepared_page = paddr(0x42_0000);
    table
        .modify()
        .map(
            vaddr(0xc000),
            original_page,
            PageSize::Size4K,
            PagingFlags::READ,
        )
        .expect("map test page");
    let stale = table.query_entry(vaddr(0xc000)).expect("query entry");
    table
        .modify()
        .remap(vaddr(0xc000), competing_page, PagingFlags::READ)
        .expect("competing remap");

    let result = table.modify().replace_if_same(
        vaddr(0xc000),
        stale,
        prepared_page,
        PagingFlags::READ | PagingFlags::WRITE,
    );

    assert!(matches!(
        result,
        Err(PteReplaceError::Changed { current: Some(current) })
            if current.paddr() == competing_page
    ));
    assert_eq!(
        table.query(vaddr(0xc000)).expect("query unchanged mapping"),
        (competing_page, PagingFlags::READ, PageSize::Size4K)
    );
}

#[def_test]
fn finish_reports_explicit_flush_boundary() {
    let mut table = TestPageTable::try_new().expect("test page table");
    let mut modify = table.modify();

    assert!(!modify.finish().had_pending());
    modify
        .map(
            vaddr(0x10_0000),
            paddr(0x50_0000),
            PageSize::Size4K,
            PagingFlags::READ,
        )
        .expect("map test page");
    assert!(modify.finish().had_pending());
    assert!(!modify.finish().had_pending());
}

/// Checks the policy flag, not the presence or effect of either DSB.
///
/// These tables are not installed in hardware. A recording hook can check
/// calls but cannot prove ordering against real walkers. The original
/// AArch64 KVM exec control and daily/Open POSIX runs cover that regression;
/// see "AArch64 Fresh-Map Recovery Evidence" in docs/design.md for logs and
/// scope. Barrier presence and placement still require architecture review.
#[cfg(target_arch = "aarch64")]
#[def_test]
fn aarch64_fresh_map_uses_publication_fence() {
    assert!(!crate::arch::aarch64::A64PagingMetaData::INVALID_TO_VALID_NEEDS_TLBI);
}

#[cfg(target_arch = "aarch64")]
#[def_test]
fn sparse_unmap_and_drop_skip_nonpresent_arm_huge_leaves() {
    use crate::arch::aarch64::A64PageEntry;

    for size in [PageSize::Size2M, PageSize::Size1G] {
        let mut table =
            PageTable64::<TestMeta, A64PageEntry, OffsetTestHandler>::try_new().unwrap();
        {
            let mut modify = table.modify();
            modify
                .map(vaddr(0), paddr(0x4000_0000), size, PagingFlags::empty())
                .unwrap();
            modify.finish();
            let before = modify
                .get_entry_mut_or_create(vaddr(0), size)
                .unwrap()
                .bits();
            modify
                .unmap_sparse_region(vaddr(0), size as usize, size)
                .unwrap();
            assert!(!modify.finish().had_pending());
            assert_eq!(
                modify
                    .get_entry_mut_or_create(vaddr(0), size)
                    .unwrap()
                    .bits(),
                before
            );
        }
        // Leave the non-present leaf installed: normal destruction must
        // neither translate nor deallocate its data frame as a table.
        drop(table);
    }
}

#[cfg(target_arch = "aarch64")]
#[def_test]
fn page_table_walks_skip_nonpresent_arm_huge_leaves() {
    use crate::arch::aarch64::A64PageEntry;

    for size in [PageSize::Size2M, PageSize::Size1G] {
        let mut table =
            PageTable64::<TestMeta, A64PageEntry, OffsetTestHandler>::try_new().unwrap();
        table
            .modify()
            .map(vaddr(0), paddr(0x4000_0000), size, PagingFlags::empty())
            .unwrap();
        assert_eq!(table.query(vaddr(0)), Err(crate::PtError::NotMapped));
        assert_eq!(
            table.modify().unmap(vaddr(0)),
            Err(crate::PtError::NotMapped)
        );
    }
}

#[cfg(target_arch = "loongarch64")]
#[def_test]
fn sparse_unmap_traverses_loongarch_directories_without_valid_bit() {
    use crate::arch::loongarch64::La64PageEntry;

    let directory = La64PageEntry::new_table(paddr(PAGE_SIZE_4K));
    assert!(!directory.is_unused());
    assert!(!directory.is_present());
    let mut table = PageTable64::<TestMeta, La64PageEntry, OffsetTestHandler>::try_new().unwrap();
    let mut modify = table.modify();
    for address in [0, PAGE_SIZE_4K, 2 * PAGE_SIZE_4K] {
        modify
            .map(
                vaddr(address),
                paddr(0x400_0000 + address),
                PageSize::Size4K,
                PagingFlags::READ,
            )
            .unwrap();
    }
    modify.finish();
    modify
        .unmap_sparse_region(vaddr(PAGE_SIZE_4K), PAGE_SIZE_4K, PageSize::Size4K)
        .unwrap();
    assert_eq!(
        modify.query(vaddr(PAGE_SIZE_4K)),
        Err(crate::PtError::NotMapped)
    );
    assert!(modify.finish().had_pending());
    assert_eq!(modify.query(vaddr(0)).unwrap().0, paddr(0x400_0000));
    assert_eq!(
        modify.query(vaddr(2 * PAGE_SIZE_4K)).unwrap().0,
        paddr(0x400_2000)
    );
    modify
        .map(
            vaddr(PAGE_SIZE_4K),
            paddr(0x500_0000),
            PageSize::Size4K,
            PagingFlags::READ,
        )
        .unwrap();
    assert_eq!(
        modify.query(vaddr(PAGE_SIZE_4K)).unwrap().0,
        paddr(0x500_0000)
    );
}

#[def_test]
fn sparse_unmap_skips_missing_subtrees_without_flushing() {
    let mut table = TestPageTable::try_new().unwrap();
    let mut modify = table.modify();
    modify
        .unmap_sparse_region(vaddr(0), 1usize << 32, PageSize::Size4K)
        .unwrap();
    assert_eq!(modify.sparse_entries_visited, 1);
    assert!(!modify.finish().had_pending());
    modify
        .unmap_sparse_region(vaddr(0), 0, PageSize::Size4K)
        .unwrap();
    assert_eq!(modify.sparse_entries_visited, 1);
}

#[def_test]
fn sparse_unmap_preserves_neighbours_across_page_table_boundaries() {
    let mut table = TestPageTable::try_new().unwrap();
    let start = 1usize << 21;
    let end = (1usize << 30) + PAGE_SIZE_4K;
    let addresses = [
        start - PAGE_SIZE_4K,
        start,
        end - 2 * PAGE_SIZE_4K,
        end - PAGE_SIZE_4K,
        end,
    ];
    {
        let mut modify = table.modify();
        for (index, address) in addresses.into_iter().enumerate() {
            modify
                .map(
                    vaddr(address),
                    paddr(0x100_0000 + index * PAGE_SIZE_4K),
                    PageSize::Size4K,
                    PagingFlags::READ,
                )
                .unwrap();
        }
        modify.finish();
        modify
            .unmap_sparse_region(vaddr(start), end - start, PageSize::Size4K)
            .unwrap();
        // Allocated leaf tables still require scans; the 1 GiB hole does
        // not require hundreds of thousands of root-to-leaf walks.
        assert!(modify.sparse_entries_visited < 2048);
        assert!(modify.finish().had_pending());
        modify
            .unmap_sparse_region(vaddr(start), end - start, PageSize::Size4K)
            .unwrap();
        assert!(!modify.finish().had_pending());
    }
    assert!(table.query(vaddr(addresses[0])).is_ok());
    assert!(table.query(vaddr(addresses[4])).is_ok());
    for address in &addresses[1..4] {
        assert_eq!(table.query(vaddr(*address)), Err(crate::PtError::NotMapped));
    }
}

#[def_test]
fn sparse_unmap_batches_dense_leaf_invalidations() {
    let mut table = TestPageTable::try_new().unwrap();
    let mut modify = table.modify();
    for index in 0..32 {
        modify
            .map(
                vaddr(index * PAGE_SIZE_4K),
                paddr(0x200_0000 + index * PAGE_SIZE_4K),
                PageSize::Size4K,
                PagingFlags::READ,
            )
            .unwrap();
    }
    modify.finish();
    modify
        .unmap_sparse_region(vaddr(0), 32 * PAGE_SIZE_4K, PageSize::Size4K)
        .unwrap();
    assert_eq!(modify.sparse_entries_visited, 35);
    assert!(matches!(modify.flush, super::ToFlush::Full));
    assert!(modify.finish().had_pending());
    for index in 0..32 {
        assert_eq!(
            modify.query(vaddr(index * PAGE_SIZE_4K)),
            Err(crate::PtError::NotMapped)
        );
    }
}

#[def_test]
fn sparse_unmap_rejects_partial_huge_and_accepts_matching_leaves() {
    let mut table = TestPageTable::try_new().unwrap();
    let mut modify = table.modify();
    modify
        .map(
            vaddr(0x20_0000),
            paddr(0x40_0000),
            PageSize::Size2M,
            PagingFlags::READ,
        )
        .unwrap();
    modify
        .map(
            vaddr(1 << 30),
            paddr(2 << 30),
            PageSize::Size1G,
            PagingFlags::READ,
        )
        .unwrap();
    modify.finish();
    assert_eq!(
        modify.unmap_sparse_region(vaddr(0x20_1000), PAGE_SIZE_4K, PageSize::Size4K),
        Err(crate::PtError::MappedToHugePage)
    );
    assert!(!modify.finish().had_pending());
    assert_eq!(modify.query(vaddr(0x20_0000)).unwrap().2, PageSize::Size2M);
    modify
        .unmap_sparse_region(vaddr(0x20_0000), 1 << 21, PageSize::Size2M)
        .unwrap();
    modify
        .unmap_sparse_region(vaddr(1 << 30), 1 << 30, PageSize::Size1G)
        .unwrap();
    assert!(modify.finish().had_pending());
    assert_eq!(
        modify.query(vaddr(0x20_0000)),
        Err(crate::PtError::NotMapped)
    );
    assert_eq!(modify.query(vaddr(1 << 30)), Err(crate::PtError::NotMapped));
}

#[def_test]
fn sparse_unmap_validates_ranges_before_mutating() {
    let mut table = TestPageTable::try_new().unwrap();
    let mut modify = table.modify();
    modify
        .map(
            vaddr(0),
            paddr(0x300_0000),
            PageSize::Size4K,
            PagingFlags::READ,
        )
        .unwrap();
    modify.finish();
    for (start, size, expected) in [
        (1, PAGE_SIZE_4K, crate::PtError::NotAligned),
        (0, PAGE_SIZE_4K - 1, crate::PtError::NotAligned),
        (
            usize::MAX - PAGE_SIZE_4K + 1,
            PAGE_SIZE_4K,
            crate::PtError::InvalidAddress,
        ),
        (
            (1usize << 48) - PAGE_SIZE_4K,
            2 * PAGE_SIZE_4K,
            crate::PtError::InvalidAddress,
        ),
    ] {
        assert_eq!(
            modify.unmap_sparse_region(vaddr(start), size, PageSize::Size4K),
            Err(expected)
        );
    }
    assert_eq!(modify.sparse_entries_visited, 0);
    assert!(modify.query(vaddr(0)).is_ok());
    assert!(!modify.finish().had_pending());
}

#[def_test]
fn sparse_unmap_error_preserves_huge_leaf_and_prefix_flush() {
    let mut table = TestPageTable::try_new().unwrap();
    let mut modify = table.modify();
    modify
        .map(
            vaddr(0),
            paddr(0x500_0000),
            PageSize::Size4K,
            PagingFlags::READ,
        )
        .unwrap();
    modify
        .map(
            vaddr(1 << 21),
            paddr(0x600_0000),
            PageSize::Size2M,
            PagingFlags::READ,
        )
        .unwrap();
    modify.finish();
    assert_eq!(
        modify.unmap_sparse_region(vaddr(0), 2 << 21, PageSize::Size4K),
        Err(crate::PtError::MappedToHugePage)
    );
    assert_eq!(modify.query(vaddr(0)), Err(crate::PtError::NotMapped));
    assert_eq!(modify.query(vaddr(1 << 21)).unwrap().2, PageSize::Size2M);
    assert!(modify.finish().had_pending());
}

#[def_test]
fn sparse_unmap_supports_three_levels_and_high_addresses() {
    struct ThreeLevelMeta;
    impl PagingMetaData for ThreeLevelMeta {
        type VirtAddr = VirtAddr;

        const LEVELS: usize = 3;
        const PA_MAX_BITS: usize = 48;
        const VA_MAX_BITS: usize = 39;

        fn flush_tlb(_vaddr: Option<VirtAddr>) {}
    }
    let mut table = PageTable64::<ThreeLevelMeta, TestEntry, TestHandler>::try_new().unwrap();
    let mut modify = table.modify();
    let start = usize::MAX - (1usize << 30) + 1;
    modify
        .map(
            vaddr(start),
            paddr(0x400_0000),
            PageSize::Size4K,
            PagingFlags::READ,
        )
        .unwrap();
    modify.finish();
    assert_eq!(
        modify.unmap_sparse_region(vaddr(1 << 39), PAGE_SIZE_4K, PageSize::Size4K),
        Err(crate::PtError::InvalidAddress)
    );
    modify
        .unmap_sparse_region(vaddr(start), PAGE_SIZE_4K, PageSize::Size4K)
        .unwrap();
    assert_eq!(modify.sparse_entries_visited, 3);
    assert!(modify.finish().had_pending());
    assert_eq!(modify.query(vaddr(start)), Err(crate::PtError::NotMapped));
}

/// Verifies that an installed CPU-residency provider is consulted exactly
/// once per pending `finish()` flush, and that the mask it returns is the
/// one delivered to `flush_tlb_process_mask`.
///
/// All bookkeeping lives on the stack-local `ProviderCtx`, so this case is
/// independent of any sibling test running under the parallel scheduler.
#[cfg(all(feature = "smp", not(target_arch = "aarch64")))]
#[def_test]
fn user_page_table_flush_uses_installed_process_mask() {
    // The provider context outlives the page table, satisfying the
    // provider lifetime contract.
    let mut ctx = ProviderCtx::new();

    let mut expected_targets = [false; kbuild_config::NR_CPUS];
    let first_nonzero_cpu = 1;
    let primary_target_cpu = if kbuild_config::NR_CPUS > 1 {
        first_nonzero_cpu
    } else {
        0
    };
    ctx.provider_mask_bits[primary_target_cpu].store(true, Ordering::Relaxed);
    expected_targets[primary_target_cpu] = true;

    let secondary_target_cpu = kbuild_config::NR_CPUS.saturating_sub(1);
    if secondary_target_cpu != primary_target_cpu {
        ctx.provider_mask_bits[secondary_target_cpu].store(true, Ordering::Relaxed);
        expected_targets[secondary_target_cpu] = true;
    }

    let mut table = TestPageTable::try_new().expect("test page table");
    let ctx_ptr = NonNull::from(&mut ctx).cast();
    // SAFETY: `ctx` lives on this stack frame for the entire lifetime of
    // `table`, and `test_user_cpu_mask_provider` performs only the read +
    // bookkeeping documented on `ProviderCtx`.
    unsafe { table.set_user_cpu_mask_provider(ctx_ptr, test_user_cpu_mask_provider) };

    table
        .modify()
        .map(
            vaddr(0x20_0000),
            paddr(0x40_0000),
            PageSize::Size4K,
            PagingFlags::READ,
        )
        .expect("map test page");

    // `finish()` (via `Drop`) consults the provider once for the single
    // pending address, so exactly one observation must be recorded.
    assert_eq!(ctx.flush_calls.load(Ordering::Relaxed), 1);
    for (cpu, observed_bit) in ctx.observed_mask_bits.iter().enumerate() {
        assert_eq!(observed_bit.load(Ordering::Relaxed), expected_targets[cpu]);
    }
}

#[cfg(target_arch = "aarch64")]
mod aarch64_drop {
    use core::{
        ptr::NonNull,
        sync::atomic::{AtomicU16, AtomicUsize, Ordering},
    };

    use super::{
        PageSize, PageTable64, PagingFlags, PagingHandler, PagingMetaData, PhysAddr, TestEntry,
        TestHandler, TestMeta, VirtAddr, def_test, paddr, vaddr,
    };

    // Only the serial tests below use these probes. The scoped guard resets
    // them on entry and exit; sibling tests use different metadata/handlers.
    // Relaxed accesses record same-thread call order, not CPU synchronization.
    static ASID_FLUSHES: AtomicUsize = AtomicUsize::new(0);
    static LAST_ASID: AtomicU16 = AtomicU16::new(0);
    static OTHER_FLUSHES: AtomicUsize = AtomicUsize::new(0);
    static FREED_FRAMES: AtomicUsize = AtomicUsize::new(0);
    static FREED_WITHOUT_ASID_FLUSH: AtomicUsize = AtomicUsize::new(0);

    struct ProbeGuard;

    impl ProbeGuard {
        fn new() -> Self {
            Self::reset();
            Self
        }

        fn reset() {
            ASID_FLUSHES.store(0, Ordering::Relaxed);
            LAST_ASID.store(0, Ordering::Relaxed);
            OTHER_FLUSHES.store(0, Ordering::Relaxed);
            FREED_FRAMES.store(0, Ordering::Relaxed);
            FREED_WITHOUT_ASID_FLUSH.store(0, Ordering::Relaxed);
        }
    }

    impl Drop for ProbeGuard {
        fn drop(&mut self) {
            Self::reset();
        }
    }

    struct DropMeta;

    impl PagingMetaData for DropMeta {
        type VirtAddr = VirtAddr;

        const LEVELS: usize = TestMeta::LEVELS;
        const PA_MAX_BITS: usize = TestMeta::PA_MAX_BITS;
        const VA_MAX_BITS: usize = TestMeta::VA_MAX_BITS;

        fn vaddr_is_valid(addr: usize) -> bool {
            TestMeta::vaddr_is_valid(addr)
        }

        fn paddr_is_valid(addr: usize) -> bool {
            TestMeta::paddr_is_valid(addr)
        }

        fn flush_tlb(_vaddr: Option<VirtAddr>) {
            OTHER_FLUSHES.fetch_add(1, Ordering::Relaxed);
        }

        fn flush_tlb_process_asid(vaddr: Option<VirtAddr>, asid: u16) {
            if vaddr.is_none() {
                LAST_ASID.store(asid, Ordering::Relaxed);
                ASID_FLUSHES.fetch_add(1, Ordering::Relaxed);
            } else {
                OTHER_FLUSHES.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    struct DropHandler;

    impl PagingHandler for DropHandler {
        fn alloc_frame() -> Option<PhysAddr> {
            TestHandler::alloc_frame()
        }

        fn dealloc_frame(paddr: PhysAddr) {
            if ASID_FLUSHES.load(Ordering::Relaxed) == 0 {
                FREED_WITHOUT_ASID_FLUSH.fetch_add(1, Ordering::Relaxed);
            }
            FREED_FRAMES.fetch_add(1, Ordering::Relaxed);
            TestHandler::dealloc_frame(paddr);
        }

        fn p2v(paddr: PhysAddr) -> VirtAddr {
            TestHandler::p2v(paddr)
        }
    }

    type DropPageTable = PageTable64<DropMeta, TestEntry, DropHandler>;

    /// Reads the ASID associated with a test-owned, inactive page table.
    ///
    /// # Safety
    ///
    /// `ctx` must point to an `AtomicU16` that remains live until the page
    /// table, including its destructor, has finished using this callback.
    unsafe fn current_asid(ctx: NonNull<()>) -> u16 {
        // SAFETY: Each test keeps the stack-owned ASID alive until after
        // dropping its page table. This callback only reads that atomic.
        unsafe { ctx.cast::<AtomicU16>().as_ref() }.load(Ordering::Relaxed)
    }

    fn map_test_page(table: &mut DropPageTable) {
        table
            .modify()
            .map(
                vaddr(0x4000),
                paddr(0x20_0000),
                PageSize::Size4K,
                PagingFlags::READ,
            )
            .expect("map test page");
        // Mapping finalization is complete. Observe only table destruction,
        // which must invalidate the ASID even with no pending leaf flushes.
        ProbeGuard::reset();
    }

    #[def_test(serial)]
    fn user_drop_invalidates_latest_asid_before_freeing_frames() {
        let _probe = ProbeGuard::new();
        let asid = AtomicU16::new(7);
        let mut table = DropPageTable::try_new().expect("user page table");
        // SAFETY: `asid` outlives `table`, the callback only reads it, and
        // this test table is never installed as a hardware translation root.
        unsafe { table.set_user_asid_provider(NonNull::from(&asid).cast(), current_asid) };
        map_test_page(&mut table);
        asid.store(0x1234, Ordering::Relaxed);

        drop(table);

        assert_eq!(ASID_FLUSHES.load(Ordering::Relaxed), 1);
        assert_eq!(LAST_ASID.load(Ordering::Relaxed), 0x1234);
        assert_eq!(OTHER_FLUSHES.load(Ordering::Relaxed), 0);
        assert_eq!(FREED_WITHOUT_ASID_FLUSH.load(Ordering::Relaxed), 0);
        assert_eq!(FREED_FRAMES.load(Ordering::Relaxed), DropMeta::LEVELS);
    }

    #[def_test(serial)]
    fn user_drop_without_provider_skips_asid_flush() {
        let _probe = ProbeGuard::new();
        let mut table = DropPageTable::try_new().expect("user page table");
        map_test_page(&mut table);

        drop(table);

        assert_eq!(ASID_FLUSHES.load(Ordering::Relaxed), 0);
        assert_eq!(OTHER_FLUSHES.load(Ordering::Relaxed), 0);
        assert_eq!(FREED_FRAMES.load(Ordering::Relaxed), DropMeta::LEVELS);
    }

    #[def_test(serial)]
    fn kernel_drop_skips_user_asid_flush() {
        let _probe = ProbeGuard::new();
        let asid = AtomicU16::new(0x5678);
        let mut table = DropPageTable::try_new_kernel().expect("kernel page table");
        // SAFETY: The context stays live until after `table` is dropped and
        // the callback is read-only. The table is never activated. Installing
        // a provider here exercises the kernel-table exclusion in Drop.
        unsafe { table.set_user_asid_provider(NonNull::from(&asid).cast(), current_asid) };
        map_test_page(&mut table);

        drop(table);

        assert_eq!(ASID_FLUSHES.load(Ordering::Relaxed), 0);
        assert_eq!(OTHER_FLUSHES.load(Ordering::Relaxed), 0);
        assert_eq!(FREED_FRAMES.load(Ordering::Relaxed), DropMeta::LEVELS);
    }
}

/// Models an architecture whose walkers cannot cache a translation for an
/// invalid entry, so a fresh mapping skips invalidation and only fences.
struct FreshMapMeta;

static FRESH_MAP_INVALIDATIONS: AtomicUsize = AtomicUsize::new(0);
static FRESH_MAP_FENCES: AtomicUsize = AtomicUsize::new(0);

impl PagingMetaData for FreshMapMeta {
    type VirtAddr = VirtAddr;

    const INVALID_TO_VALID_NEEDS_TLBI: bool = false;
    const LEVELS: usize = 4;
    const PA_MAX_BITS: usize = usize::BITS as usize - 1;
    const VA_MAX_BITS: usize = usize::BITS as usize - 1;

    fn vaddr_is_valid(_vaddr: usize) -> bool {
        true
    }

    fn paddr_is_valid(_paddr: usize) -> bool {
        true
    }

    fn flush_tlb(_vaddr: Option<Self::VirtAddr>) {
        FRESH_MAP_INVALIDATIONS.fetch_add(1, Ordering::Relaxed);
    }

    fn fence_fresh_map() {
        FRESH_MAP_FENCES.fetch_add(1, Ordering::Relaxed);
    }
}

type FreshMapPageTable = PageTable64<FreshMapMeta, TestEntry, TestHandler>;

fn reset_fresh_map_counters() {
    FRESH_MAP_INVALIDATIONS.store(0, Ordering::Relaxed);
    FRESH_MAP_FENCES.store(0, Ordering::Relaxed);
}

#[def_test(serial)]
fn fresh_map_fences_once_without_invalidating() {
    reset_fresh_map_counters();
    let mut table = FreshMapPageTable::try_new_kernel().expect("fresh-map page table");

    let receipt = {
        let mut guard = table.modify();
        guard
            .map(
                vaddr(0x4000),
                paddr(0x20_0000),
                PageSize::Size4K,
                PagingFlags::READ | PagingFlags::WRITE,
            )
            .expect("first fresh map");
        guard
            .map(
                vaddr(0x5000),
                paddr(0x21_0000),
                PageSize::Size4K,
                PagingFlags::READ | PagingFlags::WRITE,
            )
            .expect("second fresh map");
        guard.finish()
    };

    assert!(!receipt.had_pending());
    assert_eq!(FRESH_MAP_INVALIDATIONS.load(Ordering::Relaxed), 0);
    // Two fresh maps in one boundary share a single fence.
    assert_eq!(FRESH_MAP_FENCES.load(Ordering::Relaxed), 1);
}

#[def_test(serial)]
fn unmap_still_invalidates_on_a_fence_only_architecture() {
    reset_fresh_map_counters();
    let mut table = FreshMapPageTable::try_new_kernel().expect("fresh-map page table");
    {
        let mut guard = table.modify();
        guard
            .map(
                vaddr(0x4000),
                paddr(0x20_0000),
                PageSize::Size4K,
                PagingFlags::READ | PagingFlags::WRITE,
            )
            .expect("fresh map before unmap");
        guard.finish();
    }
    reset_fresh_map_counters();

    let receipt = {
        let mut guard = table.modify();
        let _ = guard.unmap(vaddr(0x4000)).expect("unmap fresh page");
        guard.finish()
    };

    assert!(receipt.had_pending());
    assert!(FRESH_MAP_INVALIDATIONS.load(Ordering::Relaxed) > 0);
    // The invalidation sequence already orders the entry write.
    assert_eq!(FRESH_MAP_FENCES.load(Ordering::Relaxed), 0);
}

#[def_test(serial)]
fn fresh_map_invalidates_when_architecture_requires_it() {
    // `TestMeta` keeps the conservative default, so installing into an
    // unused entry must still be reported as a pending invalidation.
    let mut table = TestPageTable::try_new_kernel().expect("default-policy page table");
    let mut guard = table.modify();
    guard
        .map(
            vaddr(0x4000),
            paddr(0x20_0000),
            PageSize::Size4K,
            PagingFlags::READ | PagingFlags::WRITE,
        )
        .expect("fresh map under default policy");
    let receipt = guard.finish();
    assert!(receipt.had_pending());
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! SMP bring-up helpers for the runtime.
use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicUsize, Ordering},
};

use kbuild_config::{NR_CPUS, TASK_STACK_SIZE};
use kcpu_id_map::{LogicalCpuId, for_each_present_logical_cpu};
use kernel_boot::SecondaryKernelEntry;
use kerrno::KResult;
use khal::mem::{VirtAddr, v2p};

#[unsafe(link_section = ".bss.stack")]
static SECONDARY_BOOT_STACKS: SecondaryBootStacks = SecondaryBootStacks::new();

static ENTERED_CPUS: AtomicUsize = AtomicUsize::new(1);

struct SecondaryBootStacks {
    stacks: UnsafeCell<[[u8; TASK_STACK_SIZE]; NR_CPUS - 1]>,
}

impl SecondaryBootStacks {
    const fn new() -> Self {
        Self {
            stacks: UnsafeCell::new([[0; TASK_STACK_SIZE]; NR_CPUS - 1]),
        }
    }

    fn stack_top(&self, secondary_cpu_index: usize) -> VirtAddr {
        // SAFETY: `secondary_cpu_index` is allocated monotonically by the
        // serialized AP bring-up loop and is bounded by `NR_CPUS - 1`. The
        // boot-stack backing array lives for the entire kernel lifetime, so
        // advancing to the selected slot and computing its one-past-end
        // address stays within the same allocation.
        let stack = unsafe {
            self.stacks
                .get()
                .cast::<[u8; TASK_STACK_SIZE]>()
                .add(secondary_cpu_index)
        };
        VirtAddr::from(stack.cast::<u8>().wrapping_add(TASK_STACK_SIZE) as usize)
    }
}

// SAFETY: Access is restricted to `stack_top()`, which only derives raw stack
// end addresses for uniquely assigned per-CPU slots during serialized AP
// bring-up. No shared references to the inner arrays are exposed.
unsafe impl Sync for SecondaryBootStacks {}

/// Start all secondary CPUs and wait until they enter the runtime.
#[allow(clippy::absurd_extreme_comparisons)]
pub fn start_secondary_cpus(primary_cpu_id: LogicalCpuId) -> KResult {
    let mut secondary_logical_cpu_id = 0;
    let mut start_result = Ok(());
    for_each_present_logical_cpu(|_, logical_cpu_id, _| {
        if start_result.is_err() {
            return;
        }
        if logical_cpu_id == primary_cpu_id || secondary_logical_cpu_id >= NR_CPUS - 1 {
            return;
        }

        let stack_top = v2p(SECONDARY_BOOT_STACKS.stack_top(secondary_logical_cpu_id));

        #[cfg(target_arch = "aarch64")]
        kernel_boot::arch::set_secondary_boot_context(logical_cpu_id, stack_top.as_usize());

        debug!("starting CPU {}...", logical_cpu_id.as_usize());
        if let Err(err) = khal::power::boot_ap(logical_cpu_id, stack_top.as_usize()) {
            start_result = Err(err);
            return;
        }
        secondary_logical_cpu_id += 1;

        while ENTERED_CPUS.load(Ordering::Acquire) <= secondary_logical_cpu_id {
            core::hint::spin_loop();
        }
    });
    start_result
}

/// The main entry point of the runtime for secondary cores.
///
/// It is called from the bootstrapping code in the specific platform crate.
pub fn rust_main_secondary(logical_cpu_id: LogicalCpuId) -> ! {
    khal::percpu::init_secondary(logical_cpu_id);
    kcpu::init_trap();

    ENTERED_CPUS.fetch_add(1, Ordering::Release);
    info!("Secondary CPU {} started.", logical_cpu_id.as_usize());

    memspace::init_memory_management_secondary();

    khal::final_init_secondary(logical_cpu_id);

    ktask::init_scheduler_secondary();

    #[cfg(feature = "ipi")]
    kipi::init();

    info!("Secondary CPU {:x} init OK.", logical_cpu_id.as_usize());
    super::INITED_CPUS.fetch_add(1, Ordering::Release);

    while !super::is_init_ok() {
        core::hint::spin_loop();
    }

    #[cfg(feature = "ipi")]
    khal::irq::enable(kbuild_config::IPI_IRQ, true);

    #[cfg(feature = "pmu")]
    khal::irq::enable(kbuild_config::PMU_IRQ, true);

    karch::enable_local_irq();

    #[cfg(feature = "watchdog")]
    watchdog::init_secondary();

    ktask::run_idle();
}

#[kiface::provide]
impl SecondaryKernelEntry {
    fn enter(logical_cpu_id: LogicalCpuId) -> ! {
        rust_main_secondary(logical_cpu_id)
    }
}

// ---------------------------------------------------------------------------
// Integration tests (kernel-mode, run on QEMU via `make run UNITTEST=y`)
// ---------------------------------------------------------------------------
#[cfg(all(feature = "smp", unittest))]
mod tests_tlb_shootdown {
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use kcpu_id_map::LogicalCpuId;
    use khal::percpu::this_cpu_id;
    use ktask::KCpuMask;
    use memaddr::{PhysAddr, VirtAddr};
    use unittest::{assert, assert_eq, def_test};

    fn enable_tlb_shootdown_for_test() {
        // These integration tests exercise the runtime after SMP bring-up, so
        // they must establish the same "all APs are ready" precondition that
        // production TLB shootdowns require. Do not rely on other unit tests
        // leaving this global gate enabled.
        kipi::tlb::mark_all_cpus_started();
    }

    /// Test C: Proves flush_all_cpus sends IPIs to all online CPUs and waits
    /// for remote completion.
    #[def_test(serial)]
    fn test_flush_all_targeted_shootdown() {
        let cpu_num = kcpu_id_map::nr_cpus();
        if cpu_num >= 2 {
            enable_tlb_shootdown_for_test();
            // Call flush_all — if it returns, the remote CPU received the IPI,
            // called handle_shootdown() → karch::flush_tlb(), and set COMPLETED.
            kipi::tlb::trigger_flush_all(None);
        }
    }

    /// Test D: Proves context switch updates on_cpu_mask.
    #[def_test(serial)]
    fn test_context_switch_updates_on_cpu_mask() {
        let cpu_num = kcpu_id_map::nr_cpus();
        if cpu_num >= 2 {
            let remote_cpu = if this_cpu_id() == LogicalCpuId::new(0) {
                1
            } else {
                0
            };

            static TASK_CPU: AtomicUsize = AtomicUsize::new(usize::MAX);
            static TASK_MASK_BIT: AtomicBool = AtomicBool::new(false);

            let mut affinity = KCpuMask::new();
            affinity.set(remote_cpu, true);

            let task = ktask::TaskInner::new_kthread(
                move || {
                    TASK_CPU.store(khal::percpu::this_cpu_id().as_usize(), Ordering::Release);
                    if let Some(t) = ktask::current_may_uninit() {
                        TASK_MASK_BIT.store(t.on_cpu_mask().get(remote_cpu), Ordering::Release);
                    }
                },
                "test_ctx_mask".into(),
                0x2000,
            )
            .expect("test kernel thread identity allocation should succeed");
            task.set_cpumask(affinity);
            let task = ktask::spawn_task(task);

            task.join();
            assert_eq!(TASK_CPU.load(Ordering::Acquire), remote_cpu);
            assert!(TASK_MASK_BIT.load(Ordering::Acquire));
        }
    }

    /// Test E: Proves PageTableMut::finish() calls flush_tlb_all_cpus.
    ///
    /// Creates a real page table, maps a page (setting ToFlush::Addresses),
    /// then drops the PageTableMut which calls finish() → flush_tlb_all_cpus
    /// → flush_remote → targeted IPI → completion.
    /// Flush completion does not rebuild residency state.
    #[def_test(serial)]
    fn test_finish_triggers_cross_cpu_flush() {
        let cpu_num = kcpu_id_map::nr_cpus();
        if cpu_num >= 2 {
            enable_tlb_shootdown_for_test();
            // Create a new page table (allocates a root page via kernel allocator).
            let mut pt = khal::paging::PageTable::try_new().expect("test page table alloc");

            // Map a page to set ToFlush::Addresses.
            let test_vaddr = VirtAddr::from(0x7F00_0000usize);
            let mut pt_mut = pt.modify();
            let _ = pt_mut.map(
                test_vaddr,
                PhysAddr::from(0x8000_0000usize),
                khal::paging::PageSize::Size4K,
                khal::paging::MappingFlags::READ,
            );

            // Dropping pt_mut calls finish() → flush_tlb_all_cpus → flush_remote.
            // On architectures without hardware broadcast (x86_64, riscv64), this sends IPIs.
            // If the remote CPU doesn't respond, this hangs.
            drop(pt_mut);
        }
    }

    /// Test F: Proves TLB shootdown actually eliminates stale TLB entries.
    ///
    /// Maps a virtual address to physical page P1 in the kernel page table,
    /// has the remote CPU read it (populating its TLB with V→P1), then remaps
    /// V to P2 with a shootdown. Verifies the remote CPU now reads P2's
    /// content, proving the stale V→P1 TLB entry was invalidated.
    #[def_test(serial)]
    fn test_shootdown_clears_stale_tlb() {
        let cpu_num = kcpu_id_map::nr_cpus();
        if cpu_num >= 2 {
            enable_tlb_shootdown_for_test();
            let my_cpu = this_cpu_id();
            let remote_cpu = LogicalCpuId::new(if my_cpu == LogicalCpuId::new(0) { 1 } else { 0 });

            // Allocate two physical pages with different magic values.
            let v1 = kalloc::global_allocator()
                .alloc_pages(1, memaddr::PAGE_SIZE_4K, kalloc::UsageKind::PageTable)
                .expect("alloc page 1");
            let v2 = kalloc::global_allocator()
                .alloc_pages(1, memaddr::PAGE_SIZE_4K, kalloc::UsageKind::PageTable)
                .expect("alloc page 2");
            let p1 = khal::mem::v2p(VirtAddr::from(v1));
            let p2 = khal::mem::v2p(VirtAddr::from(v2));

            const MAGIC_A: u64 = 0xDEAD_BEEF_CAFE_BABE;
            const MAGIC_B: u64 = 0xCAFE_BABE_DEAD_BEEF;

            // SAFETY: v1 and v2 are valid kernel-virtual addresses backed by
            // freshly allocated physical pages owned by this test.  Writing
            // magic values through volatile pointers is safe because no other
            // code holds references to these pages.
            unsafe {
                core::ptr::write_volatile(v1 as *mut u64, MAGIC_A);
                core::ptr::write_volatile(v2 as *mut u64, MAGIC_B);
            }

            // Pick a kernel-high virtual address in the reserved region.
            // Must be valid across all architectures (SV39 needs ≥0xFFFFFFC0).
            let test_vaddr = VirtAddr::from(kaddr_layout::IOMAP_VADDR + kaddr_layout::IOMAP_VSIZE);

            // Phase 1: Map V → P1; remote CPU reads V → sees MAGIC_A.
            {
                let mut aspace = memspace::kernel_layout().lock();
                let mut pt_mut = aspace.page_table_mut().modify();
                pt_mut
                    .map(
                        test_vaddr,
                        p1,
                        khal::paging::PageSize::Size4K,
                        khal::paging::MappingFlags::READ | khal::paging::MappingFlags::WRITE,
                    )
                    .expect("map V→P1");
            }

            static REMOTE_READ: AtomicUsize = AtomicUsize::new(0);

            REMOTE_READ.store(0, Ordering::Relaxed);
            kipi::run_on_cpu(remote_cpu, move || {
                // SAFETY: test_vaddr is mapped read/write to p1 in the kernel page table
                // before this closure runs, and p1 backs an initialized u64 written above.
                let val = unsafe { core::ptr::read_volatile(test_vaddr.as_usize() as *const u64) };
                REMOTE_READ.store(val as usize, Ordering::Release);
            })
            .unwrap();
            while REMOTE_READ.load(Ordering::Acquire) == 0 {
                core::hint::spin_loop();
            }
            assert_eq!(REMOTE_READ.load(Ordering::Acquire), MAGIC_A as usize);

            // Phase 2: Set on_cpu_mask so shootdown targets the remote CPU.
            let task = ktask::current_may_uninit().unwrap();
            task.set_on_cpu_mask_bit(remote_cpu);
            {
                let mut aspace = memspace::kernel_layout().lock();
                let mut pt_mut = aspace.page_table_mut().modify();
                let _ = pt_mut.unmap(test_vaddr).expect("unmap V");
                pt_mut
                    .map(
                        test_vaddr,
                        p2,
                        khal::paging::PageSize::Size4K,
                        khal::paging::MappingFlags::READ | khal::paging::MappingFlags::WRITE,
                    )
                    .expect("map V→P2");
            }

            // Remote CPU reads V → must see MAGIC_B (proves stale TLB was flushed).
            REMOTE_READ.store(0, Ordering::Relaxed);
            kipi::run_on_cpu(remote_cpu, move || {
                // SAFETY: test_vaddr is remapped read/write to p2 in the kernel page table
                // before this closure runs, and p2 backs an initialized u64 written above.
                let val = unsafe { core::ptr::read_volatile(test_vaddr.as_usize() as *const u64) };
                REMOTE_READ.store(val as usize, Ordering::Release);
            })
            .unwrap();
            while REMOTE_READ.load(Ordering::Acquire) == 0 {
                core::hint::spin_loop();
            }
            assert_eq!(REMOTE_READ.load(Ordering::Acquire), MAGIC_B as usize);

            // Cleanup: unmap V, free pages.
            {
                let mut aspace = memspace::kernel_layout().lock();
                let mut pt_mut = aspace.page_table_mut().modify();
                let _ = pt_mut.unmap(test_vaddr);
            }
            kalloc::global_allocator().dealloc_pages(v1, 1, kalloc::UsageKind::PageTable);
            kalloc::global_allocator().dealloc_pages(v2, 1, kalloc::UsageKind::PageTable);
        }
    }

    /// Test G: Proves that kernel page table TLB shootdown reaches ALL
    /// online CPUs, regardless of the current task's CPU residency mask.
    ///
    /// Kernel page table modifications (is_kernel=true) go through
    /// `flush_all_cpus()`, which builds a full CPU mask via
    /// `for_each_present_logical_cpu()`. Every online CPU receives a
    /// shootdown IPI regardless of user-mm residency targeting.
    ///
    /// ### What this test verifies
    ///
    /// 1. **Correct shootdown**: after modifying the kernel page table, the
    ///    remote CPU still receives a TLB shootdown IPI and reads the new
    ///    mapping (MAGIC_B instead of stale MAGIC_A).
    ///
    /// 2. **Task-local fallback irrelevant**: kernel page table shootdown does
    ///    not depend on the current task's local fallback mask.
    #[def_test(serial)]
    fn test_kernel_pt_miss_remote_without_mask() {
        let cpu_num = kcpu_id_map::nr_cpus();
        if cpu_num >= 2 {
            enable_tlb_shootdown_for_test();
            let my_cpu = this_cpu_id();
            let remote_cpu = LogicalCpuId::new(if my_cpu == LogicalCpuId::new(0) { 1 } else { 0 });

            // Ensure the current task's mask does NOT include the remote CPU.
            // This is the natural state for a kernel task that has only ever
            // run on its own CPU.
            let task = ktask::current_may_uninit().unwrap();
            task.reset_on_cpu_mask(my_cpu);

            // Allocate two physical pages with different magic values.
            let v1 = kalloc::global_allocator()
                .alloc_pages(1, memaddr::PAGE_SIZE_4K, kalloc::UsageKind::PageTable)
                .expect("alloc page 1");
            let v2 = kalloc::global_allocator()
                .alloc_pages(1, memaddr::PAGE_SIZE_4K, kalloc::UsageKind::PageTable)
                .expect("alloc page 2");
            let p1 = khal::mem::v2p(VirtAddr::from(v1));
            let p2 = khal::mem::v2p(VirtAddr::from(v2));

            const MAGIC_A: u64 = 0xDEAD_BEEF_CAFE_BABE;
            const MAGIC_B: u64 = 0xCAFE_BABE_DEAD_BEEF;

            // SAFETY: v1 and v2 are valid kernel-virtual addresses backed by
            // freshly allocated physical pages owned by this test.  Writing
            // magic values through volatile pointers is safe because no other
            // code holds references to these pages.
            unsafe {
                core::ptr::write_volatile(v1 as *mut u64, MAGIC_A);
                core::ptr::write_volatile(v2 as *mut u64, MAGIC_B);
            }

            let test_vaddr = VirtAddr::from(kaddr_layout::IOMAP_VADDR + kaddr_layout::IOMAP_VSIZE);

            // Phase 1: Map V → P1 in the kernel page table; remote CPU reads V.
            // This populates the remote CPU's TLB with V → P1.
            {
                let mut aspace = memspace::kernel_layout().lock();
                let mut pt_mut = aspace.page_table_mut().modify();
                pt_mut
                    .map(
                        test_vaddr,
                        p1,
                        khal::paging::PageSize::Size4K,
                        khal::paging::MappingFlags::READ | khal::paging::MappingFlags::WRITE,
                    )
                    .expect("map V→P1");
            }

            static REMOTE_READ_G: AtomicUsize = AtomicUsize::new(0);

            REMOTE_READ_G.store(0, Ordering::Relaxed);
            kipi::run_on_cpu(remote_cpu, move || {
                // SAFETY: test_vaddr is mapped read/write to p1 in the kernel page table
                // before this closure runs, and p1 backs an initialized u64 written above.
                let val = unsafe { core::ptr::read_volatile(test_vaddr.as_usize() as *const u64) };
                REMOTE_READ_G.store(val as usize, Ordering::Release);
            })
            .unwrap();
            while REMOTE_READ_G.load(Ordering::Acquire) == 0 {
                core::hint::spin_loop();
            }
            assert_eq!(
                REMOTE_READ_G.load(Ordering::Acquire),
                MAGIC_A as usize,
                "Phase 1: remote CPU should read MAGIC_A from V→P1 mapping"
            );

            // Phase 2: Remap V → P2 in the kernel page table.
            // `kernel_layout()` creates a kernel page table (is_kernel=true), so
            // `PageTableMut::finish()` calls `flush_all_cpus()`, which builds a
            // mask of ALL online CPUs via `for_each_present_logical_cpu()`.
            // The remote CPU WILL receive a shootdown IPI.
            {
                let mut aspace = memspace::kernel_layout().lock();
                let mut pt_mut = aspace.page_table_mut().modify();
                let _ = pt_mut.unmap(test_vaddr).expect("unmap V");
                pt_mut
                    .map(
                        test_vaddr,
                        p2,
                        khal::paging::PageSize::Size4K,
                        khal::paging::MappingFlags::READ | khal::paging::MappingFlags::WRITE,
                    )
                    .expect("map V→P2");
            }

            // ---- Assertion: TLB shootdown must reach ALL online CPUs ----
            //
            // After modifying the global kernel page table, every online CPU
            // must see the new mapping.  We assert the CORRECT behavior:
            // the remote CPU should read MAGIC_B (P2's content).
            //
            // Kernel page table modifications (is_kernel=true) go through
            // `flush_all_cpus()`, which builds a full CPU mask via
            // `for_each_present_logical_cpu()`.  Every online CPU — including
            // the remote CPU — receives a shootdown IPI, so this assertion
            // PASSES on all architectures.
            REMOTE_READ_G.store(0, Ordering::Relaxed);
            kipi::run_on_cpu(remote_cpu, move || {
                // SAFETY: test_vaddr is remapped read/write to p2 in the kernel page table
                // before this closure runs, and p2 backs an initialized u64 written above.
                let val = unsafe { core::ptr::read_volatile(test_vaddr.as_usize() as *const u64) };
                REMOTE_READ_G.store(val as usize, Ordering::Release);
            })
            .unwrap();
            while REMOTE_READ_G.load(Ordering::Acquire) == 0 {
                core::hint::spin_loop();
            }
            let remote_val = REMOTE_READ_G.load(Ordering::Acquire);

            assert_eq!(
                remote_val, MAGIC_B as usize,
                "BUG (iomap TLB): remote CPU {remote_cpu:?} read {remote_val:#x}, expected \
                 MAGIC_B ({MAGIC_B:#x}) from the V→P2 mapping.\nThe kernel page table was \
                 modified and `flush_all_cpus()` should have targeted ALL online CPUs via \
                 `for_each_present_logical_cpu()`, including the remote CPU.  If this assertion \
                 fails, the remote CPU did not receive a TLB shootdown IPI or its TLB was not \
                 properly invalidated.",
            );

            // Cleanup: unmap V, free pages.
            {
                let mut aspace = memspace::kernel_layout().lock();
                let mut pt_mut = aspace.page_table_mut().modify();
                let _ = pt_mut.unmap(test_vaddr);
            }
            kalloc::global_allocator().dealloc_pages(v1, 1, kalloc::UsageKind::PageTable);
            kalloc::global_allocator().dealloc_pages(v2, 1, kalloc::UsageKind::PageTable);
        }
    }
}

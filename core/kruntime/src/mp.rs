// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! SMP bring-up helpers for the runtime.
use core::sync::atomic::{AtomicUsize, Ordering};

use kbuild_config::{CPU_NUM, TASK_STACK_SIZE};
use kcpu_id_map::{LogicalCpuId, for_each_present_logical_cpu};
use kernel_boot::{SECOND_KERNEL_ENTRY, register_boot_init};
use khal::mem::{VirtAddr, v2p};
use kthread::AsThread;

#[unsafe(link_section = ".bss.stack")]
static mut SECONDARY_BOOT_STACK: [[u8; TASK_STACK_SIZE]; CPU_NUM - 1] =
    [[0; TASK_STACK_SIZE]; CPU_NUM - 1];

static ENTERED_CPUS: AtomicUsize = AtomicUsize::new(1);

/// Start all secondary CPUs and wait until they enter the runtime.
#[allow(clippy::absurd_extreme_comparisons)]
pub fn start_secondary_cpus(primary_cpu_id: LogicalCpuId) {
    let mut secondary_logical_cpu_id = 0;
    for_each_present_logical_cpu(|_, logical_cpu_id, _| {
        if logical_cpu_id == primary_cpu_id || secondary_logical_cpu_id >= CPU_NUM - 1 {
            return;
        }

        let stack_top = v2p(VirtAddr::from(unsafe {
            SECONDARY_BOOT_STACK[secondary_logical_cpu_id]
                .as_ptr_range()
                .end as usize
        }));

        debug!("starting CPU {}...", logical_cpu_id.as_usize());
        khal::power::boot_ap(logical_cpu_id, stack_top.as_usize());
        secondary_logical_cpu_id += 1;

        while ENTERED_CPUS.load(Ordering::Acquire) <= secondary_logical_cpu_id {
            core::hint::spin_loop();
        }
    });
}

/// The main entry point of the runtime for secondary cores.
///
/// It is called from the bootstrapping code in the specific platform crate.
#[register_boot_init(SECOND_KERNEL_ENTRY)]
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

/// Implements the [`TaskCpuResidencyIf`](kipi::tlb::TaskCpuResidencyIf)
/// interface so the TLB shootdown code can query per-task CPU residency.
struct TaskCpuResidencyImpl;

#[crate_interface::impl_interface]
impl kipi::tlb::TaskCpuResidencyIf for TaskCpuResidencyImpl {
    fn current_on_cpu_mask() -> kcpu_id_map::KCpuMask {
        let Some(current) = ktask::current_may_uninit() else {
            return kcpu_id_map::KCpuMask::new();
        };
        // Kernel tasks (e.g. during boot) don't have a Thread; fall back to
        // the current task's mask only.
        let Some(thread) = current.try_as_thread() else {
            return current.on_cpu_mask();
        };
        // Collect on_cpu_mask from all threads sharing the same address space.
        // Per-task masks alone may not cover all CPUs running sibling threads,
        // which would cause TLB shootdowns to miss those CPUs.
        let mut mask = current.on_cpu_mask();
        for tid in thread.proc_state.proc.threads() {
            if let Ok(task) = kthread::get_task(tid) {
                mask |= task.on_cpu_mask();
            }
        }
        mask
    }

    fn reset_on_cpu_mask() {
        let Some(current) = ktask::current_may_uninit() else {
            return;
        };
        // Kernel tasks don't have a Thread; fall back to the current task.
        let Some(thread) = current.try_as_thread() else {
            current.reset_on_cpu_mask(khal::percpu::this_cpu_id());
            return;
        };
        // Reset every thread's mask to its own current CPU — the shootdown has
        // just invalidated stale TLB entries on all targeted CPUs, so each
        // thread only needs to track the CPU it is currently on.
        for tid in thread.proc_state.proc.threads() {
            if let Ok(task) = kthread::get_task(tid) {
                task.reset_on_cpu_mask(task.cpu_id());
            }
        }
        // Ensure the current task's mask always reflects this CPU, even if
        // task.cpu_id() hasn't been updated by the scheduler yet.
        current.reset_on_cpu_mask(khal::percpu::this_cpu_id());
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

    /// Test B: Proves TaskCpuResidencyIf correctly reads on_cpu_mask and
    /// reset_on_cpu_mask correctly resets it.
    #[def_test(serial)]
    fn test_task_cpu_residency_interface() {
        let cpu_num = kbuild_config::CPU_NUM;
        if cpu_num >= 4 {
            let task = ktask::current_may_uninit().unwrap();
            // Clear any existing bits
            task.reset_on_cpu_mask(this_cpu_id());

            task.set_on_cpu_mask_bit(LogicalCpuId::new(0));
            task.set_on_cpu_mask_bit(LogicalCpuId::new(2));

            let mask = task.on_cpu_mask();
            assert!(mask.get(0));
            assert!(mask.get(2));
            assert!(!mask.get(1));

            let raw = crate_interface::call_interface!(
                kipi::tlb::TaskCpuResidencyIf::current_on_cpu_mask()
            );
            // raw mask includes this_cpu_id() in addition to the bits we set.
            // includes this_cpu_id() in addition to the bits we set.
            assert!(raw.get(0));
            assert!(raw.get(2));
            assert!(raw.get(this_cpu_id().as_usize()));

            crate_interface::call_interface!(kipi::tlb::TaskCpuResidencyIf::reset_on_cpu_mask());
            let mask = task.on_cpu_mask();
            // reset_on_cpu_mask resets each thread's mask to the CPU it is
            // currently running on.
            assert!(mask.get(this_cpu_id().as_usize()));
            assert!(!mask.get(0) || this_cpu_id().as_usize() == 0);
            assert!(!mask.get(2) || this_cpu_id().as_usize() == 2);
        }
    }

    /// Test C: Proves flush_all_cpus sends IPIs to all online CPUs,
    /// waits for remote completion, and resets the mask.
    #[def_test(serial)]
    fn test_flush_all_targeted_shootdown() {
        let cpu_num = kbuild_config::CPU_NUM;
        if cpu_num >= 2 {
            let my_cpu = this_cpu_id();
            let remote_cpu = LogicalCpuId::new(if my_cpu == LogicalCpuId::new(0) { 1 } else { 0 });

            let task = ktask::current_may_uninit().unwrap();
            task.set_on_cpu_mask_bit(remote_cpu);

            // Call flush_all — if it returns, the remote CPU received the IPI,
            // called handle_shootdown() → karch::flush_tlb(), and set COMPLETED.
            kipi::tlb::trigger_flush_all(None);

            // Verify mask was reset to {my_cpu} only.
            let mask = task.on_cpu_mask();
            assert!(mask.get(my_cpu.as_usize()));
            assert!(!mask.get(remote_cpu.as_usize()));
        }
    }

    /// Test D: Proves context switch updates on_cpu_mask.
    #[def_test(serial)]
    fn test_context_switch_updates_on_cpu_mask() {
        let cpu_num = kbuild_config::CPU_NUM;
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

            let task = ktask::spawn_raw(
                move || {
                    TASK_CPU.store(khal::percpu::this_cpu_id().as_usize(), Ordering::Release);
                    if let Some(t) = ktask::current_may_uninit() {
                        TASK_MASK_BIT.store(t.on_cpu_mask().get(remote_cpu), Ordering::Release);
                    }
                },
                "test_ctx_mask".into(),
                0x2000,
            );
            task.set_cpumask(affinity);

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
    /// Single-VA flushes do **not** reset the residency mask (only full flushes
    /// do), so the remote CPU bit remains set.
    #[def_test(serial)]
    fn test_finish_triggers_cross_cpu_flush() {
        let cpu_num = kbuild_config::CPU_NUM;
        if cpu_num >= 2 {
            let my_cpu = this_cpu_id();
            let remote_cpu = LogicalCpuId::new(if my_cpu == LogicalCpuId::new(0) { 1 } else { 0 });

            let task = ktask::current_may_uninit().unwrap();
            task.set_on_cpu_mask_bit(remote_cpu);

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

            // If we reach here, finish() → flush_tlb_all_cpus completed.
            // Single-VA flush preserves the mask — only full flushes reset it.
            // The remote CPU was flushed for this specific VA, but the mask bit
            // stays so future shootdowns for other VAs will still target it.
            {
                let mask = task.on_cpu_mask();
                assert!(mask.get(my_cpu.as_usize()));
                // Remote CPU bit is still present (mask not reset on single-VA).
                assert!(mask.get(remote_cpu.as_usize()));
            }

            // Clean up: manually reset mask.
            task.reset_on_cpu_mask(my_cpu);
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
        let cpu_num = kbuild_config::CPU_NUM;
        if cpu_num >= 2 {
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

            // Pick a canonical-high virtual address unlikely to conflict.
            let test_vaddr = VirtAddr::from(0xFFFF_8000_7F00_0000usize);

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

            // Clean up the mask correctly for platforms (like aarch64) that don't reset
            // via software IPI — reset to the CPU the task is currently running on.
            task.reset_on_cpu_mask(my_cpu);
        }
    }

    /// Test G: Proves that kernel page table TLB shootdown reaches ALL
    /// online CPUs, regardless of the current task's CPU residency mask.
    ///
    /// Kernel page table modifications (is_kernel=true) go through
    /// `flush_all_cpus()`, which builds a full CPU mask via
    /// `for_each_present_logical_cpu()`.  Every online CPU — including
    /// those not in the current task's `on_cpu_mask` — receives a
    /// shootdown IPI.
    ///
    /// ### What this test verifies
    ///
    /// 1. **Correct shootdown**: after modifying the kernel page table, the
    ///    remote CPU (which is deliberately absent from the current task's
    ///    `on_cpu_mask`) still receives a TLB shootdown IPI and reads the
    ///    new mapping (MAGIC_B instead of stale MAGIC_A).
    ///
    /// 2. **Mask reset**: after a full flush (`None` vaddr), the task's
    ///    residency mask is correctly reset to only the current CPU.
    #[def_test(serial)]
    fn test_kernel_pt_miss_remote_without_mask() {
        let cpu_num = kbuild_config::CPU_NUM;
        if cpu_num >= 2 {
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

            let test_vaddr = VirtAddr::from(0xFFFF_8000_7F00_0000usize);

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

            task.reset_on_cpu_mask(my_cpu);
        }
    }
}

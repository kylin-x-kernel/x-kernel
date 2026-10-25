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

/// Number of present CPUs whose per-CPU run queue has been registered via
/// `ktask::init_scheduler_secondary()` (secondary cores) / `ktask::init_scheduler()`
/// (boot core, counted by the initial `1`).
///
/// The boot core enters `late_init_main` only after its own scheduler is up,
/// so it is sched-ready from the start. Each secondary core increments this
/// counter once its run queue is published, before any later bring-up step
/// (kipi / IRQ enable / softirqd / workqueue).
///
/// `start_secondary_cpus` waits on this counter so that the primary does not
/// enter driver initialization — which may spawn kernel threads whose default
/// cpumask spans every present CPU — until every secondary run queue is
/// registered and cross-CPU placement cannot land on an unregistered CPU.
static SCHED_READY_CPUS: AtomicUsize = AtomicUsize::new(1);

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

/// Start all secondary CPUs and wait until each has both entered the
/// runtime and registered its per-CPU run queue.
///
/// Waiting on run-queue registration (rather than on the AP merely having
/// entered the runtime, which happens before scheduler bring-up) guarantees
/// that cross-CPU task placement is safe before the caller proceeds to driver
/// initialization. Driver init spawns kernel threads whose default cpumask
/// spans every present CPU; without this barrier a round-robin selection
/// could land on a secondary that has entered but not yet registered its
/// run queue, panicking in `RUN_QUEUES.get().expect()`.
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: The CPU loop bound is generated from NR_CPUS and can be one \
              in a uniprocessor configuration"
)]
#[allow(
    clippy::absurd_extreme_comparisons,
    reason = "CG-024-lint-exceptions: clippy::absurd_extreme_comparisons: The CPU loop bound is \
              generated from NR_CPUS and can be one in a uniprocessor configuration"
)]
pub(crate) fn start_secondary_cpus(primary_cpu_id: LogicalCpuId) -> KResult {
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

        // The AP has entered the runtime, but its run queue is not registered
        // until `ktask::init_scheduler_secondary()` runs later in
        // `rust_main_secondary`. Wait for that step before booting the next
        // AP so the caller can safely assume every present CPU is schedulable
        // once this function returns.
        while SCHED_READY_CPUS.load(Ordering::Acquire) <= secondary_logical_cpu_id {
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

    info!("Secondary CPU {} started.", logical_cpu_id.as_usize());

    memspace::init_memory_management_secondary();

    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: AP controller preparation \
                  failed; this CPU must not activate sources or publish its run queue"
    )]
    khal::prepare_local_interrupts(logical_cpu_id).expect("secondary interrupt preparation failed");
    #[cfg(unittest)]
    super::tests_irq_startup::record_prepared();

    #[cfg(unittest)]
    super::tests_workqueue::deferred::prepare_local_boot_probe();
    ktask::init_scheduler_secondary();

    // Boot runs on idle: switching it out can postpone all remaining setup
    // indefinitely. IRQ masking alone does not stop lock-drop preemption.
    // Keep startup non-preemptible until every per-CPU service is prepared;
    // this region must not explicitly yield or wait for a runnable task.
    let startup_guard = kspin::NoPreempt::new();
    #[cfg(unittest)]
    super::tests_workqueue::deferred::prepare_local_boot_probe();

    // Publish that this CPU's run queue is registered so the primary's
    // `start_secondary_cpus` can stop waiting and enter driver initialization
    // knowing cross-CPU placement onto this CPU is safe.
    // This permits enqueueing, not synchronous execution or IPI acknowledgement.
    // Fixed notifications remain pending until local services are enabled below;
    // primary setup before INITED_CPUS publication must not wait for AP execution.
    SCHED_READY_CPUS.fetch_add(1, Ordering::Release);

    #[cfg(feature = "ipi")]
    kipi::init();

    // Only the primary publishes the first completion, after all AP run queues
    // exist and its own IPI/watchdog services are prepared.
    while super::INITED_CPUS.load(Ordering::Acquire) == 0 {
        core::hint::spin_loop();
    }

    // The primary has installed the global handlers. Pending notifications
    // may reprogram this CPU's clockevent, so activate before draining them.
    super::activate_local_interrupts();
    #[cfg(feature = "ipi")]
    kipi::enable_fixed_notifications();

    ktask::init_softirqd_current_cpu();
    super::init_local_irq_work();
    karch::enable_local_irq();

    #[cfg(feature = "watchdog")]
    watchdog::init_secondary();

    // Unmask the PMU line only after the watchdog armed this CPU's overflow
    // source (counter + NMI promotion), so no interrupt can be delivered
    // before the source and its handler are ready.  In pmu-only builds
    // (no NMI source) the watchdog block is compiled out and this runs
    // unchanged right before idle.
    #[cfg(feature = "pmu")]
    khal::pmu::enable_irq();

    #[cfg(unittest)]
    tests_startup_order::mark_complete(logical_cpu_id);

    info!("Secondary CPU {:x} init OK.", logical_cpu_id.as_usize());
    // Publish service completion rather than arrival at the rendezvous. Nothing
    // before this publication may wait for primary driver initialization.
    super::INITED_CPUS.fetch_add(1, Ordering::Release);

    // Pending requests are preserved and may take effect on this release.
    drop(startup_guard);
    ktask::run_idle();
}

#[kiface::provide]
impl SecondaryKernelEntry {
    fn enter(logical_cpu_id: LogicalCpuId) -> ! {
        rust_main_secondary(logical_cpu_id)
    }
}

unittest::path_mod_test!(
    pub(super) mod tests_startup_order,
    "../ut-tests/mp_startup_order.rs",
);

// ---------------------------------------------------------------------------
// Integration tests (kernel-mode, run on QEMU via `make run UNITTEST=y`)
// ---------------------------------------------------------------------------
unittest::path_mod_test!(
    #[cfg(feature = "smp")]
    mod tests_tlb_shootdown,
    "../ut-tests/mp_tlb_shootdown.rs",
);

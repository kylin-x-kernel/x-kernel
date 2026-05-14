// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! SMP bring-up helpers for the runtime.
use core::sync::atomic::{AtomicUsize, Ordering};

use kbuild_config::{CPU_NUM, TASK_STACK_SIZE};
use kernel_boot::{
    SECOND_KERNEL_ENTRY,
    arch::{LogicalCpuId, for_each_present_logical_cpu},
    register_boot_init,
};
use khal::mem::{VirtAddr, v2p};

#[unsafe(link_section = ".bss.stack")]
static mut SECONDARY_BOOT_STACK: [[u8; TASK_STACK_SIZE]; CPU_NUM - 1] =
    [[0; TASK_STACK_SIZE]; CPU_NUM - 1];

static ENTERED_CPUS: AtomicUsize = AtomicUsize::new(1);

/// Start all secondary CPUs and wait until they enter the runtime.
#[allow(clippy::absurd_extreme_comparisons)]
pub fn start_secondary_cpus(primary_cpu_id: LogicalCpuId) {
    let mut secondary_logical_cpu_id = 0;
    for_each_present_logical_cpu(|logical_cpu_id| {
        if logical_cpu_id == primary_cpu_id || secondary_logical_cpu_id >= CPU_NUM - 1 {
            return;
        }

        let stack_top = v2p(VirtAddr::from(unsafe {
            SECONDARY_BOOT_STACK[secondary_logical_cpu_id]
                .as_ptr_range()
                .end as usize
        }));

        kernel_boot::arch::set_secondary_boot_stack_top(logical_cpu_id, stack_top.as_usize());

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

    #[cfg(any(feature = "ipi", all(feature = "smp", feature = "crosvm")))]
    khal::irq::enable(kbuild_config::IPI_IRQ, true);

    #[cfg(feature = "pmu")]
    khal::irq::enable(kbuild_config::PMU_IRQ, true);

    karch::enable_local_irq();

    #[cfg(feature = "watchdog")]
    watchdog::init_secondary();

    ktask::run_idle();
}

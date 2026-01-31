// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! SMP bring-up helpers for the runtime.
use core::sync::atomic::{AtomicUsize, Ordering};

use khal::mem::{VirtAddr, v2p};
use platconfig::{TASK_STACK_SIZE, plat::CPU_NUM};

#[unsafe(link_section = ".bss.stack")]
static mut SECONDARY_BOOT_STACK: [[u8; TASK_STACK_SIZE]; CPU_NUM - 1] =
    [[0; TASK_STACK_SIZE]; CPU_NUM - 1];

static ENTERED_CPUS: AtomicUsize = AtomicUsize::new(1);

/// Start all secondary CPUs and wait until they enter the runtime.
#[allow(clippy::absurd_extreme_comparisons)]
pub fn start_secondary_cpus(primary_cpu_id: usize) {
    let mut logic_cpu_id = 0;
    for i in 0..CPU_NUM {
        if i != primary_cpu_id && logic_cpu_id < CPU_NUM - 1 {
            let stack_top = v2p(VirtAddr::from(unsafe {
                SECONDARY_BOOT_STACK[logic_cpu_id].as_ptr_range().end as usize
            }));

            debug!("starting CPU {i}...");
            khal::power::boot_ap(i, stack_top.as_usize());
            logic_cpu_id += 1;

            while ENTERED_CPUS.load(Ordering::Acquire) <= logic_cpu_id {
                core::hint::spin_loop();
            }
        }
    }
}

/// The main entry point of the runtime for secondary cores.
///
/// It is called from the bootstrapping code in the specific platform crate.
#[kplat::secondary_main]
pub fn rust_main_secondary(cpu_id: usize) -> ! {
    khal::percpu::init_secondary(cpu_id);
    khal::early_init_secondary(cpu_id);

    ENTERED_CPUS.fetch_add(1, Ordering::Release);
    info!("Secondary CPU {cpu_id} started.");

    #[cfg(feature = "paging")]
    memspace::init_memory_management_secondary();

    khal::final_init_secondary(cpu_id);

    ktask::init_scheduler_secondary();

    #[cfg(feature = "ipi")]
    kipi::init();

    info!("Secondary CPU {cpu_id:x} init OK.");
    super::INITED_CPUS.fetch_add(1, Ordering::Release);

    while !super::is_init_ok() {
        core::hint::spin_loop();
    }

    #[cfg(feature = "pmu")]
    khal::irq::enable(platconfig::devices::PMU_IRQ, true);

    khal::asm::enable_local();

    #[cfg(feature = "watchdog")]
    watchdog::init_secondary();

    ktask::run_idle();
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU-local platform helpers.

use kcpu_id_map::LogicalCpuId;

#[percpu::def_percpu]
static KPCB_ID: usize = 0;

#[percpu::def_percpu]
static KPCB_BSP: bool = false;

/// Returns the current CPU ID.
#[inline]
pub fn id() -> LogicalCpuId {
    LogicalCpuId::new(KPCB_ID.read_current())
}

/// Returns whether this CPU is the bootstrap processor (BSP).
#[inline]
pub fn is_bsp() -> bool {
    KPCB_BSP.read_current()
}

/// Initializes per-CPU state for the boot CPU.
pub fn boot_cpu_init(id: LogicalCpuId) {
    percpu::init_in_place().expect("failed to initialize per-CPU data areas");
    percpu::init_percpu_reg(id.as_usize());
    unsafe {
        KPCB_ID.write_current_raw(id.as_usize());
        KPCB_BSP.write_current_raw(true);
    }
}

#[cfg(feature = "smp")]
/// Initializes per-CPU state for an application processor (SMP only).
pub fn ap_cpu_init(id: LogicalCpuId) {
    percpu::init_percpu_reg(id.as_usize());
    unsafe {
        KPCB_ID.write_current_raw(id.as_usize());
        KPCB_BSP.write_current_raw(false);
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU-local platform helpers.

use kcpu_id_map::LogicalCpuId;
use kcpu_slot::cpu_slot_cell;

cpu_slot_cell! {
    /// Logical CPU id of the CPU whose area the base register selects.
    static KCPU_ID: usize = 0;
}

cpu_slot_cell! {
    /// Whether the CPU owning this area is the bootstrap processor.
    static KCPU_BSP: bool = false;
}

/// Returns the current CPU ID.
///
/// This sits underneath every guard's `current_cpu()`, so it reads through the
/// single-instruction current-CPU accessor instead of a guard-based one to stay
/// recursion-free.
#[inline]
pub fn id() -> LogicalCpuId {
    LogicalCpuId::new(KCPU_ID.load_current())
}

/// Returns whether this CPU is the bootstrap processor (BSP).
#[inline]
pub fn is_bsp() -> bool {
    KCPU_BSP.load_current()
}

/// Initializes per-CPU state for the boot CPU.
///
/// # Panics
/// Panics if this CPU's slot area cannot be initialized, e.g. because it is
/// already being brought up.
pub fn boot_cpu_init(id: LogicalCpuId) {
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: the BSP or AP CPU slot cannot \
                  initialize; CPU entry has no error return and cannot run without its slot"
    )]
    // SAFETY: boot CPU bring-up exclusively owns this CPU's unpublished area.
    let token = unsafe { kcpu_slot::begin_cpu_init(id) }
        .expect("failed to initialize the boot CPU slot area");
    KCPU_ID.write(&token, id.as_usize());
    KCPU_BSP.write(&token, true);
    token.publish();
}

#[cfg(feature = "smp")]
/// Initializes per-CPU state for an application processor (SMP only).
///
/// # Panics
/// Panics if this CPU's slot area cannot be initialized, e.g. because it is
/// already being brought up.
pub fn ap_cpu_init(id: LogicalCpuId) {
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: the BSP or AP CPU slot cannot \
                  initialize; CPU entry has no error return and cannot run without its slot"
    )]
    // SAFETY: AP bring-up exclusively owns this CPU's unpublished area.
    let token = unsafe { kcpu_slot::begin_cpu_init(id) }
        .expect("failed to initialize the AP CPU slot area");
    KCPU_ID.write(&token, id.as_usize());
    KCPU_BSP.write(&token, false);
    token.publish();
}

/// Supplies the current logical CPU to `kspin` guards, which forward it to
/// `kcpu_slot`'s `CpuPin` contract. This crate owns the per-CPU CPU-id
/// storage, so it is the single provider of the interface.
#[kiface::provide]
impl kiface_cpu_api::CurrentCpuIf {
    fn current_cpu() -> LogicalCpuId {
        id()
    }
}

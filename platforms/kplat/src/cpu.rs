//! CPU-local platform helpers.

#[percpu::def_percpu]
static KPCB_ID: usize = 0;

#[percpu::def_percpu]
static KPCB_BSP: bool = false;

/// Returns the current CPU ID.
#[inline]
pub fn id() -> usize {
    KPCB_ID.read_current()
}

/// Returns whether this CPU is the bootstrap processor (BSP).
#[inline]
pub fn is_bsp() -> bool {
    KPCB_BSP.read_current()
}

/// Initializes per-CPU state for the boot CPU.
pub fn boot_cpu_init(id: usize) {
    percpu::init();
    percpu::init_percpu_reg(id);
    unsafe {
        KPCB_ID.write_current_raw(id);
        KPCB_BSP.write_current_raw(true);
    }
}

#[cfg(feature = "smp")]
/// Initializes per-CPU state for an application processor (SMP only).
pub fn ap_cpu_init(id: usize) {
    percpu::init_percpu_reg(id);
    unsafe {
        KPCB_ID.write_current_raw(id);
        KPCB_BSP.write_current_raw(false);
    }
}

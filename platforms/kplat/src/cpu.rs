#[percpu::def_percpu]
static KPCB_ID: usize = 0;

#[percpu::def_percpu]
static KPCB_BSP: bool = false;

#[inline]
pub fn id() -> usize {
    KPCB_ID.read_current()
}

#[inline]
pub fn is_bsp() -> bool {
    KPCB_BSP.read_current()
}

pub fn boot_cpu_init(id: usize) {
    percpu::init();
    percpu::init_percpu_reg(id);
    unsafe {
        KPCB_ID.write_current_raw(id);
        KPCB_BSP.write_current_raw(true);
    }
}

#[cfg(feature = "smp")]
pub fn ap_cpu_init(id: usize) {
    percpu::init_percpu_reg(id);
    unsafe {
        KPCB_ID.write_current_raw(id);
        KPCB_BSP.write_current_raw(false);
    }
}

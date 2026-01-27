#![no_std]
use core::time::Duration;

pub use axerrno::AxResult;
pub use memaddr::{PhysAddr, VirtAddr};
use trait_ffi::*;

pub type IrqHandler = fn();

#[def_extern_trait]
pub trait Klib {
    fn mem_iomap(addr: PhysAddr, size: usize) -> AxResult<VirtAddr>;

    fn time_busy_wait(dur: Duration);

    fn irq_enable(irq: usize, enabled: bool);

    fn irq_register(irq: usize, handler: IrqHandler) -> bool;
}

pub mod mem {
    pub use super::klib::mem_iomap as iomap;
}

pub mod time {
    pub use super::klib::time_busy_wait as busy_wait;
}

pub mod irq {
    pub use super::klib::{irq_enable as enable, irq_register as register};
}

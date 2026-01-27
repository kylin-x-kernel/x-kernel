#![cfg_attr(not(test), no_std)]
#![cfg_attr(docsrs, feature(doc_cfg))]

extern crate kplat_macros;

pub mod boot;
pub mod cpu;
#[cfg(feature = "irq")]
pub mod interrupts;
pub mod io;
pub mod memory;
#[cfg(feature = "nmi")]
pub mod nm_irq;
#[cfg(feature = "pmu")]
pub mod perf;
pub mod psci;
pub mod sys;
pub mod timer;

pub use crate_interface::impl_interface as impl_dev_interface;
pub use kplat_macros::main;
#[cfg(feature = "smp")]
pub use kplat_macros::secondary_main;

#[doc(hidden)]
pub mod __priv {
    pub use const_str::equal as str_eq;
    pub use crate_interface::{call_interface as dispatch, def_interface as interface_def};
}

#[macro_export]
macro_rules! check_str_eq {
    ($l:expr, $r:expr, $msg:literal) => {
        const _: () = assert!($crate::__priv::str_eq!($l, $r), $msg);
    };
    ($l:expr, $r:expr $(,)?) => {
        const _: () = assert!($crate::__priv::str_eq!($l, $r), "String mismatch",);
    };
}

pub fn entry(id: usize, dtb: usize) -> ! {
    unsafe { __kplat_main(id, dtb) }
}

#[cfg(feature = "smp")]
pub fn entry_secondary(id: usize) -> ! {
    unsafe { __kplat_secondary_main(id) }
}

unsafe extern "Rust" {
    fn __kplat_main(id: usize, dtb: usize) -> !;
    fn __kplat_secondary_main(id: usize) -> !;
}

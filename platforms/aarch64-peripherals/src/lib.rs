//! AArch64 platform peripheral drivers and helpers.
#![no_std]
#[macro_use]
extern crate log;
pub mod generic_timer;
pub mod gic;
#[cfg(any(feature = "nmi-pmu", feature = "nmi-sdei"))]
pub mod nmi;
pub mod ns16550a;
pub mod pl011;
pub mod pl031;
#[cfg(feature = "pmu")]
pub mod pmu;
pub mod psci;

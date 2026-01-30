//! Generic page table abstractions and implementations.
#![cfg_attr(not(test), no_std)]

mod arch;
mod defs;
mod table64;

pub use arch::*;
pub use defs::*;
pub use table64::*;

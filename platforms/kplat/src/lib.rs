// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform abstraction layer interfaces and entry points.

#![cfg_attr(not(test), no_std)]
#![cfg_attr(docsrs, feature(doc_cfg))]

extern crate kplat_macros;

pub mod boot;
pub mod cpu;
pub mod dma;
pub mod mmio;
#[cfg(feature = "nmi")]
pub mod nm_irq;
#[cfg(feature = "pmu")]
pub mod perf;
pub mod sys;

pub use crate_interface::impl_interface as impl_dev_interface;
pub use kerrno;
pub use kplat_macros::{default_dma_if_impl, default_mmio_if_impl};

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

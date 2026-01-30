<<<<<<< HEAD
//! AArch64 platform peripheral drivers and helpers.
=======
// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

>>>>>>> 62a4f63a (./init, io, mm, net, platforms, process, sync over)
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

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! LoongArch64 platform glue: boot handoff, early device bring-up
//! (timer, EIOINTC/PCH-PIC interrupts, console, RTC), the platform
//! clock source, SMP boot, and power-off. Implements the `kplat`
//! platform contracts for the loongarch64 target.
//!
//! The crate is selected by the build (`loongarch64` target); the
//! kernel core invokes it through the generated `PlatformBootIf`,
//! `IntrManagerIf`, `ClockSourceIf`/`ClockEventIf`, and `CpuStartIf`/`PlatformPowerIf`
//! providers, so there is no direct API to call. The activation
//! sequence is: `early_driver_init` (clock, interrupts, console, RTC
//! sample) -> interrupt preparation -> `activate_local_interrupts` (per-CPU timer), with
//! `dispatch_irq` running from the IRQ entry afterwards. A production
//! bring-up trace is in `src/init.rs`.
//!
//! ```ignore
//! use kiface::provide;
//! use kplat::boot::PlatformBootIf;
//!
//! // The platform's early bring-up is expressed as a `PlatformBootIf`
//! // provider — the same shape used by `src/init.rs` (the crate's own
//! // implementation, which the kernel core invokes in this order:
//! // early_driver_init -> time/irq/console/RTC, then per-CPU timer).
//! #[provide]
//! impl PlatformBootIf {
//!     fn early_driver_init() {
//!         // Platform bring-up runs here, before the driver model
//!         // exists; when it returns, interrupts are dispatchable and
//!         // the wall clock is seeded from the LS7A RTC.
//!     }
//! }
//! ```
#![no_std]
#![cfg(target_arch = "loongarch64")]

#[macro_use]
extern crate log;
mod init;
mod irq;
#[cfg(feature = "smp")]
mod mp;
mod power;
mod time;

kplat::default_dma_if_impl!();
kplat::default_mmio_if_impl!();

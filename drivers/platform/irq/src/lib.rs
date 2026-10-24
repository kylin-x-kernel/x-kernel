// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform interrupt-controller backends behind the `kirq`
//! `IntrManagerIf` contract: GIC v2/v3 on AArch64, PLIC on RISC-V, and
//! the IO-APIC glue on x86_64. See `docs/design.md` for the selection
//! and dispatch model.
//!
//! The primary entry point is the architecture module selected at
//! compile time — [`gic`] on AArch64 (`gic::init_from_device_tree`,
//! `gic::init_current_cpu`), `riscv` on RISC-V. The backend is
//! registered with `kirq` through the generated `IntrManagerIf`
//! provider; platform init only needs the module's `init*` functions.
//! A production call site is `platforms/kplat-aarch64/src/init.rs`,
//! which runs `gic::init_from_device_tree` and `gic::init_current_cpu`
//! during platform bring-up.
//!
//! ```ignore
//! // Kernel code only: run once during platform bring-up.
//! use irq_driver::gic;
//!
//! gic::init_from_device_tree()?; // discovers/maps the GIC and initializes
//!                               // the primary CPU interface
//! // On each secondary CPU only:
//! // gic::init_current_cpu()?;
//!
//! // Effect: interrupt dispatch is initialized — device interrupts
//! // acknowledged here are now routed into kirq's generic handler, and
//! // this CPU can receive and complete GIC lines.
//! ```
#![no_std]
#[macro_use]
extern crate log;

#[cfg(target_arch = "aarch64")]
pub mod gic;
#[cfg(target_arch = "riscv64")]
pub mod riscv;
#[cfg(target_arch = "x86_64")]
pub mod x86;

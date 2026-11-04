// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! OS capability contracts required by the driver framework.
//!
//! Hosts implement operations; callers retain capability guards and arrange
//! their release order. This crate owns no device identity, probe state,
//! provider registry or kernel implementation. Device descriptions and managed
//! cleanup registration belong to the device framework.
#![no_std]

extern crate alloc;

mod dma;
mod error;
mod irq;
mod memory;
mod mmio;
mod time;

pub use dma::{
    CoherentArray, CoherentBox, CoherentMem, DmaAllocation, DmaDirection, DmaMapping, DmaOp,
    DmaSpec,
};
pub use error::{ResError, ResResult};
pub use irq::{
    DevIrq, IrqController, IrqDomainId, IrqEvent, IrqHandler, IrqHandlerToken, IrqOp, IrqResource,
    IrqRouteDesc, IrqThreadHandler, IrqTrigger, MsiMessage, MsiResource, Virq,
};
pub use memory::{CpuBuffer, CpuMemoryOp};
pub use mmio::{IoMap, IoPortRange, MmioMapping, MmioOp, MmioRegion};
pub use time::TimeOp;

unittest::path_mod_test!(tests, "../ut-tests/guards.rs");

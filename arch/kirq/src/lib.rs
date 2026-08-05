// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Generic kernel IRQ core.
//!
//! `kirq` owns the IRQ descriptor namespace, handler registration, hardirq
//! dispatch context, pseudo-NMI dispatch table, lifecycle hooks, and the
//! generic interrupt-controller interface. Architecture trap entry remains
//! outside this crate; HAL adapters call [`handle_irq`] and [`handle_nmi`] from
//! their exception handlers.

#![cfg_attr(not(test), no_std)]

#[macro_use]
extern crate log;
extern crate alloc;

pub mod context;
pub mod deferred;
mod desc;
mod dispatch;
mod domain;
pub mod lifecycle;
mod manager;
mod msi;
mod nmi;
mod platform;
pub mod softirq;
mod state;

pub use desc::{
    GIC_ROOT_DOMAIN, Hwirq, IO_APIC_DOMAIN, IntoIrqDesc, IrqAffinity, IrqController, IrqDesc,
    IrqDescError, IrqDomainId, IrqEvent, IrqFlags, IrqHandler, IrqPolarity, IrqSource, IrqTrigger,
    MSI_DOMAIN, PLIC_ROOT_DOMAIN, Virq, gic_edge_irq_desc, gic_irq_desc, gic_level_irq_desc,
    io_apic_irq_desc, plic_irq_desc,
};
pub use domain::IrqRef;
pub use manager::*;
pub use msi::{MsiAllocation, MsiKind, MsiMessage, alloc_msix, free_msix};
#[doc(hidden)]
pub use msi::{MsiBackendIf, MsiBackendToken};

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

mod backend;
mod bottom_half;
mod domain;
mod model;
mod runtime;

pub use backend::msi::{MsiAllocation, MsiKind, MsiMessage, alloc_msix, free_msix};
#[doc(hidden)]
pub use backend::msi::{MsiBackendIf, MsiBackendToken};
pub(crate) use backend::platform;
pub use bottom_half::{context, deferred, init_workerqueue, lifecycle, softirq};
pub use domain::IrqRef;
pub(crate) use model::desc;
pub use model::desc::{
    GIC_ROOT_DOMAIN, Hwirq, IO_APIC_DOMAIN, IRQ_EVENT_SOURCES, IrqAffinity, IrqController, IrqDesc,
    IrqDescError, IrqDomainId, IrqEvent, IrqEventSource, IrqFlags, IrqHandler, IrqPolarity,
    IrqSource, IrqSpec, IrqTrigger, MSI_DOMAIN, PLIC_ROOT_DOMAIN, Virq, gic_edge_irq_desc,
    gic_irq_desc, gic_level_irq_desc, io_apic_irq_desc, plic_irq_desc,
};
pub(crate) use runtime::{action, dispatch, nmi, state};
pub use runtime::{
    manager::*,
    notify::{register_irq_source_waker, register_irq_waker},
    sync_wait::IrqSyncWaitIf,
};

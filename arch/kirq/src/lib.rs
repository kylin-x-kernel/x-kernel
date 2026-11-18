// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Generic kernel IRQ core.
//!
//! `kirq` owns the IRQ descriptor namespace, handler registration, hardirq
//! dispatch context, Line-owned NMI actions, lifecycle hooks, and the
//! generic interrupt-controller interface. Architecture trap entry remains
//! outside this crate; HAL adapters call [`handle_irq`] and [`handle_nmi`] from
//! their exception handlers.
//!
//! # Responsibilities
//! Mapping gives a source a logical identity; registration installs a consumer;
//! dispatch borrows that consumer only for an admitted execution. These are
//! separate lifetimes: freeing an action does not dispose of its mapping, and
//! controller completion does not prove that the source has stopped producing.
//! Device and per-CPU source owners retain that shutdown responsibility.
//!
//! # Interfaces
//! - Drivers use [`try_map`], registration, enable/disable, and synchronous free.
//!   Checked operations report context and ownership errors instead of hiding them.
//! - [`irq_affinity`] / [`set_irq_affinity`] distinguish requested, pending and
//!   confirmed routing. Sources may expose an immediate or ACK/EOI-driven
//!   deferred controller capability; per-CPU, NMI and MSI lifecycles remain separate.
//! - [`lookup_irq_line`] / [`next_irq_line`] provide sparse, identity-retained
//!   Line handles for kernel control consumers. They do not create mappings or
//!   grant user authority; every operation revalidates the original Line.
//!   `irq_affinity` remains readable for protected Lines, while
//!   `check_user_affinity_change` and `set_user_affinity` enforce the
//!   action-owned user-routing restriction. Trusted `set_irq_affinity` remains
//!   policy-independent and is not user authorization.
//! - Architecture adapters enter through [`handle_irq`] / [`handle_nmi`]; irqchip
//!   providers implement [`IntrManagerIf`] and transfer claims using [`PendingIrq`].
//!   These entry functions and claim construction are unsafe: the adapter must
//!   prove captured-source provenance and retain initialized-CPU, IRQ and
//!   migration exclusion through the matching completion. A driver must not
//!   call them as a substitute for requesting an interrupt.
//!   [`generic_handle_irq`] and [`generic_handle_nmi`] safely consume an existing
//!   claim; they do not accept raw cookies or create hardware-entry provenance.
//! - [`softirq`] owns fixed-vector bottom halves; [`irq_work`] provides a distinct
//!   NMI-to-hardirq handoff. Neither is a sleepable worker implementation.
//!
//! # Internal ownership
//! `model` defines values, `domain` owns hardware-number indexes, `backend`
//! defines controller contracts, and `runtime` owns Line lifecycle transactions
//! and dispatch. `bottom_half` owns execution context and the IRQ-tail runner.
//! Internal modules are not alternative public entry points. The explicit
//! exports below keep implementation moves separate from API changes.

#![cfg_attr(not(test), no_std)]

#[macro_use]
extern crate log;
extern crate alloc;

mod backend;
mod bottom_half;
mod domain;
pub mod irq_work;
mod model;
mod runtime;

#[doc(hidden)]
pub use backend::msi::{MsiBackendIf, MsiBackendToken, MsiReservation};
pub(crate) use backend::platform;
pub use backend::{
    affinity::{
        IrqAffinityApplyError, IrqAffinityChip, IrqAffinityDelivery, IrqAffinityPrepare,
        IrqDeferredAffinityChip,
    },
    msi::{MsiAllocation, MsiKind, MsiMessage, alloc_msix, free_msix},
    platform::{
        EdgeChip, FasteoiChip, FasteoiResend, GuestIrqInterface, Handler, IntrManagerIf, IrqFlow,
        LevelChip, PendingIrq, PerCpuForwardingChip, PerCpuForwardingState, PerCpuNmiChip,
        TargetCpu, notify_cpu, set_prio,
    },
};
pub(crate) use bottom_half::deferred;
pub use bottom_half::{
    context, lifecycle,
    lifecycle::{
        IrqLifecycleHook, IrqLifecycleHooks, clear_irq_lifecycle_hooks,
        register_irq_lifecycle_hooks,
    },
    softirq,
};
pub use domain::{
    IrqRef,
    fw::{
        IrqDomainRegisterError, IrqDomainTarget, IrqDomainXlate, IrqHwSpec, IrqXlateError,
        alloc_irq_domain, register_irq_domain,
    },
};
pub(crate) use model::desc;
pub use model::{
    affinity::{IrqAffinityError, IrqAffinitySnapshot},
    desc::{
        GIC_ROOT_DOMAIN, Hwirq, IO_APIC_DOMAIN, IRQ_EVENT_SOURCES, IrqAffinity, IrqController,
        IrqDesc, IrqDescError, IrqDomainId, IrqEvent, IrqEventSource, IrqFlags, IrqHandler,
        IrqPolarity, IrqSource, IrqSpec, IrqTrigger, MSI_DOMAIN, PLIC_ROOT_DOMAIN, Virq,
        gic_edge_irq_desc, gic_irq_desc, gic_level_irq_desc, io_apic_irq_desc, plic_irq_desc,
    },
};
#[cfg(unittest)]
pub(crate) use runtime::dispatch;
#[cfg(unittest)]
pub use runtime::manager::tests_irq;
pub(crate) use runtime::{action, state};
pub use runtime::{
    action::IrqActionToken,
    affinity::{irq_affinity, set_irq_affinity},
    entry::{generic_handle_irq, generic_handle_nmi, handle_irq, handle_nmi},
    handle::{IrqLineHandle, lookup_irq_line, next_irq_line},
    manager::{
        FwIrqMapError, IrqRegisterOptions, MappedIrq, PerCpuForwarding, descriptor, disable_irq,
        disable_irq_nosync, enable, enable_irq, free_irq, free_irq_action,
        prepare_percpu_forwarding, register, register_disabled, register_shared, synchronize_irq,
        translate_hwirq, try_disable_irq, try_disable_irq_nosync, try_enable, try_enable_irq,
        try_free_irq, try_free_irq_action, try_free_percpu_irq, try_map, try_map_fwspec,
        try_register, try_register_disabled, try_register_percpu, try_register_percpu_startup,
        try_register_shared, try_register_startup, try_register_with_options, try_synchronize_irq,
        unregister,
    },
    nmi::{
        NmiShutdown, begin_nmi_shutdown, prepare_percpu_nmi, register_nmi, register_nmi_startup,
        teardown_percpu_nmi, unregister_nmi,
    },
    resend,
    retirement::{IrqLineRetirementObserver, IrqLineRetirementSubscription},
    state::DYNAMIC_VIRQ_BASE,
};

#[kiface::provide]
impl kiface_irq_context_api::InterruptContextIf {
    fn is_in_nmi() -> bool {
        context::is_in_nmi()
    }

    fn is_in_interrupt_context() -> bool {
        context::is_in_interrupt_context()
    }

    fn is_in_interrupt_handler() -> bool {
        matches!(
            context::interrupt_context_level(),
            context::InterruptContextLevel::Nmi
                | context::InterruptContextLevel::Hardirq
                | context::InterruptContextLevel::Softirq
        )
    }
}

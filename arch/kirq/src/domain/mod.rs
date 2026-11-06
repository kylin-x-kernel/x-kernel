// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Domain-owned RCU indexes. Entries retain the same Line as the logical directory.
//!
//! This module selects the static hardware-number namespace and its fallback
//! policy. Registry transactions publish into these indexes; domain lookup
//! neither installs actions nor grants permission to service a source. Firmware
//! parsing and concrete interrupt-controller setup are outside this module.

use alloc::sync::Arc;

use kspin::SpinNoIrq;
use kxarray::XArray;

pub(crate) mod fw;

use crate::{
    desc::{Hwirq, IrqDomainId, Virq},
    model::desc::IrqController,
    runtime::state::IrqLine,
};

/// Static hardware-number namespace retaining the registry's original Lines.
/// Numeric identity fallback is a lookup policy, not implicit flow binding.
///
/// The domain also owns the firmware-controller association registered for it.
/// `kind` is domain metadata, so the controller family used in mapped
/// descriptors has one authoritative source.
pub(crate) struct IrqDomain {
    pub(crate) lines: XArray<Arc<IrqLine>>,
    is_identity: bool,
    kind: IrqController,
    /// Set once during boot by `fw::register_irq_domain`.
    pub(crate) firmware: SpinNoIrq<Option<fw::DomainEntry>>,
}

impl IrqDomain {
    const fn new(is_identity: bool, kind: IrqController) -> Self {
        Self {
            lines: XArray::new(),
            is_identity,
            kind,
            firmware: SpinNoIrq::new(None),
        }
    }

    pub(crate) const fn kind(&self) -> IrqController {
        self.kind
    }

    pub(crate) fn resolve_unmapped(&self, hwirq: Hwirq) -> Option<Virq> {
        self.is_identity.then_some(hwirq)
    }

    fn resolve(&self, hwirq: Hwirq) -> Option<Virq> {
        let mapped = if kiface_irq_context_api::InterruptContextIf::is_in_interrupt_handler() {
            // NMI borrows only: it must not drop a potentially final Arc owner.
            krcu::with_interrupt_read(|scope| {
                self.lines.read(scope, hwirq).and_then(IrqLine::logical_irq)
            })
        } else {
            self.lines
                .get_owned_locked(hwirq)
                .and_then(|line| line.logical_irq())
        };
        mapped.or_else(|| self.resolve_unmapped(hwirq))
    }
}

pub(crate) static GIC_IRQ_DOMAIN: IrqDomain = IrqDomain::new(true, IrqController::Gic);
pub(crate) static PLIC_IRQ_DOMAIN: IrqDomain = IrqDomain::new(false, IrqController::Plic);
pub(crate) static IO_APIC_IRQ_DOMAIN: IrqDomain = IrqDomain::new(false, IrqController::IoApic);
pub(crate) static MSI_IRQ_DOMAIN: IrqDomain = IrqDomain::new(false, IrqController::Msi);
#[cfg(unittest)]
pub(crate) const TEST_DOMAIN: IrqDomainId = IrqDomainId::new(u32::MAX);
#[cfg(unittest)]
pub(crate) static TEST_IRQ_DOMAIN: IrqDomain = IrqDomain::new(false, IrqController::Gic);
// Namespace-isolation fixtures must not borrow a real controller's domain:
// its provider validates hardware bounds and may reject synthetic sources.
#[cfg(unittest)]
pub(crate) const SECOND_TEST_DOMAIN: IrqDomainId = IrqDomainId::new(u32::MAX - 1);
#[cfg(unittest)]
pub(crate) static SECOND_TEST_IRQ_DOMAIN: IrqDomain = IrqDomain::new(false, IrqController::Plic);

pub(crate) const DOMAIN_COUNT: usize = if cfg!(unittest) { 6 } else { 4 };

/// Every statically available domain, in slot order.
///
/// Firmware association scans this list for duplicate-controller detection and
/// lookup. It is the complete domain set, so no separate association table or
/// capacity rule exists.
#[cfg(unittest)]
pub(crate) const DOMAIN_IDS: [IrqDomainId; DOMAIN_COUNT] = [
    crate::desc::GIC_ROOT_DOMAIN,
    crate::desc::PLIC_ROOT_DOMAIN,
    crate::desc::IO_APIC_DOMAIN,
    crate::desc::MSI_DOMAIN,
    TEST_DOMAIN,
    SECOND_TEST_DOMAIN,
];
#[cfg(not(unittest))]
pub(crate) const DOMAIN_IDS: [IrqDomainId; DOMAIN_COUNT] = [
    crate::desc::GIC_ROOT_DOMAIN,
    crate::desc::PLIC_ROOT_DOMAIN,
    crate::desc::IO_APIC_DOMAIN,
    crate::desc::MSI_DOMAIN,
];

pub(crate) fn slot(id: IrqDomainId) -> Option<usize> {
    match id {
        crate::desc::GIC_ROOT_DOMAIN => Some(0),
        crate::desc::PLIC_ROOT_DOMAIN => Some(1),
        crate::desc::IO_APIC_DOMAIN => Some(2),
        crate::desc::MSI_DOMAIN => Some(3),
        #[cfg(unittest)]
        TEST_DOMAIN => Some(4),
        #[cfg(unittest)]
        SECOND_TEST_DOMAIN => Some(5),
        _ => None,
    }
}

pub(crate) fn domain(id: IrqDomainId) -> Option<&'static IrqDomain> {
    match id {
        crate::desc::GIC_ROOT_DOMAIN => Some(&GIC_IRQ_DOMAIN),
        crate::desc::PLIC_ROOT_DOMAIN => Some(&PLIC_IRQ_DOMAIN),
        crate::desc::IO_APIC_DOMAIN => Some(&IO_APIC_IRQ_DOMAIN),
        crate::desc::MSI_DOMAIN => Some(&MSI_IRQ_DOMAIN),
        #[cfg(unittest)]
        TEST_DOMAIN => Some(&TEST_IRQ_DOMAIN),
        #[cfg(unittest)]
        SECOND_TEST_DOMAIN => Some(&SECOND_TEST_IRQ_DOMAIN),
        _ => None,
    }
}

/// Numeric observation grants no service admission or retained ownership.
pub(crate) fn resolve(id: IrqDomainId, hwirq: Hwirq) -> Option<Virq> {
    domain(id)?.resolve(hwirq)
}

/// The namespace of a controller claim; completion always uses its original cookie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqRef {
    /// Domain-local hardware number, resolved through an RCU-protected Line index.
    Domain(IrqDomainId, Hwirq),
    /// Explicit logical identity (for example a LAPIC timer or RISC-V local IRQ).
    Virq(Virq),
}

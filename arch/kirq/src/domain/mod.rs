// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Domain-owned RCU indexes. Entries retain the same Line as the logical directory.

use alloc::sync::Arc;

use kxarray::XArray;

use crate::{
    desc::{Hwirq, IrqDomainId, Virq},
    runtime::state::IrqLine,
};

pub(crate) struct IrqDomain {
    pub(crate) lines: XArray<Arc<IrqLine>>,
    is_identity: bool,
}

impl IrqDomain {
    const fn new(is_identity: bool) -> Self {
        Self {
            lines: XArray::new(),
            is_identity,
        }
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

pub(crate) static GIC_IRQ_DOMAIN: IrqDomain = IrqDomain::new(true);
pub(crate) static PLIC_IRQ_DOMAIN: IrqDomain = IrqDomain::new(false);
pub(crate) static IO_APIC_IRQ_DOMAIN: IrqDomain = IrqDomain::new(false);
pub(crate) static MSI_IRQ_DOMAIN: IrqDomain = IrqDomain::new(false);
#[cfg(unittest)]
pub(crate) const TEST_DOMAIN: IrqDomainId = IrqDomainId::new(u32::MAX);
#[cfg(unittest)]
pub(crate) static TEST_IRQ_DOMAIN: IrqDomain = IrqDomain::new(false);
// Namespace-isolation fixtures must not borrow a real controller's domain:
// its provider validates hardware bounds and may reject synthetic sources.
#[cfg(unittest)]
pub(crate) const SECOND_TEST_DOMAIN: IrqDomainId = IrqDomainId::new(u32::MAX - 1);
#[cfg(unittest)]
pub(crate) static SECOND_TEST_IRQ_DOMAIN: IrqDomain = IrqDomain::new(false);

pub(crate) const DOMAIN_COUNT: usize = if cfg!(unittest) { 6 } else { 4 };

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

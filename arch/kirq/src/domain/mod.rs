// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Domain-owned RCU indexes. Entries retain the same Line as the logical directory.
//!
//! This module owns the registered hardware-number namespaces, their fallback
//! policy and the append-only domain registry. Registry transactions publish
//! into these indexes; domain lookup neither installs actions nor grants
//! permission to service a source. Firmware parsing and concrete
//! interrupt-controller setup are outside this module.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use kalloc_types::{AllocFlags, KBox};
use kspin::SpinNoIrq;
use kxarray::XArray;

pub(crate) mod fw;

use crate::{
    desc::{Hwirq, IrqDomainId, Virq},
    model::desc::IrqController,
    runtime::state::IrqLine,
};

/// Controller-local hardware-number namespace retaining its own Lines.
/// Numeric identity fallback is a lookup policy, not implicit flow binding.
///
/// The domain also owns the firmware-controller association registered for it.
/// `kind` is domain metadata, so the controller family used in mapped
/// descriptors has one authoritative source. Domains are allocated at boot and
/// live for the kernel lifetime; they are never unregistered or freed.
pub(crate) struct IrqDomain {
    id: IrqDomainId,
    pub(crate) lines: XArray<Arc<IrqLine>>,
    is_identity: bool,
    kind: IrqController,
    /// Set once during boot by `fw::register_irq_domain`, or left empty for a
    /// controller instance that has no firmware namespace node.
    pub(crate) firmware: SpinNoIrq<Option<fw::DomainEntry>>,
    /// Next domain in the append-only registry. Written once before the domain
    /// is published to readers.
    next: AtomicPtr<IrqDomain>,
}

impl IrqDomain {
    const fn new(id: IrqDomainId, is_identity: bool, kind: IrqController) -> Self {
        Self {
            id,
            lines: XArray::new(),
            is_identity,
            kind,
            firmware: SpinNoIrq::new(None),
            next: AtomicPtr::new(core::ptr::null_mut()),
        }
    }

    pub(crate) const fn id(&self) -> IrqDomainId {
        self.id
    }

    pub(crate) const fn kind(&self) -> IrqController {
        self.kind
    }

    #[cfg(unittest)]
    pub(crate) const fn is_identity(&self) -> bool {
        self.is_identity
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

// Well-known domains remain available before any firmware association so
// platform-static and legacy consumers keep stable identities. Boot wiring may
// explicitly associate a primary controller with one of these roots; additional
// instances receive dynamically allocated domains. Domains without a firmware
// registration path (IO-APIC, MSI) and test fixtures stay static.
pub(crate) static GIC_IRQ_DOMAIN: IrqDomain =
    IrqDomain::new(crate::desc::GIC_ROOT_DOMAIN, true, IrqController::Gic);
pub(crate) static PLIC_IRQ_DOMAIN: IrqDomain =
    IrqDomain::new(crate::desc::PLIC_ROOT_DOMAIN, false, IrqController::Plic);
pub(crate) static IO_APIC_IRQ_DOMAIN: IrqDomain =
    IrqDomain::new(crate::desc::IO_APIC_DOMAIN, false, IrqController::IoApic);
pub(crate) static MSI_IRQ_DOMAIN: IrqDomain =
    IrqDomain::new(crate::desc::MSI_DOMAIN, false, IrqController::Msi);
#[cfg(unittest)]
pub(crate) const TEST_DOMAIN: IrqDomainId = IrqDomainId::new(u32::MAX);
#[cfg(unittest)]
pub(crate) static TEST_IRQ_DOMAIN: IrqDomain =
    IrqDomain::new(TEST_DOMAIN, false, IrqController::Gic);
// Namespace-isolation fixtures must not borrow a real controller's domain:
// its provider validates hardware bounds and may reject synthetic sources.
#[cfg(unittest)]
pub(crate) const SECOND_TEST_DOMAIN: IrqDomainId = IrqDomainId::new(u32::MAX - 1);
#[cfg(unittest)]
pub(crate) static SECOND_TEST_IRQ_DOMAIN: IrqDomain =
    IrqDomain::new(SECOND_TEST_DOMAIN, false, IrqController::Plic);

/// First identifier used for domains allocated after the well-known set.
const DYNAMIC_DOMAIN_ID_BASE: u32 = 5;

#[cfg(unittest)]
pub(crate) const DOMAIN_COUNT: usize = 6;
#[cfg(not(unittest))]
pub(crate) const DOMAIN_COUNT: usize = 4;

#[cfg(unittest)]
static STATIC_DOMAINS: [&IrqDomain; DOMAIN_COUNT] = [
    &GIC_IRQ_DOMAIN,
    &PLIC_IRQ_DOMAIN,
    &IO_APIC_IRQ_DOMAIN,
    &MSI_IRQ_DOMAIN,
    &TEST_IRQ_DOMAIN,
    &SECOND_TEST_IRQ_DOMAIN,
];
#[cfg(not(unittest))]
static STATIC_DOMAINS: [&IrqDomain; DOMAIN_COUNT] = [
    &GIC_IRQ_DOMAIN,
    &PLIC_IRQ_DOMAIN,
    &IO_APIC_IRQ_DOMAIN,
    &MSI_IRQ_DOMAIN,
];

/// Head of the append-only list of domains allocated after the static set.
///
/// Readers traverse atomically published links without a lock or RCU scope.
/// Domains are never removed, so a link observed here keeps referring to the
/// same live domain. Writers serialize through `fw::FW_REGISTRATION`.
static DOMAIN_HEAD: AtomicPtr<IrqDomain> = AtomicPtr::new(core::ptr::null_mut());
static NEXT_DOMAIN_ID: AtomicU32 = AtomicU32::new(DYNAMIC_DOMAIN_ID_BASE);

/// Allocates and publishes a domain already associated with its controller.
///
/// Requires the caller to serialize with other registration writers. A
/// firmware association, when supplied, is installed before the domain becomes
/// reachable, so a reader that observes the new domain also observes its
/// binding. Controller instances without a firmware namespace node pass
/// `None`; they remain pure hardware-number namespaces. The new domain is
/// leaked for the kernel lifetime; `None` means its storage could not be
/// reserved. A failed attempt may consume an identifier.
///
/// Allocated domains require explicit mappings. Numeric identity fallback is a
/// compatibility policy of a reserved well-known root and is never inherited by
/// an additional controller instance; an unmapped hardware number must not
/// resolve to an unrelated global logical number.
pub(super) fn create_domain(
    kind: IrqController,
    entry: Option<fw::DomainEntry>,
) -> Option<&'static IrqDomain> {
    let id = IrqDomainId::new(NEXT_DOMAIN_ID.fetch_add(1, Ordering::Relaxed));
    let domain = KBox::new_with_flags(IrqDomain::new(id, false, kind), AllocFlags::NOWAIT).ok()?;
    *domain.firmware.lock() = entry;
    let pointer = KBox::into_raw(domain);
    // SAFETY: `into_raw` transfers the sole owner of a live domain. The domain
    // is published below and intentionally never reclaimed, so the reference
    // stays valid for the kernel lifetime and no mutable access exists after
    // publication.
    let domain: &'static IrqDomain = unsafe { &*pointer };
    // The registration lock makes this the only writer. Release publishes the
    // initialized fields before a reader can acquire the new head.
    domain
        .next
        .store(DOMAIN_HEAD.load(Ordering::Relaxed), Ordering::Relaxed);
    DOMAIN_HEAD.store(pointer, Ordering::Release);
    Some(domain)
}

/// Every registered domain, static then dynamically allocated.
///
/// Traversal takes no lock and only reads immutable fields plus atomically
/// published links. Visitors may take their own control-plane locks, so this
/// enumeration is used for boot registration and mapping lookup; interrupt
/// delivery resolves one domain by identifier through [`domain`] instead.
pub(crate) fn for_each_domain(mut visit: impl FnMut(&'static IrqDomain) -> bool) {
    for domain in STATIC_DOMAINS {
        if !visit(domain) {
            return;
        }
    }
    let mut pointer = DOMAIN_HEAD.load(Ordering::Acquire);
    while !pointer.is_null() {
        // SAFETY: published domains are leaked and never mutated after
        // publication; `next` is written once before the release store above.
        let domain = unsafe { &*pointer };
        if !visit(domain) {
            return;
        }
        pointer = domain.next.load(Ordering::Acquire);
    }
}

#[cfg(unittest)]
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
    for domain in STATIC_DOMAINS {
        if domain.id == id {
            return Some(domain);
        }
    }
    let mut pointer = DOMAIN_HEAD.load(Ordering::Acquire);
    while !pointer.is_null() {
        // SAFETY: published domains are leaked and immutable after publication,
        // so both the pointee and its `next` link remain valid for the read.
        let domain = unsafe { &*pointer };
        if domain.id == id {
            return Some(domain);
        }
        pointer = domain.next.load(Ordering::Acquire);
    }
    None
}

/// Returns the controller family metadata of a registered domain.
///
/// Internal query only. This is domain metadata, not proof that the caller may
/// operate a particular controller instance; admission must use the domain
/// identifier registered by the instance's own boot wiring.
#[cfg(unittest)]
pub(crate) fn domain_kind(id: IrqDomainId) -> Option<IrqController> {
    domain(id).map(IrqDomain::kind)
}

/// Numeric observation grants no service admission or retained ownership.
pub(crate) fn resolve(id: IrqDomainId, hwirq: Hwirq) -> Option<Virq> {
    domain(id)?.resolve(hwirq)
}

/// The namespace of a controller claim; completion always uses its original cookie.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrqRef {
    /// Domain-local hardware number, resolved through an RCU-protected Line index.
    Domain(IrqDomainId, Hwirq),
    /// Explicit logical identity (for example a LAPIC timer or RISC-V local IRQ).
    Virq(Virq),
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Firmware-controller to IRQ-domain association.
//!
//! Firmware describes an interrupt with a controller node and binding cells.
//! The controller driver owns the meaning of those cells through
//! [`IrqDomainXlate`]; this module owns the association between the exact
//! firmware node and the kernel IRQ domain the driver registered for it.
//! Association is by node identity, not by compatible string or controller
//! family, so two instances of the same family remain distinguishable.
//!
//! Translation and mapping are intentionally separate: a lookup here yields
//! only a copied binding, and the domain's association lock is never held
//! while the controller translates or while the IRQ core maps.
//!
//! Registration associates one controller instance with one domain. The boot
//! wiring chooses explicitly whether that instance reuses a well-known domain
//! (so platform-static consumers keep stable identities) or receives a newly
//! allocated domain; this module does not infer that choice from controller
//! family or registration order. Two same-family controllers therefore keep
//! independent hardware-number namespaces.
//!
//! A well-known domain keeps its own identity-fallback policy. A newly
//! allocated domain is never identity-mapped: unmapped hardware numbers stay
//! unresolved instead of borrowing an unrelated global logical number.

use fwnode::{FwNode, FwNodeId};
use kspin::{SpinNoIrq, static_lock};

use crate::{Hwirq, IrqController, IrqDomainId, IrqTrigger};

/// Hardware identity produced by one controller's binding translation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IrqHwSpec {
    /// Controller-local hardware interrupt number.
    pub hwirq: Hwirq,
    /// Trigger mode described by the firmware binding.
    pub trigger: IrqTrigger,
}

/// A controller binding rejected the firmware cells.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrqXlateError {
    /// The cell list does not match the binding's required shape or values.
    Malformed,
    /// The binding is valid, but this controller cannot implement it.
    Unsupported,
    /// The decoded hardware number is outside the controller's range.
    InvalidHwirq,
}

/// Controller-owned interpretation of its firmware binding cells.
///
/// Implementations must be pure with respect to kernel IRQ state: they decode
/// cells into a hardware number and trigger attributes, and must not allocate
/// logical IRQs, select another controller, or call back into `kirq`. They may
/// run in task context while another caller maps a different line.
pub trait IrqDomainXlate: Send + Sync {
    /// Translates raw binding cells in firmware order.
    ///
    /// # Errors
    ///
    /// Returns [`IrqXlateError::Malformed`] for a cell list that does not match
    /// the binding, [`IrqXlateError::Unsupported`] for a valid but unsupported
    /// encoding, or [`IrqXlateError::InvalidHwirq`] when the decoded number is
    /// outside the controller's range.
    fn translate(&self, params: &[u32]) -> Result<IrqHwSpec, IrqXlateError>;
}

/// Failure to associate a firmware controller node with an IRQ domain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrqDomainRegisterError {
    /// The selected existing domain is not a kernel IRQ domain.
    UnknownDomain { domain: IrqDomainId },
    /// The selected existing domain already has a firmware controller.
    DomainAlreadyRegistered { domain: IrqDomainId },
    /// This controller node is already associated with a domain.
    ControllerAlreadyRegistered {
        controller: FwNodeId,
        existing: IrqDomainId,
    },
    /// Storage for a new per-instance domain could not be reserved.
    NoMemory,
}

/// Association stored inside the IRQ domain it belongs to.
#[derive(Clone, Copy)]
pub(crate) struct DomainEntry {
    pub(crate) controller: FwNode<'static>,
    pub(crate) xlate: &'static dyn IrqDomainXlate,
}

/// A copied association returned by lookup.
#[derive(Clone, Copy)]
pub(crate) struct DomainBinding {
    pub(crate) domain: IrqDomainId,
    pub(crate) kind: IrqController,
    pub(crate) xlate: &'static dyn IrqDomainXlate,
}

static_lock! {
    /// Serializes boot-time registration; the associations themselves live in
    /// their domains.
    static FW_REGISTRATION: SpinNoIrq<()> = SpinNoIrq::new(());
}

/// Where boot wiring places a firmware controller's IRQ domain.
///
/// The wiring chooses this explicitly. The generic registration path does not
/// infer root reuse from controller family, identity policy or registration
/// order; those are compatibility decisions of the platform bring-up.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrqDomainTarget {
    /// Associate with this existing well-known domain.
    ///
    /// The domain keeps its own controller metadata and identity policy. It
    /// must exist and must not already have a firmware association.
    Existing(IrqDomainId),
    /// Allocate a new domain for this controller instance.
    ///
    /// The supplied controller family becomes the new domain's metadata. A new
    /// domain always requires explicit mappings and never inherits numeric
    /// identity fallback. This target requires an initialized allocator and
    /// performs a non-sleeping `NOWAIT` allocation, which may fail with
    /// [`IrqDomainRegisterError::NoMemory`].
    New(IrqController),
}

/// Associates a firmware controller node with the selected IRQ domain.
///
/// `target` is the boot wiring's explicit choice: reuse one well-known domain
/// or allocate a per-instance domain. Same-family controllers must not share a
/// hardware-number namespace; using `New` for every additional instance keeps
/// their mappings independent.
///
/// The returned identifier belongs to the caller's controller instance and
/// must be used for dispatch on that instance. Controller family metadata
/// (`IrqDomain::kind`) does not prove instance ownership.
///
/// The controller node must remain valid for the kernel lifetime; the platform
/// publishes the firmware backend before controllers register.
///
/// Registration is a boot-time operation and is not idempotent: a duplicate
/// controller association is an explicit error so a second registration cannot
/// silently take over a mapping. Domains are never unregistered.
///
/// # Execution context
///
/// Control-plane boot wiring only. Callers must not invoke this from an IRQ,
/// NMI or bottom-half handler; registration serializes through a non-sleeping
/// lock. `New` additionally requires an initialized allocator and performs a
/// `NOWAIT` allocation: it never sleeps, but it can fail with
/// [`IrqDomainRegisterError::NoMemory`]. `Existing` allocates nothing.
///
/// # Errors
///
/// Returns [`IrqDomainRegisterError::UnknownDomain`] or
/// [`IrqDomainRegisterError::DomainAlreadyRegistered`] for an invalid
/// `Existing` target, [`IrqDomainRegisterError::ControllerAlreadyRegistered`]
/// when the exact controller node is already associated, or
/// [`IrqDomainRegisterError::NoMemory`] when a new domain cannot be allocated.
pub fn register_irq_domain(
    target: IrqDomainTarget,
    controller: FwNode<'static>,
    xlate: &'static dyn IrqDomainXlate,
) -> Result<IrqDomainId, IrqDomainRegisterError> {
    let _registration = FW_REGISTRATION.lock();
    let mut duplicate = None;
    super::for_each_domain(|candidate| {
        let entry = *candidate.firmware.lock();
        if entry.is_some_and(|entry| entry.controller == controller) {
            duplicate = Some(candidate.id());
            false
        } else {
            true
        }
    });
    if let Some(existing) = duplicate {
        return Err(IrqDomainRegisterError::ControllerAlreadyRegistered {
            controller: controller.id(),
            existing,
        });
    }

    let entry = DomainEntry { controller, xlate };
    match target {
        IrqDomainTarget::Existing(id) => {
            let domain =
                super::domain(id).ok_or(IrqDomainRegisterError::UnknownDomain { domain: id })?;
            let mut firmware = domain.firmware.lock();
            if firmware.is_some() {
                return Err(IrqDomainRegisterError::DomainAlreadyRegistered { domain: id });
            }
            *firmware = Some(entry);
            Ok(id)
        }
        IrqDomainTarget::New(kind) => super::create_domain(kind, Some(entry))
            .map(super::IrqDomain::id)
            .ok_or(IrqDomainRegisterError::NoMemory),
    }
}

/// Allocates a boot-lifetime IRQ domain for a controller instance that has no
/// firmware namespace node.
///
/// Some controllers (for example additional x86 IO-APICs) are described by
/// firmware records rather than a firmware object model, so [`register_irq_domain`]
/// cannot be used without fabricating a node. This path allocates the same
/// kind of non-identity per-instance namespace without any firmware
/// association. Mapping and dispatch still use the returned identifier as the
/// instance's domain; controller family metadata alone never proves ownership.
///
/// Registration and allocation serialize through the same boot-time lock, so a
/// domain is reachable with its controller family fully initialized. The
/// domain is never unregistered or freed.
///
/// # Execution context
///
/// Control-plane boot wiring only. Callers must not invoke this from an IRQ,
/// NMI or bottom-half handler; allocation uses a non-sleeping lock and a
/// `NOWAIT` allocation, so it never sleeps but can fail.
///
/// Returns `None` when the allocator is not ready or storage cannot be
/// reserved. A failed attempt may consume an identifier.
pub fn alloc_irq_domain(kind: IrqController) -> Option<IrqDomainId> {
    let _registration = FW_REGISTRATION.lock();
    super::create_domain(kind, None).map(super::IrqDomain::id)
}

/// Returns the binding registered for this exact controller node.
///
/// The caller must invoke the returned translator after this function returns;
/// no association lock may span translation or mapping.
pub(crate) fn lookup(controller: FwNode<'static>) -> Option<DomainBinding> {
    let mut binding = None;
    super::for_each_domain(|irq_domain| {
        let entry = *irq_domain.firmware.lock();
        if let Some(entry) = entry.filter(|entry| entry.controller == controller) {
            binding = Some(DomainBinding {
                domain: irq_domain.id(),
                kind: irq_domain.kind(),
                xlate: entry.xlate,
            });
            false
        } else {
            true
        }
    });
    binding
}

unittest::path_mod_test!(tests_fw_domain, "../../ut-tests/fw_domain.rs");

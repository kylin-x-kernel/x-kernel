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
//! Phase one exposes one static domain per controller family. Exact node
//! association keeps controllers from being confused with one another, but
//! registering two instances of the same family would require a per-instance
//! domain; the current static domain set does not support that yet.

use fwnode::{FwNode, FwNodeId};
use kspin::{SpinNoIrq, static_lock};

use crate::{Hwirq, IrqController, IrqDomainId, IrqTrigger};

/// Hardware identity produced by one controller's binding translation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IrqHwSpec {
    /// Controller-local hardware interrupt number.
    pub hwirq: Hwirq,
    /// Trigger mode described by the firmware binding.
    pub trigger: IrqTrigger,
}

/// A controller binding rejected the firmware cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqDomainRegisterError {
    /// The domain identifier is not a kernel IRQ domain.
    UnknownDomain { domain: IrqDomainId },
    /// Another controller node is already associated with this domain.
    DomainAlreadyRegistered { domain: IrqDomainId },
    /// This controller node is already associated with a domain.
    ControllerAlreadyRegistered {
        controller: FwNodeId,
        existing: IrqDomainId,
    },
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

/// Associates a firmware controller node with an IRQ domain.
///
/// The association is stored in the target domain; the controller kind comes
/// from that domain's metadata, so callers cannot register a contradictory
/// domain/controller-family pair.
///
/// The controller node must remain valid for the kernel lifetime; the platform
/// publishes the firmware backend before controllers register. Registration is
/// a boot-time operation and is not idempotent: a duplicate association is an
/// explicit error so a second controller cannot silently take over a mapping.
/// A device with two instances of one controller family needs a per-instance
/// domain, which the current static domain set does not provide.
///
/// # Errors
///
/// Returns [`IrqDomainRegisterError::UnknownDomain`] when `domain` is not a
/// kernel IRQ domain, or a duplicate-association error when either side is
/// already registered.
pub fn register_irq_domain(
    domain: IrqDomainId,
    controller: FwNode<'static>,
    xlate: &'static dyn IrqDomainXlate,
) -> Result<(), IrqDomainRegisterError> {
    let target = super::domain(domain).ok_or(IrqDomainRegisterError::UnknownDomain { domain })?;
    let _registration = FW_REGISTRATION.lock();
    if target.firmware.lock().is_some() {
        return Err(IrqDomainRegisterError::DomainAlreadyRegistered { domain });
    }
    for existing in super::DOMAIN_IDS {
        let Some(candidate) = super::domain(existing) else {
            continue;
        };
        let entry = *candidate.firmware.lock();
        if entry.is_some_and(|entry| entry.controller == controller) {
            return Err(IrqDomainRegisterError::ControllerAlreadyRegistered {
                controller: controller.id(),
                existing,
            });
        }
    }
    *target.firmware.lock() = Some(DomainEntry { controller, xlate });
    Ok(())
}

/// Returns the binding registered for this exact controller node.
///
/// The caller must invoke the returned translator after this function returns;
/// no association lock may span translation or mapping.
pub(crate) fn lookup(controller: FwNode<'static>) -> Option<DomainBinding> {
    for domain in super::DOMAIN_IDS {
        let Some(irq_domain) = super::domain(domain) else {
            continue;
        };
        let entry = *irq_domain.firmware.lock();
        if let Some(entry) = entry.filter(|entry| entry.controller == controller) {
            return Some(DomainBinding {
                domain,
                kind: irq_domain.kind(),
                xlate: entry.xlate,
            });
        }
    }
    None
}

unittest::path_mod_test!(tests_fw_domain, "../../ut-tests/fw_domain.rs");

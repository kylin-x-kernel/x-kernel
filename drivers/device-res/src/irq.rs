// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IRQ resource types: trigger/controller/domain/resource description, the
//! [`IrqEvent`] outcome, the [`IrqHandler`] / [`IrqOp`] capability traits, and
//! the [`Irq`] RAII handle.

use alloc::sync::Arc;

use crate::{
    ResError, ResResult,
    provider::{irq_provider, try_irq_provider},
};

/// Interrupt trigger mode.
///
/// This is intentionally OS-neutral. Host kernels convert their own trigger
/// representation into this enum at discovery time. `Unknown` carries the raw
/// flag bits the host preserved (0 when truly unknown) so downstream layers
/// never lose firmware-described trigger information.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqTrigger {
    EdgeRising,
    EdgeFalling,
    LevelHigh,
    LevelLow,
    /// Trigger mode not described by firmware; carries raw flag bits (0 if none).
    Unknown(u32),
}

/// The interrupt controller family that owns an IRQ line.
///
/// OS-neutral: host kernels translate their own controller representation
/// into this enum when describing a discovered interrupt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqController {
    Gic,
    Plic,
    IoApic,
    LoongArchExtioi,
    /// Controller not described by firmware / unknown.
    Unknown,
}

/// Opaque identifier for an interrupt domain.
///
/// Host kernels that partition interrupt numbers into domains (e.g. separate
/// GIC / PLIC / IO-APIC number spaces) use this to disambiguate. The raw value
/// is meaningless outside the host that minted it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct IrqDomainId(pub u32);

/// An interrupt resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IrqResource {
    /// IRQ number visible to the OS (virtual IRQ after domain translation).
    pub number: usize,
    /// Trigger mode.
    pub trigger: IrqTrigger,
    /// Controller family that owns this IRQ, if known.
    pub controller: Option<IrqController>,
    /// Domain this IRQ belongs to, if the host partitions IRQ number space.
    pub domain: Option<IrqDomainId>,
    /// Hardware IRQ number within `controller` / `domain`, when distinct from
    /// the OS-visible `number`.
    pub hwirq: Option<usize>,
}

impl IrqResource {
    /// Construct a minimal IRQ resource with just a number and trigger.
    pub const fn new(number: usize, trigger: IrqTrigger) -> Self {
        Self {
            number,
            trigger,
            controller: None,
            domain: None,
            hwirq: None,
        }
    }

    /// Builder: attach a controller family.
    pub const fn with_controller(mut self, c: IrqController) -> Self {
        self.controller = Some(c);
        self
    }

    /// Builder: attach a domain id.
    pub const fn with_domain(mut self, d: IrqDomainId) -> Self {
        self.domain = Some(d);
        self
    }

    /// Builder: attach the hardware IRQ number.
    pub const fn with_hwirq(mut self, n: usize) -> Self {
        self.hwirq = Some(n);
        self
    }
}

/// Device-visible MSI message returned by the host IRQ core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsiMessage {
    /// Message address programmed into the device.
    pub address: u64,
    /// Message data programmed into the device.
    pub data: u32,
}

impl MsiMessage {
    /// Creates a device-visible MSI message.
    pub const fn new(address: u64, data: u32) -> Self {
        Self { address, data }
    }
}

/// MSI/MSI-X interrupt resource allocated by the host IRQ core.
#[derive(Debug, PartialEq, Eq)]
pub struct MsiResource {
    /// OS-visible IRQ used for handler registration.
    pub irq: IrqResource,
    /// Device-visible message to program into MSI/MSI-X registers.
    pub message: MsiMessage,
}

impl MsiResource {
    /// Creates an MSI resource from an IRQ and device message.
    pub const fn new(irq: IrqResource, message: MsiMessage) -> Self {
        Self { irq, message }
    }
}

/// A discovered interrupt route awaiting translation into an [`IrqResource`].
///
/// Firmware (device-tree / ACPI) or bus discovery produces this raw
/// description; the provider's [`IrqOp::map_irq`] turns it into the OS-visible
/// [`IrqResource`].
#[derive(Debug, Clone, Copy)]
pub struct IrqRouteDesc {
    /// Hardware IRQ number as discovered (e.g. GIC SPI number, PLIC source).
    pub hwirq: usize,
    /// Trigger mode.
    pub trigger: IrqTrigger,
    /// Controller family that owns this route.
    pub controller: IrqController,
    /// Domain id, when the host partitions IRQ number space.
    pub domain: Option<IrqDomainId>,
}

/// A small opaque identifier for a logical interrupt event source.
///
/// Drivers assign meaning to these (e.g. a NIC may use 0 = rx queue, 1 = tx
/// queue). The device-res layer treats them as opaque indices into an
/// [`IrqEvent`] source bitmap and never interprets the value.
pub type IrqEventSource = u8;

/// Maximum number of distinct event sources a single [`IrqEvent`] can describe.
pub const IRQ_EVENT_SOURCES: usize = 8;

/// Outcome reported by an interrupt handler, augmented with which logical
/// event sources fired.
///
/// Carries (a) whether the handler claimed the interrupt and (b) a small
/// bitmap of which logical sources triggered. The interpretation of each bit
/// is left to the driver. Uses a plain `u8` so the type is `Copy` and
/// allocation-free — safe to construct and return from interrupt context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IrqEvent {
    handled: bool,
    sources: u8,
}

impl IrqEvent {
    /// The handler claimed the interrupt but reports no specific source.
    pub const HANDLED: Self = Self {
        handled: true,
        sources: 0,
    };
    /// The handler did not claim the interrupt (shared-line fallback).
    pub const NOT_HANDLED: Self = Self {
        handled: false,
        sources: 0,
    };

    /// Claim the interrupt with the given source bitmask (bit `i` set ⇒ source
    /// `i` fired).
    pub const fn from_sources(sources: u8) -> Self {
        Self {
            handled: true,
            sources,
        }
    }

    /// Whether this handler claimed and serviced the interrupt.
    pub const fn handled(&self) -> bool {
        self.handled
    }

    /// The raw source bitmask.
    pub const fn sources(&self) -> u8 {
        self.sources
    }

    /// Whether source `src` is reported as fired.
    pub fn has_source(&self, src: IrqEventSource) -> bool {
        src < IRQ_EVENT_SOURCES as u8 && (self.sources & (1 << src)) != 0
    }

    /// Combine another event into this one (OR the source bits; handled if
    /// either is). Useful for shared-line dispatch aggregating handlers.
    pub fn merge(&mut self, other: IrqEvent) {
        self.handled |= other.handled;
        self.sources |= other.sources;
    }
}

impl Default for IrqEvent {
    fn default() -> Self {
        Self::HANDLED
    }
}

/// A device interrupt handler.
///
/// Handlers run in interrupt context: they must not block, must not allocate,
/// and should defer heavy work to a thread. The argument is the OS-visible IRQ
/// number that triggered the handler. Any closure that is
/// `Fn(usize) -> IrqEvent + Send + Sync` implements this trait.
pub trait IrqHandler: Send + Sync {
    /// Service a fired interrupt and report which event sources fired.
    fn handle(&self, irq: usize) -> IrqEvent;
}

impl<F> IrqHandler for F
where
    F: Fn(usize) -> IrqEvent + Send + Sync,
{
    fn handle(&self, irq: usize) -> IrqEvent {
        self(irq)
    }
}

/// Provider-owned identity for one registered interrupt handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IrqHandlerToken(usize);

impl IrqHandlerToken {
    /// Create a token from a provider-local numeric id.
    pub const fn new(id: usize) -> Self {
        Self(id)
    }

    /// Return the provider-local numeric id.
    pub const fn id(self) -> usize {
        self.0
    }
}

/// Interrupt capability: handler registration, interrupt-domain translation,
/// and MSI-X vector allocation.
///
/// A host kernel implements this and installs it once via
/// [`crate::set_irq_provider`]. The domain-translation and MSI-X methods have
/// default implementations so a host only overrides what its interrupt
/// controller supports (e.g. non-x86 leaves MSI-X as `Unsupported`).
pub trait IrqOp: Sync {
    /// Register an interrupt handler for `irq`.
    fn request_irq(
        &self,
        irq: IrqResource,
        handler: Arc<dyn IrqHandler>,
    ) -> ResResult<IrqHandlerToken>;

    /// Release an interrupt handler previously registered for `irq`.
    fn release_irq(&self, irq: IrqResource, token: IrqHandlerToken);

    /// Enable or disable delivery of `irq`.
    fn set_irq_enabled(&self, irq: IrqResource, enabled: bool);

    /// Translate a discovered interrupt route into an OS-visible IRQ resource.
    ///
    /// Default: treat the hardware number as the OS-visible number with no
    /// translation.
    fn map_irq(&self, route: IrqRouteDesc) -> ResResult<IrqResource> {
        Ok(IrqResource::new(route.hwirq, route.trigger).with_controller(route.controller))
    }

    /// Allocate a PCI MSI-X interrupt resource.
    ///
    /// The returned IRQ is OS-visible and suitable for handler registration.
    /// The returned message is device-visible and suitable for programming an
    /// MSI-X table entry. Controller-local vectors and APIC destination details
    /// remain host IRQ-core implementation details.
    fn alloc_msix(&self) -> ResResult<MsiResource> {
        Err(ResError::Unsupported)
    }

    /// Release an MSI-X resource previously allocated by [`Self::alloc_msix`].
    /// Default: no-op.
    fn free_msix(&self, resource: MsiResource) {
        let _ = resource;
    }
}

/// RAII handle to a registered interrupt.
///
/// Dropping the handle releases the handler through the installed provider.
#[derive(Debug)]
pub struct Irq {
    resource: IrqResource,
    token: IrqHandlerToken,
    armed: bool,
}

impl Irq {
    /// Register `handler` for the interrupt described by `resource`.
    pub fn request(resource: IrqResource, handler: Arc<dyn IrqHandler>) -> ResResult<Self> {
        let token = irq_provider()?.request_irq(resource, handler)?;
        Ok(Self {
            resource,
            token,
            armed: true,
        })
    }

    /// Enable or disable delivery of this interrupt.
    pub fn set_enabled(&self, enabled: bool) {
        if let Some(provider) = try_irq_provider() {
            provider.set_irq_enabled(self.resource, enabled);
        }
    }

    /// The interrupt number.
    pub fn number(&self) -> usize {
        self.resource.number
    }
}

impl Drop for Irq {
    fn drop(&mut self) {
        if self.armed
            && let Some(provider) = try_irq_provider()
        {
            provider.release_irq(self.resource, self.token);
        }
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IRQ resource types: trigger/controller/domain/resource description, the
//! [`IrqEvent`] outcome, the [`IrqHandler`] / [`IrqOp`] capability traits, and
//! the [`Irq`] RAII handle.

use alloc::sync::Arc;

use crate::{ResError, ResResult};

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
/// event sources fired and whether a threaded handler should run.
///
/// Carries whether the handler claimed the interrupt, a small bitmap of which
/// logical sources triggered, and a wake-thread bit for threaded IRQ primary
/// handlers. The interpretation of each source bit is left to the driver. The
/// value is `Copy` and allocation-free, so it is safe to construct and return
/// from interrupt context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IrqEvent {
    handled: bool,
    sources: u8,
    wake_thread: bool,
}

impl IrqEvent {
    /// The handler claimed the interrupt but reports no specific source.
    pub const HANDLED: Self = Self {
        handled: true,
        sources: 0,
        wake_thread: false,
    };
    /// The handler did not claim the interrupt (shared-line fallback).
    pub const NOT_HANDLED: Self = Self {
        handled: false,
        sources: 0,
        wake_thread: false,
    };
    /// The handler claimed the interrupt and requests its threaded handler.
    pub const WAKE_THREAD: Self = Self {
        handled: true,
        sources: 0,
        wake_thread: true,
    };

    /// Claim the interrupt with the given source bitmask (bit `i` set ⇒ source
    /// `i` fired).
    pub const fn from_sources(sources: u8) -> Self {
        Self {
            handled: true,
            sources,
            wake_thread: false,
        }
    }

    /// Claim the interrupt with source bits and request the threaded handler.
    pub const fn wake_thread_from_sources(sources: u8) -> Self {
        Self {
            handled: true,
            sources,
            wake_thread: true,
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

    /// Whether this handler requests its threaded IRQ handler.
    pub const fn wake_thread(&self) -> bool {
        self.wake_thread
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
        self.wake_thread |= other.wake_thread;
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

/// A sleepable device threaded IRQ handler.
///
/// Thread handlers run in task context after the primary IRQ handler returns
/// [`IrqEvent::WAKE_THREAD`] or [`IrqEvent::wake_thread_from_sources`]. They may
/// use sleepable primitives, but must still obey the owning driver's teardown
/// protocol because devres release waits for the IRQ thread before freeing the
/// action.
pub trait IrqThreadHandler: Send + Sync {
    /// Service deferred interrupt work in task context.
    fn handle(&self, irq: usize) -> IrqEvent;
}

impl<F> IrqThreadHandler for F
where
    F: Fn(usize) -> IrqEvent + Send + Sync,
{
    fn handle(&self, irq: usize) -> IrqEvent {
        self(irq)
    }
}

/// Provider-owned identity for one registered interrupt handler.
///
/// Shared hardirq registrations are released by the provider-local action id.
/// Non-shared regular registrations, including current threaded IRQ requests,
/// own the whole IRQ action and are released without a per-action id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqHandlerToken {
    /// One handler within a shared IRQ action list.
    SharedAction(usize),
    /// The sole regular action installed on an IRQ line.
    RegularAction,
}

impl IrqHandlerToken {
    /// Create a token for one shared IRQ action.
    pub const fn shared_action(id: usize) -> Self {
        Self::SharedAction(id)
    }

    /// Create a token for a non-shared regular IRQ action.
    pub const fn regular_action() -> Self {
        Self::RegularAction
    }
}

/// Interrupt capability: handler registration, interrupt-domain translation,
/// and MSI-X vector allocation.
///
/// A host kernel implements this and passes it to its driver framework. The
/// domain-translation and MSI-X methods have default implementations so a host
/// only overrides what its interrupt controller supports (e.g. non-x86 leaves
/// MSI-X as `Unsupported`).
pub trait IrqOp: Sync {
    /// Register an interrupt handler for `irq`.
    fn request_irq(
        &self,
        irq: IrqResource,
        handler: Arc<dyn IrqHandler>,
    ) -> ResResult<IrqHandlerToken>;

    /// Register a primary interrupt handler and a sleepable threaded handler.
    ///
    /// The primary handler still runs in interrupt context and should return
    /// [`IrqEvent::WAKE_THREAD`] or [`IrqEvent::wake_thread_from_sources`] when
    /// the threaded handler should run. Providers that do not support threaded
    /// IRQs return [`ResError::Unsupported`].
    fn request_threaded_irq(
        &self,
        irq: IrqResource,
        primary: Arc<dyn IrqHandler>,
        thread: Arc<dyn IrqThreadHandler>,
        name: Option<&'static str>,
    ) -> ResResult<IrqHandlerToken> {
        let _ = (irq, primary, thread, name);
        Err(ResError::Unsupported)
    }

    /// Register a sleepable threaded handler with the provider's default
    /// primary wake handler.
    ///
    /// This is the common level-triggered device path: the provider is expected
    /// to apply its safe oneshot policy so the line remains masked until the
    /// threaded handler reaches idle.
    fn request_threaded_irq_default(
        &self,
        irq: IrqResource,
        thread: Arc<dyn IrqThreadHandler>,
        name: Option<&'static str>,
    ) -> ResResult<IrqHandlerToken> {
        let _ = (irq, thread, name);
        Err(ResError::Unsupported)
    }

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
/// Dropping the handle releases the handler through the provider that created
/// it.
pub struct Irq {
    provider: Option<&'static dyn IrqOp>,
    resource: IrqResource,
    token: IrqHandlerToken,
    armed: bool,
}

impl Irq {
    /// Register `handler` with an explicit provider.
    pub fn request_with(
        provider: &'static dyn IrqOp,
        resource: IrqResource,
        handler: Arc<dyn IrqHandler>,
    ) -> ResResult<Self> {
        let token = provider.request_irq(resource, handler)?;
        Ok(Self {
            provider: Some(provider),
            resource,
            token,
            armed: true,
        })
    }

    /// Register primary and threaded handlers with an explicit provider.
    pub fn request_threaded_with(
        provider: &'static dyn IrqOp,
        resource: IrqResource,
        primary: Arc<dyn IrqHandler>,
        thread: Arc<dyn IrqThreadHandler>,
        name: Option<&'static str>,
    ) -> ResResult<Self> {
        let token = provider.request_threaded_irq(resource, primary, thread, name)?;
        Ok(Self {
            provider: Some(provider),
            resource,
            token,
            armed: true,
        })
    }

    /// Register a default-primary threaded handler with an explicit provider.
    pub fn request_threaded_default_with(
        provider: &'static dyn IrqOp,
        resource: IrqResource,
        thread: Arc<dyn IrqThreadHandler>,
        name: Option<&'static str>,
    ) -> ResResult<Self> {
        let token = provider.request_threaded_irq_default(resource, thread, name)?;
        Ok(Self {
            provider: Some(provider),
            resource,
            token,
            armed: true,
        })
    }

    /// Enable or disable delivery of this interrupt.
    pub fn set_enabled(&self, enabled: bool) {
        if let Some(provider) = self.provider {
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
            && let Some(provider) = self.provider.take()
        {
            provider.release_irq(self.resource, self.token);
        }
    }
}

impl core::fmt::Debug for Irq {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Irq")
            .field("resource", &self.resource)
            .field("token", &self.token)
            .field("armed", &self.armed)
            .finish_non_exhaustive()
    }
}

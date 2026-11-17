// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The IRQ family: handler callback vocabulary and the request guard.

use alloc::sync::Arc;

pub use irq_types::{IrqController, IrqDomainId, IrqTrigger};

mod guard;
mod handler;

pub use guard::DevIrq;
pub use handler::{IrqEvent, IrqHandler, IrqThreadHandler};

use crate::{ResError, ResResult};

/// An OS-visible virtual interrupt number (the number the host IRQ core
/// dispatches).
///
/// Firmware reports controller-relative **hwirq** numbers, the bus may
/// already carry a **virq**, and consumers (a shared-line filter, an MSI-X
/// vector pairing, a uevent) need one or the other. Both used to be `usize`,
/// which is exactly the confusion that made IRQ routing bugs invisible on
/// identity-mapped QEMU. The newtype keeps them apart: only [`Virq::raw`]
/// crosses into a plain number, and only at a boundary that demands one.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Virq(usize);

impl Virq {
    /// Wrap a number the host IRQ core already resolved.
    pub const fn new(number: usize) -> Self {
        Self(number)
    }

    /// The raw number, for APIs that predate this type (kernel IRQ core,
    /// vendored device libraries) — the single explicit crossing point.
    pub const fn raw(self) -> usize {
        self.0
    }
}

impl core::fmt::Display for Virq {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Device-visible MSI message returned by the host IRQ core.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]

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

/// An interrupt resource.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrqResource {
    /// A plain virtual irq with no retained origin.
    PlainVirq { virq: Virq, trigger: IrqTrigger },
    /// A domain-mapped irq that remembers its firmware origin: the
    /// controller/domain/hwirq the mapping went through — identity audits
    /// and release go through the same domain the mapping came from,
    /// never a re-derived guess. The mapping has HAPPENED: `virq` is
    /// the real virq (request paths resolve firmware routes through
    /// `map_irq` before registering — no pre-mapping placeholder state
    /// exists in this shape anymore).
    DomainMapped {
        /// OS-visible irq number (the domain translation's output).
        virq: Virq,
        trigger: IrqTrigger,
        controller: IrqController,
        /// The domain the mapping used: firmware-stated, or the
        /// controller's root domain when firmware named none (single-domain
        /// controllers).
        domain: Option<IrqDomainId>,
        hwirq: usize,
    },
}

/// MSI/MSI-X interrupt resource allocated by the host IRQ core.
///
/// This is an explicit allocation identity, not an RAII release guard. Its owner
/// must return it to the allocating provider after source/action shutdown;
/// dropping this value alone does not free the vector. [`IrqOp::try_free_msix`]
/// returns this same identity on rejection so the owner can retain and retry it.
#[derive(Debug, Eq, PartialEq)]
pub struct MsiResource {
    /// OS-visible IRQ used for handler registration.
    pub irq: IrqResource,
    /// Device-visible message to program into MSI/MSI-X registers.
    pub message: MsiMessage,
}

/// A discovered interrupt route awaiting translation into an [`IrqResource`].
///
/// Firmware (device-tree / ACPI) or bus discovery produces this raw
/// description; the provider's [`IrqOp::map_irq`] turns it into the OS-visible
/// [`IrqResource`].
#[derive(Clone, Copy, Debug)]
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

/// Provider-owned identity for one registered interrupt handler.
///
/// Shared hardirq registrations are released by the provider-local action id.
/// Non-shared regular registrations own the whole IRQ action and are released
/// without a per-action id. Threaded registration defaults to Unsupported.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrqHandlerToken {
    /// One handler within a shared IRQ action list.
    SharedAction(usize),
    /// The sole regular action installed on an IRQ line.
    RegularAction,
}

/// Interrupt capability: handler registration, interrupt-domain translation,
/// and MSI-X vector allocation.
///
/// A host kernel implements this and passes it to its driver framework. The
/// domain-translation and MSI-X methods have default implementations so a host
/// only overrides what its interrupt controller supports (e.g. non-x86 leaves
/// MSI-X as `Unsupported`).
pub trait IrqOp: Send + Sync {
    /// Register a normal device interrupt handler for the resolved `irq`.
    ///
    /// The handler captures its device context, receives the OS-visible virtual
    /// IRQ number and returns IrqEvent. The host
    /// adapts this to its own callback and result types; it must not dispatch
    /// this callback through a pseudo-NMI path. Invocation must not sleep.
    /// A wake-thread bit does not establish a
    /// threaded registration; this ordinary entry has no paired thread handler.
    /// The callback may run before this method returns. On error the provider
    /// must retain neither a new registration nor its callback.
    fn request_irq(
        &self,
        irq: IrqResource,
        handler: Arc<dyn IrqHandler>,
    ) -> ResResult<IrqHandlerToken>;

    /// Register a primary interrupt handler and a sleepable threaded handler.
    ///
    /// The primary handler still runs in interrupt context and should return
    /// [`IrqEvent::WAKE_THREAD`] when
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

    /// Detach the exact previously registered action and drain in-flight calls.
    ///
    /// Returning means the callback can no longer execute. Callers must stop
    /// device access and use the provider's permitted teardown context without
    /// locks needed by handlers. This infallible interface must not turn a
    /// failed detach/drain into a successful return, nor disable other actions
    /// sharing the line. The original resource and token must be supplied once.
    fn release_irq(&self, irq: IrqResource, token: IrqHandlerToken);

    /// Enable or disable delivery of `irq`.
    fn set_irq_enabled(&self, irq: IrqResource, enabled: bool);

    /// Map a firmware interrupt declaration to an OS-visible IRQ resource.
    ///
    /// The declaration carries the exact controller firmware node and the
    /// complete binding cells; the provider must resolve both through the host
    /// IRQ core rather than re-decoding cells or guessing a controller family.
    /// Mapping may reuse an existing mapping for the same valid declaration.
    ///
    /// Errors must distinguish an unready controller (`BadState`), a malformed
    /// declaration (`InvalidResource`), an unsupported binding (`Unsupported`),
    /// and an incompatible duplicate association (`AlreadyExists`). A provider
    /// must not fall back to the raw firmware number.
    ///
    /// Default: unsupported; providers without firmware-node identity return
    /// this instead of pretending the declaration was mapped.
    fn map_fw_irq(&self, spec: fwnode::FwIrqSpec<'static>) -> ResResult<IrqResource> {
        let _ = spec;
        Err(ResError::Unsupported)
    }

    /// Translate a discovered interrupt route into an OS-visible IRQ resource.
    ///
    /// Legacy route mapping by controller family and hardware number.
    /// [`IrqRouteDesc::hwirq`] is always controller-local; an ACPI route
    /// carrying a global system interrupt (GSI) must go through
    /// [`Self::map_gsi`] instead. New firmware consumers use
    /// [`Self::map_fw_irq`], which preserves the exact controller node and full
    /// binding cells.
    ///
    /// Default: treat the hardware number as the OS-visible number with no
    /// translation.
    fn map_irq(&self, route: IrqRouteDesc) -> ResResult<IrqResource> {
        Ok(IrqResource::mapped(
            Virq::new(route.hwirq),
            route.trigger,
            route.controller,
            route.domain,
            route.hwirq,
        ))
    }

    /// Resolve a firmware-described global system interrupt to an OS-visible
    /// IRQ resource.
    ///
    /// A GSI is a platform-wide number, not a controller-local hardware IRQ.
    /// The provider resolves it through the host's published controller
    /// topology into the owning domain and local pin, then maps that
    /// descriptor like any other route. A GSI that no installed controller
    /// owns is an invalid resource, never an identity mapping.
    ///
    /// Default: unsupported; providers without a GSI namespace return this.
    fn map_gsi(&self, gsi: u32, trigger: IrqTrigger) -> ResResult<IrqResource> {
        let _ = (gsi, trigger);
        Err(ResError::Unsupported)
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

    /// Try to release a resource obtained from this provider's [`Self::alloc_msix`].
    ///
    /// The caller must stop device message production, order already-issued
    /// messages, synchronously detach/drain all IRQ actions, and leave the line
    /// logically disabled. Serialize release against registration and other
    /// allocation-owner operations. This does not stop DMA or drain actions.
    /// Call in sleepable task context, not IRQ/NMI context, without spinlocks,
    /// IRQ/preemption exclusion or RCU read scopes. Host retirement services
    /// must be running; follow any additional provider context requirements.
    ///
    /// Success consumes the resource.
    ///
    /// # Errors
    /// On error there must be no release side
    /// effects: return the original resource unchanged and still reusable by
    /// its owner for an explicit retry after correcting the rejection cause.
    /// No retry is scheduled. The default returns [`ResError::Unsupported`].
    fn try_free_msix(&self, resource: MsiResource) -> Result<(), (ResError, MsiResource)> {
        Err((ResError::Unsupported, resource))
    }

    /// Terminal compatibility cleanup with [`Self::try_free_msix`] preconditions.
    ///
    /// Explicit teardown owners must use the fallible operation to retain and
    /// recover rejected resources. This wrapper neither retries nor implements
    /// dynamic device removal.
    ///
    /// # Panics
    /// Panics on rejection rather than silently discarding allocation ownership.
    fn free_msix(&self, resource: MsiResource) {
        if let Err((error, resource)) = self.try_free_msix(resource) {
            #[expect(
                clippy::panic,
                reason = "CG-021-no-panic-on-reachable-failure: TODO(cg): terminal MSI-X cleanup \
                          cannot return rejected ownership; explicit teardown owners must use \
                          try_free_msix and retain the resource for recovery"
            )]
            {
                panic!("MSI-X release rejected: {resource:?}: {error:?}");
            }
        }
    }
}

impl IrqResource {
    /// A plain (origin-less) virtual irq.
    pub const fn plain(virq: Virq, trigger: IrqTrigger) -> Self {
        Self::PlainVirq { virq, trigger }
    }

    /// A domain-mapped irq with its firmware origin.
    pub const fn mapped(
        virq: Virq,
        trigger: IrqTrigger,
        controller: IrqController,
        domain: Option<IrqDomainId>,
        hwirq: usize,
    ) -> Self {
        Self::DomainMapped {
            virq,
            trigger,
            controller,
            domain,
            hwirq,
        }
    }

    /// The OS-visible virtual irq of either shape.
    pub const fn virq(&self) -> Virq {
        match self {
            Self::PlainVirq { virq, .. } | Self::DomainMapped { virq, .. } => *virq,
        }
    }

    /// The trigger mode of either shape.
    pub const fn trigger(&self) -> IrqTrigger {
        match self {
            Self::PlainVirq { trigger, .. } | Self::DomainMapped { trigger, .. } => *trigger,
        }
    }
}

impl MsiResource {
    /// Creates an MSI resource from an IRQ and device message.
    pub const fn new(irq: IrqResource, message: MsiMessage) -> Self {
        Self { irq, message }
    }
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

unittest::path_mod_test!(tests, "../ut-tests/irq.rs");

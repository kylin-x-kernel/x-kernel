// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ACPI INTx ownership for the PCI backend (x86 only).
//!
//! This module owns the one AML context used for PCI routing. It runs
//! `\_PIC(1)` and loads the root `_PRT` once during PCI enumeration, publishes
//! the resulting read-only [`PrtTable`], and answers per-device route queries
//! without evaluating AML again.
//!
//! The context is retained for the kernel lifetime so the host/OSL lifetime
//! stays valid. It is not locked for lookups: [`LazyInit::call_once`] runs the
//! initialization outside any lock and only the immutable table is consulted
//! afterwards. A failed initialization is retained for the boot; later
//! enumerations reuse the failure instead of retrying `_PIC`.
//!
//! This module publishes firmware route facts only. Reading the PCI Interrupt
//! Pin register, mapping the route through the provider, recording the result
//! with the device, and converting failures into probe errors belong to the
//! PCI and VirtIO integration layers.

use alloc::boxed::Box;

use acpi::{
    context::{AcpiContext, IrqModel},
    prt::{self, PrtLookupError, PrtRoute, PrtTable, PrtTrigger},
};
use driver_os_api_xkernel::acpi_osl::AcpiOs;
use lazyinit::LazyInit;
use pci::{DeviceFunction, LegacyInterruptRoute, PciConfigAccess};

/// Compact reason why the ACPI INTx owner could not be initialized.
///
/// The full typed error is logged once at initialization; this snapshot keeps
/// the failure publishable from a `Sync` static and reachable at query time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InitFailure {
    /// The AML context could not be constructed.
    Context,
    /// `\_PIC(1)` selection failed.
    IrqModel,
    /// The root `_PRT` could not be loaded.
    Prt,
}

/// Why a firmware route query produced no answer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RouteFailure {
    /// The owner was queried before the ACPI discovery path initialized it.
    NotInitialized,
    /// Initialization failed earlier; the failure is permanent for the boot.
    Init(InitFailure),
    /// The published `_PRT` could not answer this key.
    Lookup(PrtLookupError),
}

/// Published owner payload.
///
/// Boxed so the one-time static does not reserve the context's full size for
/// the compact `Failed` state.
struct ReadyState {
    /// Retained so the OSL/interpreter lifetime outlives the table.
    _ctx: AcpiContext<AcpiOs>,
    table: PrtTable,
}

enum OwnerState {
    Ready(Box<ReadyState>),
    Failed(InitFailure),
}

static OWNER: LazyInit<OwnerState> = LazyInit::new();

// The owner is published in a `Sync` static: the context must be shareable and
// the initialized table is read concurrently by route queries.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<OwnerState>();
};

/// Initializes the ACPI INTx owner once for the boot.
///
/// Called by the PCI backend during enumeration, after the allocator and after
/// firmware tables have been discovered. Later calls reuse the retained state.
pub(crate) fn init(config: PciConfigAccess) {
    let _ = OWNER.call_once(|| initialize(config));
}

/// Queries the published firmware route for one PCI function and pin.
///
/// Returns `Ok(None)` when the table has no entry for the key,
/// `Ok(Some(route))` for the published route, and a typed failure when the
/// owner is not ready or the table rejected the key. No AML is evaluated and
/// no context lock is taken here.
pub(crate) fn route(
    bdf: DeviceFunction,
    pin: u8,
) -> Result<Option<LegacyInterruptRoute>, RouteFailure> {
    route_from_owner(OWNER.get(), bdf, pin)
}

/// Resolves a route against one publication state.
///
/// Split from [`route`] so the not-initialized and retained-failure outcomes
/// stay testable: by the time unit tests run, boot enumeration has already
/// initialized the process-wide owner.
fn route_from_owner(
    state: Option<&OwnerState>,
    bdf: DeviceFunction,
    pin: u8,
) -> Result<Option<LegacyInterruptRoute>, RouteFailure> {
    let table = match state {
        Some(OwnerState::Ready(ready)) => &ready.table,
        Some(OwnerState::Failed(failure)) => return Err(RouteFailure::Init(*failure)),
        None => return Err(RouteFailure::NotInitialized),
    };
    route_from_lookup(table.lookup(bdf.bus, bdf.device, bdf.function, pin))
}

/// Converts one `_PRT` lookup result into the neutral PCI route type.
fn route_from_lookup(
    lookup: Result<Option<PrtRoute>, PrtLookupError>,
) -> Result<Option<LegacyInterruptRoute>, RouteFailure> {
    match lookup {
        Ok(Some(route)) => Ok(Some(LegacyInterruptRoute::Gsi {
            gsi: route.gsi,
            trigger: prt_trigger_to_firmware(route.trigger),
        })),
        Ok(None) => Ok(None),
        Err(error) => Err(RouteFailure::Lookup(error)),
    }
}

fn prt_trigger_to_firmware(trigger: PrtTrigger) -> pci::InterruptTrigger {
    match trigger {
        PrtTrigger::EdgeRising => pci::InterruptTrigger::EdgeRising,
        PrtTrigger::EdgeFalling => pci::InterruptTrigger::EdgeFalling,
        PrtTrigger::LevelHigh => pci::InterruptTrigger::LevelHigh,
        PrtTrigger::LevelLow => pci::InterruptTrigger::LevelLow,
    }
}

fn initialize(config: PciConfigAccess) -> OwnerState {
    let mut ctx = match AcpiContext::new(AcpiOs::for_kernel(Some(config))) {
        Ok(ctx) => ctx,
        Err(error) => {
            log::error!("ACPI INTx: context creation failed: {error:?}");
            return OwnerState::Failed(InitFailure::Context);
        }
    };

    if let Err(error) = ctx.init_irq_model(IrqModel::IoApic) {
        log::error!("ACPI INTx: \\_PIC(1) failed: {error:?}");
        return OwnerState::Failed(InitFailure::IrqModel);
    }

    match prt::load(&mut ctx, 0) {
        Ok(table) => {
            log::info!(
                "ACPI INTx: root _PRT {:?} with {} entries",
                table.origin(),
                table.len()
            );
            OwnerState::Ready(Box::new(ReadyState { _ctx: ctx, table }))
        }
        Err(error) => {
            log::error!("ACPI INTx: root _PRT load failed: {error:?}");
            OwnerState::Failed(InitFailure::Prt)
        }
    }
}

unittest::path_mod_test!(acpi_owner_unittest, "../../ut-tests/acpi_owner_unittest.rs");

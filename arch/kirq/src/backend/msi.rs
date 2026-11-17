// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Generic MSI/MSI-X allocation and message model.
//!
//! `kirq` owns the OS-visible MSI IRQ resource. Concrete interrupt-controller
//! backends own CPU-vector allocation and architecture-specific MSI message
//! composition.

#[cfg(target_arch = "x86_64")]
use crate::state;
use crate::{Hwirq, IrqAffinity, Virq};
#[cfg(target_arch = "x86_64")]
use crate::{IrqController, IrqDesc, IrqFlags, IrqTrigger, MSI_DOMAIN};

/// Architecture-neutral MSI message programmed into a device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MsiMessage {
    address: u64,
    data: u32,
}

impl MsiMessage {
    /// Creates an MSI message from a target address and payload data.
    pub const fn new(address: u64, data: u32) -> Self {
        Self { address, data }
    }

    /// Returns the message address to write into the device's MSI registers.
    pub const fn address(self) -> u64 {
        self.address
    }

    /// Returns the message payload data to write into the device's MSI registers.
    pub const fn data(self) -> u32 {
        self.data
    }
}

/// MSI interrupt style requested from the active backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MsiKind {
    /// PCI MSI-X table entry.
    PciMsix,
}

/// Capability token required to call backend-only MSI operations.
#[derive(Clone, Copy, Debug)]
pub struct MsiBackendToken {
    _private: (),
}

impl MsiBackendToken {
    #[cfg(target_arch = "x86_64")]
    const fn new() -> Self {
        Self { _private: () }
    }
}

/// Exclusive ownership of one backend allocation, consumed by explicit release.
///
/// Only KIRQ constructs this after a successful backend allocation. It cannot
/// be cloned or reconstructed from a hardware number. Published ownership lives
/// in the Line, not in the device-visible allocation handle. Drop deliberately
/// does not release hardware: dropping a lookup owner proves no device quiescence.
#[must_use = "MSI reservations must be published or explicitly returned to the backend"]
#[derive(Debug)]
pub struct MsiReservation {
    hwirq: Hwirq,
}

impl MsiReservation {
    /// Returns the hardware number owned by this reservation.
    pub const fn hwirq(&self) -> Hwirq {
        self.hwirq
    }

    #[cfg(unittest)]
    pub(crate) const fn for_test(hwirq: Hwirq) -> Self {
        // Software fixtures must consume this through an injected release,
        // never through the real platform allocator.
        Self { hwirq }
    }
}

/// Device-facing identity and message for a KIRQ-owned MSI allocation.
///
/// Dropping this handle does not free the reservation. The resource owner must
/// stop the device, remove its actions, and explicitly call [`free_msix`].
#[derive(Debug, Eq, PartialEq)]
pub struct MsiAllocation {
    virq: Virq,
    message: MsiMessage,
}

impl MsiAllocation {
    /// Returns the OS-visible IRQ number used for handler registration.
    pub const fn virq(&self) -> Virq {
        self.virq
    }

    /// Returns the MSI message to program into the device.
    pub const fn message(&self) -> MsiMessage {
        self.message
    }
}

/// Platform backend for MSI vector allocation and message composition.
#[kiface::interface]
pub trait MsiBackendIf {
    /// Allocates a backend-local hardware MSI vector.
    fn alloc_msi_vector(
        token: MsiBackendToken,
        kind: MsiKind,
        affinity: IrqAffinity,
    ) -> Option<Hwirq>;

    /// Consumes an exclusively owned backend reservation after withdrawal.
    ///
    /// KIRQ releases registry exclusion and submits index retirements before
    /// published-resource release; allocation rollback also runs unlocked. The backend must
    /// provide its own allocator exclusion and must not sleep or reenter KIRQ.
    /// KIRQ withdraws published mappings before this call, or supplies a fresh
    /// unpublished allocation for rollback. Returning normally commits release;
    /// there is no recoverable refusal after withdrawal. An invalid reservation
    /// is an internal allocator/ownership bug, not a device teardown error.
    /// Device shutdown remains the resource owner's duty. A backend requiring
    /// fallible release must add a pre-withdrawal protocol before integration.
    fn free_msi_vector(reservation: MsiReservation);

    /// Composes the device-visible MSI message for `hwirq`.
    ///
    /// Returns `None` if the backend cannot encode `hwirq` or the requested
    /// affinity into a device-visible MSI message.
    fn compose_msi_message(
        token: MsiBackendToken,
        hwirq: Hwirq,
        affinity: IrqAffinity,
    ) -> Option<MsiMessage>;
}

/// Allocates a PCI MSI-X interrupt for the requested affinity.
///
/// The returned [`MsiAllocation::virq`] is the IRQ number higher layers should
/// register a handler on. The CPU vector and APIC destination remain backend
/// details and are intentionally not exposed.
///
/// Failure after backend allocation rolls back the unpublished vector. The
/// backend must accept release of that fresh, exclusively owned reservation.
/// Both index retirement records are allocated before publishing the Line;
/// failure to prepare them follows this rollback path, not a later release.
/// NMI callers are rejected before entering the backend allocator.
pub fn alloc_msix(affinity: IrqAffinity) -> Option<MsiAllocation> {
    #[cfg(target_arch = "x86_64")]
    {
        alloc_msi(MsiKind::PciMsix, affinity)
    }

    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = affinity;
        None
    }
}

/// Releases a PCI MSI-X interrupt previously allocated by [`alloc_msix`].
///
/// The owner must first stop device message production, satisfy the transport's
/// ordering requirements for already-issued messages, and synchronously remove
/// its IRQ actions. The Line must also be logically disabled; an actionless
/// Line enabled through the legacy force-enable API needs an explicit disable.
/// Serialize release against registration and other owner operations.
/// Call from a watched, sleepable task with the RCU service running. No read
/// scope or caller-held IRQ/preemption exclusion may span this operation.
///
/// Returns false for an unknown/non-MSI IRQ, a mapping without an allocation
/// reservation, an IRQ still in use or logically enabled, invalid submission
/// context. Retirement storage is prepared before allocation publication;
/// release does not allocate retirement records.
/// Rejection does not remove the mapping or release its reservation;
/// the owner must retain its allocation identity for cleanup. Success releases
/// the mapping before returning the reservation; the old IRQ number must no longer
/// be used. This function neither resets the device nor waits for its DMA.
pub fn free_msix(virq: Virq) -> bool {
    free_msi(virq)
}

#[cfg(target_arch = "x86_64")]
fn alloc_msi(kind: MsiKind, affinity: IrqAffinity) -> Option<MsiAllocation> {
    if crate::context::is_in_interrupt_context() || krcu::is_reading() {
        return None;
    }
    let token = MsiBackendToken::new();
    let hwirq = MsiBackendIf::alloc_msi_vector(token, kind, affinity)?;
    let reservation = MsiReservation { hwirq };
    let Some(message) = MsiBackendIf::compose_msi_message(token, hwirq, affinity) else {
        MsiBackendIf::free_msi_vector(reservation);
        return None;
    };

    let desc = IrqDesc::new(hwirq, IrqTrigger::EdgeRising)
        .with_controller(IrqController::Msi)
        .with_domain(MSI_DOMAIN)
        .with_affinity(affinity)
        .with_flags(IrqFlags::MSI);
    let virq = match state::publish_msi_reservation(desc, reservation) {
        Ok(virq) => virq,
        Err((err, reservation)) => {
            warn!("failed to map MSI vector {hwirq}: {err:?}");
            MsiBackendIf::free_msi_vector(reservation);
            return None;
        }
    };
    Some(MsiAllocation { virq, message })
}

#[cfg(not(target_arch = "x86_64"))]
fn free_msi(_virq: Virq) -> bool {
    false
}

#[cfg(target_arch = "x86_64")]
fn free_msi(virq: Virq) -> bool {
    if crate::context::is_in_nmi() {
        false
    } else {
        state::free_msi_if_unused(virq, MsiBackendIf::free_msi_vector)
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/backend_msi.rs");

#[cfg(not(target_arch = "x86_64"))]
unittest::path_mod_test!(
    unittest_tests,
    "../../ut-tests/backend_msi_unittest_tests.rs"
);

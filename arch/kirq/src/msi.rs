// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Generic MSI/MSI-X allocation and message model.
//!
//! `kirq` owns the OS-visible MSI IRQ resource. Concrete interrupt-controller
//! backends own CPU-vector allocation and architecture-specific MSI message
//! composition.

use super::{Hwirq, IrqAffinity, Virq};
#[cfg(target_arch = "x86_64")]
use super::{IrqController, IrqDesc, IrqFlags, IrqTrigger, MSI_DOMAIN};
#[cfg(target_arch = "x86_64")]
use crate::state::IRQ_STATE;

/// Architecture-neutral MSI message programmed into a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsiKind {
    /// PCI MSI-X table entry.
    PciMsix,
}

/// Capability token required to call backend-only MSI operations.
#[derive(Debug, Clone, Copy)]
pub struct MsiBackendToken {
    _private: (),
}

impl MsiBackendToken {
    #[cfg(target_arch = "x86_64")]
    const fn new() -> Self {
        Self { _private: () }
    }
}

/// A kirq-owned MSI allocation.
#[derive(Debug, PartialEq, Eq)]
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

    /// Releases a backend-local hardware MSI vector.
    fn free_msi_vector(token: MsiBackendToken, hwirq: Hwirq) -> bool;

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
pub fn free_msix(virq: Virq) -> bool {
    free_msi(virq)
}

#[cfg(target_arch = "x86_64")]
fn alloc_msi(kind: MsiKind, affinity: IrqAffinity) -> Option<MsiAllocation> {
    let token = MsiBackendToken::new();
    let hwirq = MsiBackendIf::alloc_msi_vector(token, kind, affinity)?;
    let Some(message) = MsiBackendIf::compose_msi_message(token, hwirq, affinity) else {
        if !MsiBackendIf::free_msi_vector(token, hwirq) {
            warn!("failed to roll back MSI vector {hwirq} after message composition failure");
        }
        return None;
    };

    let desc = IrqDesc::new(hwirq, IrqTrigger::EdgeRising)
        .with_controller(IrqController::Msi)
        .with_domain(MSI_DOMAIN)
        .with_affinity(affinity)
        .with_flags(IrqFlags::MSI);
    let virq = match crate::try_map(desc) {
        Ok(virq) => virq,
        Err(err) => {
            warn!("failed to map MSI vector {hwirq}: {err:?}");
            if !MsiBackendIf::free_msi_vector(token, hwirq) {
                warn!("failed to roll back MSI vector {hwirq} after IRQ mapping failure");
            }
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
    let mut state = IRQ_STATE.lock();
    let Some(desc) = state.stored_desc(virq) else {
        return false;
    };
    if !desc.flags.contains(IrqFlags::MSI) {
        return false;
    }
    if !state.is_unused(virq) {
        warn!(
            "refusing to free MSI IRQ {virq} while a handler or wake subscription is still \
             registered"
        );
        return false;
    }

    let hwirq = desc.hwirq;
    if !MsiBackendIf::free_msi_vector(MsiBackendToken::new(), hwirq) {
        warn!("failed to free backend MSI vector {hwirq} for IRQ {virq}");
        return false;
    }

    state.remove_msi_if_unused(virq).is_some()
}

#[cfg(unittest)]
#[allow(missing_docs)]
mod tests {
    use unittest::def_test;

    use super::MsiMessage;

    #[def_test]
    fn test_msi_message_accessors() {
        let message = MsiMessage::new(0xfee0_0000, 0x80);
        assert_eq!(message.address(), 0xfee0_0000);
        assert_eq!(message.data(), 0x80);
    }
}

#[cfg(all(unittest, not(target_arch = "x86_64")))]
#[allow(missing_docs)]
mod unittest_tests {
    use unittest::def_test;

    use super::{alloc_msix, free_msix};
    use crate::IrqAffinity;

    #[def_test]
    fn test_msix_is_unsupported_without_backend() {
        assert!(alloc_msix(IrqAffinity::Any).is_none());
        assert!(!free_msix(0xffff_ffff));
    }
}

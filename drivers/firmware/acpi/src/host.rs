// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AML host failure contract.
//!
//! The firmware crate owns the failure vocabulary so the AML context, the PCI
//! layer, and the host adapter can carry failures without depending on the
//! vendored interpreter types. It records no failures itself and holds no OSL
//! state.

/// The host operation that recorded a [`HostFailure`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostAccessKind {
    /// Mapping ACPI table memory.
    TableMap,
    /// Reading or writing a SystemMemory operation region.
    SystemMemory,
    /// Reading or writing a SystemIO operation region.
    SystemIo,
    /// Reading or writing a PCI-config operation region.
    PciConfig,
    /// An operation-region space the host does not implement. The address
    /// field carries the raw ACPI region-space number.
    UnsupportedRegion,
    /// An AML `Stall` request.
    Stall,
    /// An AML `Sleep` request.
    Sleep,
    /// An AML mutex operation. The address field carries the handle.
    Mutex,
    /// An AML `Fatal` opcode. The address field carries its argument.
    AmlFatal,
}

/// Why a host access was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostFailureReason {
    /// The operation or region space is not supported by this host.
    Unsupported,
    /// The requested range or width is outside the permitted bounds.
    OutOfRange,
    /// Address arithmetic wrapped.
    Wrap,
    /// The memory category is not permitted for this operation.
    NotPermitted,
    /// A PCI configuration access failed or was out of range.
    PciAccess,
    /// Retaining host-side state for the operation failed.
    Allocation,
    /// A bounded wait timed out.
    Timeout,
    /// The underlying capability reported an I/O failure.
    Io,
}

/// The first host access failure observed during one AML evaluation.
///
/// The AML interpreter returns raw values from its host `Handler` and cannot
/// stop itself when an access fails. The host adapter records the first failure
/// here, blocks all later hardware access, and the ACPI context rejects the
/// evaluation result. Failures are not rolled back: writes that completed
/// before the failure remain in effect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostFailure {
    /// The operation that failed.
    pub access: HostAccessKind,
    /// Physical address, port, PCI config offset, or handle.
    pub address: u64,
    /// Access width in bytes, or zero when the operation has no width.
    pub width: u8,
    /// The rejection reason.
    pub reason: HostFailureReason,
}

impl HostFailure {
    /// Creates a host failure record.
    pub const fn new(
        access: HostAccessKind,
        address: u64,
        width: u8,
        reason: HostFailureReason,
    ) -> Self {
        Self {
            access,
            address,
            width,
            reason,
        }
    }
}

/// Failure-query contract for an AML host adapter.
///
/// This trait deliberately does not expose vendor interpreter types: the
/// firmware facade and the PCI layer can carry `&dyn AmlHost` without depending
/// on the `acpi` interpreter crate.
///
/// `take_failure` drains the recorded failure. The host must remain failed
/// after the drain so later accesses still do not reach hardware.
pub trait AmlHost {
    /// Takes the first host failure recorded since construction or the previous
    /// call, if any.
    fn take_failure(&self) -> Option<HostFailure>;

    /// Records an operation-region space the host does not implement.
    ///
    /// The vendored interpreter rejects some unsupported spaces before calling
    /// a `Handler` method, so this hook lets the host preserve the same sticky
    /// failure contract for those evaluations. `space` is the raw ACPI
    /// region-space number. The default does nothing; an adapter that can
    /// carry failures should override it.
    fn note_unsupported_region(&self, _space: u8) {}
}

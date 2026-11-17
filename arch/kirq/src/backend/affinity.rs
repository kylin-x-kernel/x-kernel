// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Controller routing transactions: immediate application and KIRQ-driven deferred completion.

use kcpu_id_map::KCpuMask;

use crate::IrqDesc;

/// Delivery constraint for an accepted routing transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrqAffinityDelivery {
    /// Preserve actual hardware enablement, including temporary flow masking.
    Preserve,
    /// Establish suppression and keep it on success, including fault recovery.
    KeepMasked,
}

/// A controller must distinguish pre-write rejection from uncertain hardware.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrqAffinityApplyError {
    /// No route or delivery write occurred. The preceding state remains valid.
    Rejected,
    /// Writes may have occurred. Request masking without claiming quiescence;
    /// do not restore delivery. KIRQ closes software admission until recovery.
    HardwareFailure,
}

/// Deferred routing operations for a validated, ordinary non-MSI source.
///
/// KIRQ owns the requested/effective/pending lifecycle. All callbacks run under
/// Line exclusion with ordinary IRQs disabled, outside NMI. They must be
/// bounded, non-sleeping, allocation-free and must not reenter KIRQ. Controller
/// locks nest below Line. The backend must not retain policy or a second pending
/// mask; KIRQ owns the request until the matching result.
pub struct IrqDeferredAffinityChip {
    /// Validates a nonempty interrupt-ready candidate set without writing hardware.
    ///
    /// Rejection must not modify the RTE or delivery state. Acceptance records
    /// no hardware progress; the old route remains usable.
    pub accept: fn(IrqDesc, &KCpuMask) -> Result<(), IrqAffinityApplyError>,

    /// Prepares the migration before the original completion.
    ///
    /// Edge may complete the whole transaction here; level masks and returns an
    /// opaque token for [`Self::finish`]. The token is valid only for the paired
    /// finish in the same dispatch, must not be allocated, and is never a policy
    /// mask. A `Pending` result means hardware is masked/prepared; `Complete`
    /// must contain only candidates actually established. No result proves old
    /// deliveries drained.
    ///
    /// KIRQ also invokes this with [`Self::finish`] from a disabled
    /// activation/recovery path where no real EOI exists. The backend must keep
    /// the source masked across that pair and must not assume an EOI occurred.
    pub prepare: fn(
        IrqDesc,
        &KCpuMask,
        IrqAffinityDelivery,
    ) -> Result<IrqAffinityPrepare, IrqAffinityApplyError>,

    /// Finishes a prepared level transaction after EOI.
    ///
    /// `Some(mask)` confirms the new destination; `None` means Remote IRR or
    /// another hardware condition requires the pending request to remain.
    /// Rejection or `None` must restore the incoming mask; failure must leave
    /// the source at least masked where possible and must not claim rollback.
    /// The token comes only from the immediately preceding `prepare`.
    pub finish:
        fn(IrqDesc, usize, IrqAffinityDelivery) -> Result<Option<KCpuMask>, IrqAffinityApplyError>,
}

/// Result of the pre-EOI deferred preparation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrqAffinityPrepare {
    /// The transaction completed before EOI and this is the effective target.
    Complete(KCpuMask),
    /// The transaction is masked/prepared; finish after EOI with this opaque token.
    Pending(usize),
}

/// Permanent operations for a validated, ordinary non-MSI source.
///
/// Called under Line exclusion, with ordinary IRQs disabled, outside NMI.
/// Operations are bounded, allocation-free, non-sleeping and cannot reenter
/// KIRQ. Controller locks nest below Line. The provider must validate actual
/// controller reachability and destination encoding before any hardware write.
/// This immediate contract does not queue work. Controllers requiring real
/// ACK/EOI migration expose [`IrqDeferredAffinityChip`] separately; KIRQ owns
/// that pending lifecycle.
pub struct IrqAffinityChip {
    /// Applies a nonempty interrupt-ready candidate set. Success returns a
    /// nonempty subset actually established by the controller. It preserves
    /// trigger, priority, action ownership and the requested delivery constraint.
    /// KeepMasked success must verify suppression even after a preceding failure.
    /// Preserve success restores only the actual incoming enable state, not a
    /// guessed logical enable flag. Original claims still complete at their
    /// original controller/CPU; success is neither drain nor hardware inactivity.
    /// Later ordinary configure calls must preserve this established route.
    pub apply:
        fn(IrqDesc, &KCpuMask, IrqAffinityDelivery) -> Result<KCpuMask, IrqAffinityApplyError>,
}

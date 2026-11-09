// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Immediate controller routing transactions; deferred migration is not implied.

use kcpu_id_map::KCpuMask;

use crate::IrqDesc;

/// Delivery constraint for an immediate routing transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrqAffinityDelivery {
    /// Preserve actual hardware enablement, including temporary flow masking.
    Preserve,
    /// Establish suppression and keep it on success, including fault recovery.
    KeepMasked,
}

/// A controller must distinguish pre-write rejection from uncertain hardware.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrqAffinityApplyError {
    /// No route or delivery write occurred. The preceding state remains valid.
    Rejected,
    /// Writes may have occurred. Request masking without claiming quiescence;
    /// do not restore delivery. KIRQ closes software admission until recovery.
    HardwareFailure,
}

/// Permanent operations for a validated, ordinary non-MSI source.
///
/// Called under Line exclusion, with ordinary IRQs disabled, outside NMI.
/// Operations are bounded, allocation-free, non-sleeping and cannot reenter
/// KIRQ. Controller locks nest below Line. The provider must validate actual
/// controller reachability and destination encoding before any hardware write.
/// Controllers requiring real ACK/EOI migration must return no capability;
/// this contract does not queue work or promise deferred progress.
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

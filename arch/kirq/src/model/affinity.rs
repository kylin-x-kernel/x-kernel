// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Runtime routing observations, distinct from firmware affinity metadata.

use kcpu_id_map::KCpuMask;

use crate::IrqDescError;

/// One locked observation of routing policy and confirmed controller state.
/// None means no route has been established by this API, or confirmation was
/// lost after a programming failure. It never means all present CPUs.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IrqAffinitySnapshot {
    /// Last successfully applied policy, including requested CPUs not yet ready.
    pub requested: Option<KCpuMask>,
    /// Confirmed destinations, not a promise that old deliveries have drained.
    pub effective: Option<KCpuMask>,
    /// Hardware may have changed; enable and ordinary flow admission are closed.
    pub has_failed_programming: bool,
    /// A request has been accepted but its effective routing is not yet confirmed.
    ///
    /// `requested` is the policy target; there is no separate pending mask.
    pub is_pending: bool,
}

/// Failure of a lookup-only ordinary IRQ routing operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrqAffinityError {
    /// Context, identity or lifecycle validation failed before routing changes.
    Management(IrqDescError),
    /// This source has no supported immediate routing operation.
    Unsupported,
    /// Descriptor affinity belongs to the source owner, not runtime reassignment.
    OwnerFixed,
    /// A source owner refuses user-originated routing changes.
    ///
    /// This is distinct from `OwnerFixed`: trusted kernel routing may remain
    /// available, and the current affinity snapshot remains observable.
    UserRestricted,
    /// The request contains no interrupt-ready CPU.
    NoReadyCpu,
    /// The controller rejected the target before changing hardware.
    Rejected,
    /// Programming failed after possible mutation; effective routing is unknown.
    HardwareFailure,
    /// Disable the faulted source before retrying its routing transaction.
    RecoveryRequiresDisable,
}

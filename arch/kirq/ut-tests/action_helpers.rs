// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Private fixture adapters; production prepares publication before Line locks.
//!
//! Removal helpers require exclusive fixture control: no live action snapshot
//! or concurrent dispatch/membership mutation. Synthetic running state and
//! private queued replays are allowed only before they have captured actions;
//! later traversal captures the post-withdrawal chain. These helpers deliberately
//! do not drain running state that only the calling fixture can finish.

use super::*;

pub(in crate::runtime) fn register_locked(
    virq: Virq,
    entry: &mut IrqStateDesc,
    handler: &Handler,
    options: IrqRegisterOptions,
    apply_plan: impl FnMut(IrqPlatformPlan) -> Result<(), IrqDescError>,
) -> Result<bool, IrqDescError> {
    let mut prepared = PreparedAction::regular(handler.clone())?;
    register_prepared_locked(virq, entry, &mut prepared, options, apply_plan)
}

pub(in crate::runtime) fn register_shared_locked(
    virq: Virq,
    entry: &mut IrqStateDesc,
    handler: &Handler,
    apply_plan: impl FnMut(IrqPlatformPlan) -> Result<(), IrqDescError>,
) -> Result<Option<IrqActionToken>, IrqDescError> {
    let mut prepared = PreparedAction::shared(handler.clone())?;
    register_shared_prepared_locked(virq, entry, &mut prepared, apply_plan)
}

pub(in crate::runtime) fn withdraw_on_line(
    virq: Virq,
    line: &Arc<IrqLine>,
    token: Option<IrqActionToken>,
    apply_plan: impl FnOnce(IrqPlatformPlan),
) -> Option<IrqAction> {
    let retired = withdraw_action_on_line(virq, line, token, false, apply_plan).unwrap()?;
    // SAFETY: audited callers exclusively drive synthetic Lines and have no
    // live snapshots. Any retained running state is synthetic, not a reader;
    // future snapshots cannot reach the detached node. The Line lock is gone.
    Some(unsafe { retired.reclaim() })
}

/// Removes membership only, for a private, exclusively borrowed state fixture.
pub(in crate::runtime) fn take_from_state(
    entry: &mut IrqStateDesc,
    token: Option<IrqActionToken>,
) -> Option<IrqAction> {
    let retired = entry.withdraw_action(token)?;
    // SAFETY: callers supply private state, not a locked Line guard, and have
    // never captured a live snapshot of the removed node. Exclusive fixture
    // ownership excludes concurrent readers; later snapshots see its removal.
    Some(unsafe { retired.reclaim() })
}

/// Removes fixture membership without masking or installing a teardown credit.
pub(in crate::runtime) fn take_from_line(
    line: &IrqLine,
    token: Option<IrqActionToken>,
) -> Option<IrqAction> {
    let retired = line.lock().withdraw_action(token)?;
    // SAFETY: callers exclusively control this fixture's readers. Any previous
    // traversal has ended; synthetic running state/private queued replay have not
    // captured the removed node. Replay after withdrawal sees the new chain.
    // The temporary Line guard ended before this allocation is reclaimed.
    Some(unsafe { retired.reclaim() })
}

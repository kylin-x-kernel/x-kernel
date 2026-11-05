// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Private fixture adapters; production prepares publication before Line locks.

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
    let actions = line.lock().action_list();
    actions
        .with_prepared_take(|prepared| {
            let retired = withdraw_prepared_on_line(virq, line, token, prepared, apply_plan)?;
            // SAFETY: the fixture calls from a sleepable task. The production short
            // transaction has returned and no reader/progress lock remains held.
            unsafe { retired.reclaim() }.map(ActionRecord::into_action)
        })
        .expect("fixture retirement preparation")
}

/// Removes membership only, for a private, exclusively borrowed state fixture.
pub(in crate::runtime) fn take_from_state(
    entry: &mut IrqStateDesc,
    token: Option<IrqActionToken>,
) -> Option<IrqAction> {
    let actions = entry.action_list();
    actions
        .with_prepared_take(|prepared| {
            let retired = entry.withdraw_action(prepared, token)?;
            // SAFETY: fixtures pass private state, not a locked Line guard, and
            // hold no read scope. Withdrawal has released the list writer lock.
            unsafe { retired.reclaim() }.map(ActionRecord::into_action)
        })
        .expect("fixture retirement preparation")
}

/// Removes fixture membership without masking or installing a teardown credit.
pub(in crate::runtime) fn take_from_line(
    line: &IrqLine,
    token: Option<IrqActionToken>,
) -> Option<IrqAction> {
    let actions = line.lock().action_list();
    actions
        .with_prepared_take(|prepared| {
            let retired = line.lock().withdraw_action(prepared, token)?;
            // SAFETY: fixture task prepared outside exclusion; the temporary Line
            // guard and list writer lock have been released before the wait.
            unsafe { retired.reclaim() }.map(ActionRecord::into_action)
        })
        .expect("fixture retirement preparation")
}

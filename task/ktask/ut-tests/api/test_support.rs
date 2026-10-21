// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

/// Reads the initialized current task's pending reschedule flag.
pub fn is_current_reschedule_pending() -> bool {
    super::current().is_preempt_pending()
}

/// Sets the current task's pending flag without checking it or notifying a CPU.
///
/// Fixtures must exclude competing flag writers while replacing/restoring
/// the flag, so they do not discard an unrelated scheduling request.
pub fn set_current_reschedule_pending(pending: bool) {
    super::current().set_preempt_pending(pending);
}

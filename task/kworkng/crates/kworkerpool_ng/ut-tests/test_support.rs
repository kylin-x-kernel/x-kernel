// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Controlled provider fixtures; no global service installation.

use alloc::sync::Arc;
use core::{num::NonZeroUsize, task::Waker};

use kcpu_id_map::LogicalCpuId;

use crate::{BottomHalfService, PartitionDispatch, SourceRemainder};

/// Holds a queued source inspection while the test schedules concurrent teardown.
///
/// No execution is acquired. The body runs unlocked and must return normally.
///
/// # Panics
/// Panics if no source is queued or the body panics.
pub fn with_source_inspection<S, R>(
    dispatch: &PartitionDispatch<S>,
    body: impl FnOnce() -> R,
) -> R {
    let inspection = dispatch
        .begin_inspection()
        .expect("test needs queued source");
    let result = body();
    inspection.finish(SourceRemainder::Empty);
    result
}

/// Binds a test-local BH dispatcher and continuation waker without CPU validation.
///
/// The test must serialize passes, install a real softirq action, retain its
/// storage and provide an IRQ-safe waker that never invokes callbacks inline.
/// An absent CPU is allowed only to exercise run-pass rejection.
pub fn bottom_half_service<S>(
    cpu: LogicalCpuId,
    budget: NonZeroUsize,
    dispatch: Arc<PartitionDispatch<S>>,
    kick: Arc<Waker>,
) -> BottomHalfService<S> {
    BottomHalfService::with_dispatch(cpu, budget, dispatch, kick)
}

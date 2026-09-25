// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Payload-free acknowledgement of source inspection AND reference retirement.
//! Dispatch state stops new selection; this control tracks references still being
//! released outside that lock, which can overlap later inspections. It contains
//! no source, Queue or dispatcher reference, so final payload release can precede
//! acknowledgement. Lock order is dispatch -> this control, never the reverse.

use alloc::sync::Arc;

use kpoll::Completion;
use kspin::SpinNoIrq;

struct SourceDetachState {
    // Includes unlocked reference release after the inspection logic returns.
    outstanding_inspections: usize,
    // The first detach has removed and released the dispatcher's queued reference.
    is_detached: bool,
}

/// Completes source detachment after all inspection references have been retired.
/// This does not track acquired Work callbacks or publisher-owned references.
pub(super) struct SourceDetachment {
    state: SpinNoIrq<SourceDetachState>,
    completion: Arc<Completion>,
}

impl SourceDetachment {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: SpinNoIrq::new(SourceDetachState {
                outstanding_inspections: 0,
                is_detached: false,
            }),
            completion: Arc::new(Completion::new()),
        })
    }

    pub(super) fn completion(&self) -> Arc<Completion> {
        self.completion.clone()
    }

    /// Called with dispatch protection, before a selected source can escape.
    pub(super) fn begin_inspection(&self) {
        let mut state = self.state.lock();
        assert!(!state.is_detached, "inspection after source detachment");
        state.outstanding_inspections = state
            .outstanding_inspections
            .checked_add(1)
            .expect("inspection count overflow");
    }

    /// Called unlocked, after the inspection has returned/released its source Arc.
    pub(super) fn finish_inspection(&self) {
        let wake = {
            let mut state = self.state.lock();
            state.outstanding_inspections = state
                .outstanding_inspections
                .checked_sub(1)
                .expect("unaccounted inspection");
            (state.is_detached && state.outstanding_inspections == 0)
                .then(|| self.completion.complete_all_defer_wake())
        };
        if let Some(wake) = wake {
            wake.wake();
        }
    }

    /// Only the first detach transition owner calls this, after locked removal
    /// and unlocked reference release. Source state already forbids new begins.
    pub(super) fn mark_detached(&self) {
        let wake = {
            let mut state = self.state.lock();
            assert!(!state.is_detached, "duplicate source detachment");
            state.is_detached = true;
            (state.outstanding_inspections == 0).then(|| self.completion.complete_all_defer_wake())
        };
        if let Some(wake) = wake {
            wake.wake();
        }
    }
}

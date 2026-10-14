// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Flat coordination of node obligations, without hardware observation or waiters.

use core::sync::atomic::{Ordering, fence};

use kspin::{NoPreemptIrqSave, SpinNoIrq};

use crate::{PeriodId, ReaderNode};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Idle,
    Initializing,
    Waiting,
    Finalizing,
}

struct PeriodState<const CPUS: usize> {
    completed: PeriodId,
    phase: Phase,
    pending: [bool; CPUS],
}

/// The sole period controller for a fixed set of already admitted CPU nodes.
///
/// Nodes may still enroll/remove readers independently. No other controller may
/// drive their period controls. Initialization/finalization claims a phase under
/// the coordinator lock, then visits nodes without holding that lock. Reports
/// can run concurrently, but never from NMI. No method waits or runs callbacks.
///
/// Hardware integration must supply valid CPU observations and RCU-watched
/// report context. This internal protocol alone is not permission to reclaim.
pub(crate) struct GracePeriodCoordinator<'nodes, const CPUS: usize> {
    nodes: &'nodes [ReaderNode; CPUS],
    participants: [bool; CPUS],
    state: SpinNoIrq<PeriodState<CPUS>>,
}

impl<'nodes, const CPUS: usize> GracePeriodCoordinator<'nodes, CPUS> {
    /// Rejects an empty participant set; unused capacity does not owe reports.
    pub(crate) fn new(
        nodes: &'nodes [ReaderNode; CPUS],
        participants: [bool; CPUS],
    ) -> Option<Self> {
        if !participants.iter().any(|&is_participant| is_participant) {
            return None;
        }
        Some(Self {
            nodes,
            participants,
            state: SpinNoIrq::new(PeriodState {
                completed: 0,
                phase: Phase::Idle,
                pending: [false; CPUS],
            }),
        })
    }

    /// Starts a new period only while idle, publishing all obligations first.
    /// The returned identity is internal bookkeeping, not a public wait cookie.
    /// The driver must check completion after this returns: all reports may
    /// already have arrived while initialization was still in progress.
    pub(crate) fn begin_period(&self) -> Option<PeriodId> {
        let period = self.start_initialization()?;
        for (cpu, &is_participant) in self.participants.iter().enumerate() {
            if is_participant {
                assert!(
                    self.nodes[cpu].begin_period(period),
                    "RCU node still has a period before initialization"
                );
            }
        }
        self.finish_initialization(period);
        Some(period)
    }

    fn start_initialization(&self) -> Option<PeriodId> {
        let mut state = self.state.lock();
        // Order prior withdrawal before this period's node observations, even
        // when a UP lock provides exclusion without a hardware lock operation.
        fence(Ordering::SeqCst);
        if state.phase != Phase::Idle {
            return None;
        }
        state.pending = self.participants;
        state.phase = Phase::Initializing;
        Some(state.completed.wrapping_add(1))
    }

    fn finish_initialization(&self, period: PeriodId) {
        let mut state = self.state.lock();
        fence(Ordering::SeqCst);
        debug_assert_eq!(state.phase, Phase::Initializing);
        debug_assert_eq!(state.completed.wrapping_add(1), period);
        // Only begin_period's initializer can close this gate, after every
        // node has been initialized. Early reports do not change the phase.
        state.phase = Phase::Waiting;
    }

    /// Records a validated CPU boundary, not a locally inferred observation.
    /// The identity must belong to the observation being supplied, never a
    /// cached identity held across an unbounded delay or counter wrap.
    pub(crate) fn observe_cpu(&self, cpu: usize, period: PeriodId) -> bool {
        self.participants.get(cpu) == Some(&true) && self.nodes[cpu].observe_cpu(period)
    }

    /// Inspects a node afresh and submits its ready identity before unpinning.
    /// Caller must be in watched task/IRQ context, never NMI. Deferred work
    /// calls this method again rather than storing an earlier numeric report.
    pub(crate) fn report_node(&self, cpu: usize) -> bool {
        if self.participants.get(cpu) != Some(&true) {
            return false;
        }
        let _guard = NoPreemptIrqSave::new();
        let Some(period) = self.nodes[cpu].ready_period() else {
            return false;
        };
        // ready_period has released the node lock. Keep the watched IRQ-off
        // interval through report acceptance to bound identity lifetime.
        self.accept_report(cpu, period)
    }

    fn accept_report(&self, cpu: usize, period: PeriodId) -> bool {
        let mut state = self.state.lock();
        fence(Ordering::SeqCst);
        if !matches!(state.phase, Phase::Initializing | Phase::Waiting)
            || state.completed.wrapping_add(1) != period
            || state.pending.get(cpu) != Some(&true)
        {
            return false;
        }
        state.pending[cpu] = false;
        true
    }

    /// Finishes all participating nodes before publishing global completion.
    /// A competing initializer/finalizer cannot claim another phase meanwhile.
    pub(crate) fn try_complete_period(&self) -> Option<PeriodId> {
        let period = self.start_finalization()?;
        for (cpu, &is_participant) in self.participants.iter().enumerate() {
            if is_participant {
                assert!(
                    self.nodes[cpu].finish_period(period),
                    "RCU node reported ready before its obligations cleared"
                );
            }
        }
        self.finish_finalization(period);
        Some(period)
    }

    fn start_finalization(&self) -> Option<PeriodId> {
        let mut state = self.state.lock();
        fence(Ordering::SeqCst);
        if state.phase != Phase::Waiting || state.pending.iter().any(|&is_pending| is_pending) {
            return None;
        }
        state.phase = Phase::Finalizing;
        Some(state.completed.wrapping_add(1))
    }

    fn finish_finalization(&self, period: PeriodId) {
        let mut state = self.state.lock();
        // Node finalization precedes the completion seen by another CPU. A
        // callback driver must also latch eligibility before restarting.
        fence(Ordering::SeqCst);
        debug_assert_eq!(state.phase, Phase::Finalizing);
        debug_assert_eq!(state.completed.wrapping_add(1), period);
        state.completed = period;
        state.phase = Phase::Idle;
    }

    /// Reads ordered internal progress, not a wrap-safe predicate for waiters.
    pub(crate) fn completed_period(&self) -> PeriodId {
        let state = self.state.lock();
        fence(Ordering::SeqCst);
        state.completed
    }
}

#[cfg(any(test, unittest))]
#[path = "../ut-tests/coordinator.rs"]
mod tests;

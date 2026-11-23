// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel-lifetime prepared network work, with serialized foreground assistance.

use alloc::vec::Vec;

use kspin::SpinNoIrq;
use kworkng::{CpuPolicy, NormalQueue, QueueConfig, SubmitError, Work};

use super::{ASSIST_BUDGET, BACKGROUND_BUDGET, BACKGROUND_MAX_ROUNDS, PollBudget, PollReason};

pub(crate) struct NetworkPoller {
    work: Work<fn()>,
    state: SpinNoIrq<PollState>,
    #[cfg(unittest)]
    poll_once: fn(PollBudget) -> super::PollProgress,
}

struct PollState {
    queue: Option<NormalQueue>,
    is_running: bool,
    has_pending: bool,
}

impl PollState {
    fn claim(&mut self) -> bool {
        if self.is_running || !self.has_pending {
            return false;
        }
        self.is_running = true;
        self.has_pending = false;
        true
    }

    fn finish(&mut self, has_more: bool) -> bool {
        self.has_pending |= has_more;
        self.is_running = false;
        self.has_pending
    }
}

impl NetworkPoller {
    pub(super) fn new() -> Self {
        Self {
            work: Work::with_callback(run_background as fn()),
            state: SpinNoIrq::new(PollState {
                queue: None,
                is_running: false,
                has_pending: false,
            }),
            #[cfg(unittest)]
            poll_once: super::poll_network_once,
        }
    }

    /// Prepares the private queue before network producers start, in sleepable,
    /// reclaim-safe task context without locks needed by reclaim, IO or filesystems.
    pub(crate) fn start(&'static self) {
        if self.state.lock().queue.is_some() {
            return;
        }
        let mut cpus = Vec::new();
        kcpu_id_map::for_each_present_logical_cpu(|_, cpu, _| cpus.push(cpu));
        let config = match QueueConfig::new(&cpus, CpuPolicy::CurrentCpu, 1) {
            Ok(config) => config,
            Err(error) => {
                warn!("failed to prepare NG knet-poller configuration: {error:?}");
                return;
            }
        };
        let queue = match NormalQueue::new("knet-poller", config) {
            Ok(queue) => queue,
            Err(error) => {
                warn!("failed to start NG knet-poller: {error:?}");
                return;
            }
        };
        {
            let mut state = self.state.lock();
            if state.queue.is_none() {
                state.queue = Some(queue.clone());
            }
        }
        // An early network notification is demand owned by this poller, not an
        // accepted Work occurrence before queue readiness. Replay it now.
        self.kick();
    }

    /// Records demand without allocation or executing the network data plane.
    pub(crate) fn notify(&'static self, _: PollReason) {
        self.state.lock().has_pending = true;
        self.kick();
    }

    /// Competes for one scheduled round without waiting for another executor.
    pub(crate) fn assist_once(&'static self) {
        self.run(ASSIST_BUDGET, 1, false);
    }

    /// Records task-context demand and assists before submitting background work.
    ///
    /// An existing executor schedules follow-up work when it finishes. Otherwise,
    /// this round submits unfinished work and concurrent notifications to the queue.
    pub(crate) fn notify_and_assist(&'static self, _: PollReason) {
        self.run(ASSIST_BUDGET, 1, true);
    }

    fn kick(&'static self) {
        let queue = {
            let state = self.state.lock();
            if state.is_running || !state.has_pending {
                return;
            }
            state.queue.clone()
        };
        let Some(queue) = queue else { return };
        match queue.queue_static_work(&self.work, None) {
            Ok(_) | Err(SubmitError::AlreadyQueued) => {}
            Err(error) => warn!("NG network polling admission failed: {error:?}"),
        }
    }

    fn run(&'static self, budget: PollBudget, rounds: usize, has_new_demand: bool) {
        if !self.try_claim(has_new_demand) {
            return;
        }
        let mut has_more = false;
        for _ in 0..rounds {
            #[cfg(unittest)]
            let progress = (self.poll_once)(budget);
            #[cfg(not(unittest))]
            let progress = super::poll_network_once(budget);
            has_more = progress.has_more;
            if !has_more {
                break;
            }
        }
        let needs_followup = self.state.lock().finish(has_more);
        if needs_followup {
            self.kick();
        }
    }

    fn try_claim(&self, has_new_demand: bool) -> bool {
        let mut state = self.state.lock();
        state.has_pending |= has_new_demand;
        state.queue.is_some() && state.claim()
    }
}

fn run_background() {
    super::network_poller().run(BACKGROUND_BUDGET, BACKGROUND_MAX_ROUNDS, false);
    #[cfg(unittest)]
    BACKGROUND_RUNS.fetch_add(1, core::sync::atomic::Ordering::Release);
}

#[cfg(unittest)]
static BACKGROUND_RUNS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

unittest::path_mod_test!(tests, "../../../ut-tests/runtime/poller/worker/tests.rs");

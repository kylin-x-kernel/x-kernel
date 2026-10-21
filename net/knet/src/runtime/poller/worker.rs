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
        }
    }

    /// Prepares the private queue in task context before network producers start.
    pub(crate) fn start(&'static self) {
        if self.state.lock().queue.is_some() {
            return;
        }
        let mut cpus = Vec::new();
        kcpu_id_map::for_each_present_logical_cpu(|_, cpu, _| cpus.push(cpu));
        let config = QueueConfig::new(&cpus, CpuPolicy::CurrentCpu, 1)
            .expect("network uses the complete initialized boot CPU set");
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
        self.run(ASSIST_BUDGET, 1);
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

    fn run(&'static self, budget: PollBudget, rounds: usize) {
        {
            let mut state = self.state.lock();
            if state.queue.is_none() || !state.claim() {
                return;
            }
        }
        let mut has_more = false;
        for _ in 0..rounds {
            has_more = super::poll_network_once(budget).has_more;
            if !has_more {
                break;
            }
        }
        let needs_followup = self.state.lock().finish(has_more);
        if needs_followup {
            self.kick();
        }
    }
}

fn run_background() {
    super::network_poller().run(BACKGROUND_BUDGET, BACKGROUND_MAX_ROUNDS);
    #[cfg(unittest)]
    BACKGROUND_RUNS.fetch_add(1, core::sync::atomic::Ordering::Release);
}

#[cfg(unittest)]
static BACKGROUND_RUNS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

unittest::path_mod_test!(tests, "../../../ut-tests/runtime/poller/worker/tests.rs");

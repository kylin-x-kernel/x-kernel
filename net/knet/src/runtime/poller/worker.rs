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

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::PollState;

    /// IRQ-side requests reach the boot-bound reusable network Work.
    ///
    /// # Panics
    /// Panics if startup omitted the private queue or notifications make no progress.
    #[def_test(serial)]
    fn boot_network_poller_handles_irq_notifications() {
        use core::sync::atomic::Ordering;

        use ktime_types::TimeSpan;

        let poller = super::super::network_poller();
        assert!(poller.state.lock().queue.is_some());
        for _ in 0..2 {
            let before = super::BACKGROUND_RUNS.load(Ordering::Acquire);
            {
                let _irq = kspin::NoPreemptIrqSave::new();
                for _ in 0..16 {
                    poller.notify(super::PollReason::Rx);
                }
            }
            let limit = khal::time::monotonic_time() + TimeSpan::from_secs(2);
            while super::BACKGROUND_RUNS.load(Ordering::Acquire) == before
                && khal::time::monotonic_time() < limit
            {
                ktask::sleep(TimeSpan::from_millis(1));
            }
            assert!(super::BACKGROUND_RUNS.load(Ordering::Acquire) > before);
        }
        // Do not cancel the kernel-lifetime poller: network producers stay live.
    }

    /// Concurrent demand survives completion; a second executor cannot claim.
    ///
    /// # Panics
    /// Panics if running demand is lost or two executors can own the poller.
    #[def_test]
    fn notifications_during_poll_require_another_round() {
        let mut state = PollState {
            queue: None,
            is_running: false,
            has_pending: true,
        };
        assert!(state.claim());
        state.has_pending = true;
        assert!(!state.claim());
        assert!(state.finish(false));
        assert!(state.claim());
        assert!(!state.finish(false));
        assert!(!state.claim());
    }

    /// Exhausted budget preserves demand even without another IRQ notification.
    ///
    /// # Panics
    /// Panics if budget exhaustion cannot schedule a continuation.
    #[def_test]
    fn budget_exhaustion_preserves_pending_demand() {
        let mut state = PollState {
            queue: None,
            is_running: false,
            has_pending: true,
        };
        assert!(state.claim());
        assert!(state.finish(true));
        assert!(state.claim());
        assert!(!state.finish(false));
        state.has_pending = true;
        assert!(state.claim());
    }
}

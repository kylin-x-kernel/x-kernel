// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Budgeted network data-plane poller.

use klazy::lazy_static;

use crate::SERVICE;

const BACKGROUND_BUDGET: PollBudget = PollBudget {
    rx_packets: 512,
    tx_packets: 256,
    timer_events: 32,
};

const ASSIST_BUDGET: PollBudget = PollBudget {
    rx_packets: 16,
    tx_packets: 16,
    timer_events: 8,
};

const BACKGROUND_MAX_ROUNDS: usize = 4;

lazy_static! {
    static ref NETWORK_POLLER: NetworkPoller = NetworkPoller::new();
}

#[derive(Clone, Copy)]
pub(crate) enum PollReason {
    Rx,
    RxWindow,
    Tx,
    Timer,
}

/// The budget for a single network polling round.
#[derive(Clone, Copy)]
pub(crate) struct PollBudget {
    pub rx_packets: usize,
    pub tx_packets: usize,
    pub timer_events: usize,
}

/// The progress made in a single network polling round.
#[derive(Default)]
pub(crate) struct PollProgress {
    pub rx_packets: usize,
    pub tx_packets: usize,
    pub timer_events: usize,
    /// Whether work is ready for immediate processing in another round.
    ///
    /// A future protocol timer deadline does not count as immediate work.
    pub has_more: bool,
}

pub(crate) struct NetworkPoller {
    poller: kwork::BudgetedPoller<PollBudget, fn(PollBudget) -> kwork::BudgetedPollProgress>,
}

impl NetworkPoller {
    fn new() -> Self {
        Self {
            poller: kwork::BudgetedPoller::new(
                "knet-poller",
                BACKGROUND_BUDGET,
                ASSIST_BUDGET,
                BACKGROUND_MAX_ROUNDS,
                poll_network_once,
            ),
        }
    }

    /// Starts the background workqueue during network initialization.
    pub(crate) fn start(&self) {
        if let Err(err) = self.poller.start() {
            warn!("failed to start knet-poller workqueue: {err:?}");
        }
    }

    /// Publishes network work and queues the background worker when it is idle.
    ///
    /// This method may run in IRQ-adjacent paths and does not acquire a sleepable
    /// lock or execute the network data plane.
    pub(crate) fn notify(&self, reason: PollReason) {
        let _ = reason;
        let _ = self.poller.notify_irq_safe();
    }

    /// Assists one already scheduled round from the current task context.
    ///
    /// The call does not publish new work or wait for another executor.
    pub(crate) fn assist_once(&self) {
        self.poller.assist_once();
    }
}

pub(crate) fn network_poller() -> &'static NetworkPoller {
    &NETWORK_POLLER
}

fn poll_network_once(budget: PollBudget) -> kwork::BudgetedPollProgress {
    if !SERVICE.is_inited() {
        return kwork::BudgetedPollProgress::default();
    }

    let progress = SERVICE.poll_budgeted(budget);
    debug_assert!(progress.rx_packets <= budget.rx_packets);
    debug_assert!(progress.tx_packets <= budget.tx_packets);
    debug_assert!(progress.timer_events <= budget.timer_events);

    kwork::BudgetedPollProgress {
        has_more: progress.has_more,
    }
}

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

mod worker;
use worker::NetworkPoller;

pub(crate) fn network_poller() -> &'static NetworkPoller {
    &NETWORK_POLLER
}

fn poll_network_once(budget: PollBudget) -> PollProgress {
    if !SERVICE.is_inited() {
        return PollProgress::default();
    }

    let progress = SERVICE.poll_budgeted(budget);
    debug_assert!(progress.rx_packets <= budget.rx_packets);
    debug_assert!(progress.tx_packets <= budget.tx_packets);
    debug_assert!(progress.timer_events <= budget.timer_events);

    progress
}

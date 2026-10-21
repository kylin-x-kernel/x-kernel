// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::AtomicUsize;

use super::*;
use crate::ReaderNode;

#[path = "allocation.rs"]
mod allocation;
#[path = "../support/reposting.rs"]
mod reposting;
use reposting::SelfReposting;

fn execute(queue: &CallbackQueue, budget: usize) -> usize {
    // SAFETY: deterministic fixtures retire no published data and hold no RCU
    // locks. Their owned callbacks use nonblocking operations and real queue/GP
    // transitions, with explicitly supplied simulated CPU observations.
    unsafe { queue.invoke_ready(budget) }
}

fn observe<const CPUS: usize>(gp: &GracePeriodCoordinator<'_, CPUS>, period: PeriodId) -> bool {
    let mut observed = true;
    for cpu in 0..CPUS {
        observed &= gp.observe_cpu(cpu, period);
        let _ = gp.report_node(cpu);
    }
    observed
}

#[test]
fn repost_completes_without_allocations() {
    let nodes = [ReaderNode::new()];
    let gp = GracePeriodCoordinator::new(&nodes, [true]).unwrap();
    let queues = Arc::new([CallbackQueue::new()]);
    // SAFETY: fresh isolated coordinator/queues, controlled only by this driver.
    // All three invocations are drained before storage is released.
    let mut driver = unsafe { CallbackDriver::new(&gp, &queues) };
    let calls = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let prepared = Box::new(SelfReposting {
        node: CallbackNode::new(SelfReposting::invoke),
        queues: queues.clone(),
        calls: calls.clone(),
        drops: drops.clone(),
        remaining: 2,
    });

    let run_fn = || {
        prepared.enqueue();
        for period in 1..=3 {
            assert_eq!(execute(&queues[0], 8), 0);
            assert_eq!(driver.drive(), Some(period));
            // Merely starting the successor GP cannot make the repost eligible.
            assert_eq!(execute(&queues[0], 8), 0);
            assert!(observe(&gp, period));
            assert_eq!(driver.drive(), None);
            assert_eq!(execute(&queues[0], 8), 1);
            assert_eq!(calls.load(Ordering::Relaxed), usize::from(period));
            let has_repost = period < 3;
            assert_eq!(queues[0].outstanding(), usize::from(has_repost));
            assert_eq!(drops.load(Ordering::Relaxed), usize::from(!has_repost));
        }
    };
    allocation::without_allocations(run_fn);
}

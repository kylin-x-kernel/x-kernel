// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Predicate waits are checked through notification delivery, not elapsed time.
//! The deadlines only bound waits; no assertion requires a particular latency.

use super::*;

#[unittest::def_test(serial)]
fn timed_wait_rechecks_ack_published_before_listener_registration() {
    let queue = WaitQueue::new();
    let mut has_acknowledgement = false;
    let has_timed_out = queue.wait_timeout_until(TimeSpan::from_secs(5), || {
        let observed = has_acknowledgement;
        if !observed {
            // Model an ACK arriving after the condition load but before listening.
            has_acknowledgement = true;
            assert!(!queue.notify_one(false));
        } else {
            // This private queue has no other users. Delivery proves that the ACK
            // is rechecked with a listener registered, not after its timeout/drop.
            assert!(queue.notify_one(false));
        }
        observed
    });
    assert!(!has_timed_out);
    // No additional notification is deliverable after the wait returns.
    assert!(!queue.notify_one(false));
}

#[unittest::def_test(serial)]
fn timed_wait_preserves_ack_and_timeout_results() {
    let queue = WaitQueue::new();
    assert!(!queue.wait_timeout_until(TimeSpan::from_secs(5), || true));
    assert!(queue.wait_timeout_until(TimeSpan::from_millis(1), || false));
}

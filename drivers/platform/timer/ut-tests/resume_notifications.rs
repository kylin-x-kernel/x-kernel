// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::ResumeNotifications;

#[def_test]
fn peers_consume_independent_requests() {
    let requests = ResumeNotifications::<4>::new();
    assert!(requests.mark_peers(3, 1));
    assert!(!requests.take(1));
    assert!(requests.take(0));
    assert!(!requests.take(0));
    assert!(requests.take(2));
    assert!(!requests.take(3));
}

#[def_test]
fn cpu_ids_beyond_machine_word_are_not_lost() {
    let requests = ResumeNotifications::<130>::new();
    assert!(requests.mark_peers(130, 129));
    for cpu in 0..129 {
        assert!(requests.take(cpu));
        assert!(!requests.take(cpu));
    }
    assert!(!requests.take(129));
    assert!(requests.mark_peers(130, 64));
    assert!(!requests.take(64));
    assert!(requests.take(129));
}

#[def_test]
fn repeated_requests_coalesce_until_consumed() {
    let requests = ResumeNotifications::<2>::new();
    assert!(requests.mark_peers(2, 0));
    assert!(requests.mark_peers(2, 0));
    assert!(requests.take(1));
    assert!(!requests.take(1));
    assert!(requests.mark_peers(2, 0));
    assert!(requests.take(1));
}

#[def_test]
fn publication_after_consumption_remains_pending() {
    let requests = ResumeNotifications::<130>::new();
    for _ in 0..1_000 {
        assert!(requests.mark_peers(130, 0));
        assert!(requests.take(129));
        assert!(!requests.take(129));
        // Publish after consumption, then consume another CPU's request.
        // That consumption must not clear CPU 129's newly published flag.
        assert!(requests.mark_peers(130, 0));
        assert!(requests.take(64));
        assert!(requests.take(129));
        assert!(!requests.take(129));
    }
}

#[def_test]
fn invalid_topology_does_not_change_pending_requests() {
    let requests = ResumeNotifications::<2>::new();
    assert!(requests.mark_peers(2, 0));
    assert!(!requests.mark_peers(3, 0));
    assert!(!requests.mark_peers(2, 2));
    assert!(!requests.take(usize::MAX));
    assert!(!requests.take(0));
    assert!(requests.take(1));
}

#[def_test]
fn single_cpu_needs_no_remote_notification() {
    let requests = ResumeNotifications::<1>::new();
    assert!(!requests.mark_peers(1, 0));
    assert!(!requests.take(0));
}

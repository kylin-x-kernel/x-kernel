// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;
use crate::{process_exit, wait_reap};

#[def_test]
fn test_ioevents_constants() {
    let events = IoEvents::IN;
    assert!(events.contains(IoEvents::IN));
}

/// A pidfd keeps naming its process until the child is reaped: the Linux
/// `pid_has_task` gate that pidfd fdinfo relies on.
#[def_test(serial)]
fn test_has_attached_task_survives_exit_until_reap() {
    let parent = Process::new_init(9100);
    let proc = parent.fork(9101);
    assert!(
        proc.has_attached_task(),
        "running process must still have its task attached"
    );

    process_exit::finalize_process_exit(&proc);
    assert!(
        proc.has_attached_task(),
        "a waitable zombie must keep its task attached, as Linux pid_has_task does"
    );

    wait_reap::assert_reap_zombie_process(&proc);
    assert!(
        !proc.has_attached_task(),
        "a reaped process must report no attached task"
    );
}

#[def_test(serial)]
fn test_pidfd_poll_becomes_readable_after_exit() {
    // Keep the parent alive even when another test already initialized global init.
    let parent = Process::new_init(9000);
    let proc = parent.fork(9001);
    let pidfd = PidFd::new(&proc);

    assert!(
        !pidfd.poll().contains(IoEvents::IN),
        "live process must not make pidfd readable"
    );

    process_exit::finalize_process_exit(&proc);

    assert!(
        pidfd.poll().contains(IoEvents::IN),
        "exited zombie must make pidfd readable"
    );

    wait_reap::assert_reap_zombie_process(&proc);
}

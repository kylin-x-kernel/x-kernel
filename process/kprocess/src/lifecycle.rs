// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Lifecycle state shared by all threads in a process.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use kpoll::PollSet;

/// Process lifecycle state shared by all threads in a process.
pub(crate) struct ProcessLifecycleState {
    child_exit_event: Arc<PollSet>,
    exit_event: Arc<PollSet>,
    /// Accumulated user-mode nanoseconds of exited threads in this process.
    exited_thread_utime_ns: AtomicUsize,
    /// Accumulated kernel-mode nanoseconds of exited threads in this process.
    exited_thread_stime_ns: AtomicUsize,
    /// Accumulated user-mode nanoseconds of reaped children.
    child_utime_ns: AtomicUsize,
    /// Accumulated kernel-mode nanoseconds of reaped children.
    child_stime_ns: AtomicUsize,
}

impl Default for ProcessLifecycleState {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessLifecycleState {
    /// Creates a new [`ProcessLifecycleState`].
    pub(crate) fn new() -> Self {
        Self {
            child_exit_event: Arc::default(),
            exit_event: Arc::default(),
            exited_thread_utime_ns: AtomicUsize::new(0),
            exited_thread_stime_ns: AtomicUsize::new(0),
            child_utime_ns: AtomicUsize::new(0),
            child_stime_ns: AtomicUsize::new(0),
        }
    }

    /// Returns the child-exit wait event.
    pub(crate) fn child_exit_event(&self) -> &Arc<PollSet> {
        &self.child_exit_event
    }

    /// Returns the process-exit event.
    pub(crate) fn exit_event(&self) -> &Arc<PollSet> {
        &self.exit_event
    }

    /// Adds exited-thread CPU time to the accumulated counters.
    pub(crate) fn accumulate_exited_thread_time(&self, utime_ns: usize, stime_ns: usize) {
        self.exited_thread_utime_ns
            .fetch_add(utime_ns, Ordering::Relaxed);
        self.exited_thread_stime_ns
            .fetch_add(stime_ns, Ordering::Relaxed);
    }

    /// Returns accumulated exited-thread user and kernel time in nanoseconds.
    pub(crate) fn exited_thread_time_ns(&self) -> (usize, usize) {
        (
            self.exited_thread_utime_ns.load(Ordering::Relaxed),
            self.exited_thread_stime_ns.load(Ordering::Relaxed),
        )
    }

    /// Adds CPU time from a child reaped via `wait*()` to the accumulated
    /// counters.
    ///
    /// Mirrors Linux `kernel/exit.c`, where the parent's child CPU totals are
    /// incremented by the reaped thread-group time plus the child's own
    /// accumulated descendant totals.
    pub(crate) fn accumulate_child_time(&self, utime_ns: usize, stime_ns: usize) {
        self.child_utime_ns.fetch_add(utime_ns, Ordering::Relaxed);
        self.child_stime_ns.fetch_add(stime_ns, Ordering::Relaxed);
    }

    /// Returns accumulated reaped-children user and kernel time in nanoseconds.
    pub(crate) fn child_time_ns(&self) -> (usize, usize) {
        (
            self.child_utime_ns.load(Ordering::Relaxed),
            self.child_stime_ns.load(Ordering::Relaxed),
        )
    }
}

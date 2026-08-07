// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Lifecycle state shared by all threads in a process.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use kpoll::{Completion, PollSet};
use ktime_types::TimeSpan;

/// Process lifecycle state shared by all threads in a process.
pub(crate) struct ProcessLifecycleState {
    events: ProcessEvents,
    cpu_totals: ProcessCpuTotals,
}

struct ProcessEvents {
    child_exit_event: Arc<PollSet>,
    exit_event: Arc<Completion>,
}

struct ProcessCpuTotals {
    /// Accumulated user-mode nanoseconds of exited threads in this process.
    exited_thread_utime_ns: AtomicU64,
    /// Accumulated kernel-mode nanoseconds of exited threads in this process.
    exited_thread_stime_ns: AtomicU64,
    /// Accumulated user-mode nanoseconds of reaped children.
    child_utime_ns: AtomicU64,
    /// Accumulated kernel-mode nanoseconds of reaped children.
    child_stime_ns: AtomicU64,
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
            events: ProcessEvents::new(),
            cpu_totals: ProcessCpuTotals::new(),
        }
    }

    /// Returns the child-exit wait event.
    pub(crate) fn child_exit_event(&self) -> &Arc<PollSet> {
        self.events.child_exit_event()
    }

    /// Wakes waiters that are blocked on a child process becoming waitable.
    pub(crate) fn notify_child_exit(&self) {
        self.events.notify_child_exit();
    }

    /// Returns the process-exit event.
    pub(crate) fn exit_event(&self) -> &Arc<Completion> {
        self.events.exit_event()
    }

    /// Wakes waiters that are blocked on this process exiting.
    pub(crate) fn notify_exit(&self) {
        self.events.notify_exit();
    }

    /// Adds exited-thread CPU time to the accumulated counters.
    pub(crate) fn accumulate_exited_thread_time(&self, utime: TimeSpan, stime: TimeSpan) {
        self.cpu_totals.accumulate_exited_thread_time(utime, stime);
    }

    /// Returns accumulated exited-thread user and kernel time.
    pub(crate) fn exited_thread_time(&self) -> (TimeSpan, TimeSpan) {
        self.cpu_totals.exited_thread_time()
    }

    /// Adds CPU time from a child reaped via `wait*()` to the accumulated
    /// counters.
    ///
    /// Mirrors Linux `kernel/exit.c`, where the parent's child CPU totals are
    /// incremented by the reaped thread-group time plus the child's own
    /// accumulated descendant totals.
    pub(crate) fn accumulate_child_time(&self, utime: TimeSpan, stime: TimeSpan) {
        self.cpu_totals.accumulate_child_time(utime, stime);
    }

    /// Returns accumulated reaped-children user and kernel time.
    pub(crate) fn child_time(&self) -> (TimeSpan, TimeSpan) {
        self.cpu_totals.child_time()
    }
}

impl ProcessEvents {
    fn new() -> Self {
        Self {
            child_exit_event: Arc::default(),
            exit_event: Arc::default(),
        }
    }

    fn child_exit_event(&self) -> &Arc<PollSet> {
        &self.child_exit_event
    }

    fn notify_child_exit(&self) {
        self.child_exit_event.wake();
    }

    fn exit_event(&self) -> &Arc<Completion> {
        &self.exit_event
    }

    fn notify_exit(&self) {
        self.exit_event.complete_all();
    }
}

impl ProcessCpuTotals {
    fn new() -> Self {
        Self {
            exited_thread_utime_ns: AtomicU64::new(0),
            exited_thread_stime_ns: AtomicU64::new(0),
            child_utime_ns: AtomicU64::new(0),
            child_stime_ns: AtomicU64::new(0),
        }
    }

    fn accumulate_exited_thread_time(&self, utime: TimeSpan, stime: TimeSpan) {
        self.exited_thread_utime_ns
            .fetch_add(span_to_raw_nanos(utime), Ordering::Relaxed);
        self.exited_thread_stime_ns
            .fetch_add(span_to_raw_nanos(stime), Ordering::Relaxed);
    }

    fn exited_thread_time(&self) -> (TimeSpan, TimeSpan) {
        (
            TimeSpan::from_nanos(self.exited_thread_utime_ns.load(Ordering::Relaxed)),
            TimeSpan::from_nanos(self.exited_thread_stime_ns.load(Ordering::Relaxed)),
        )
    }

    fn accumulate_child_time(&self, utime: TimeSpan, stime: TimeSpan) {
        self.child_utime_ns
            .fetch_add(span_to_raw_nanos(utime), Ordering::Relaxed);
        self.child_stime_ns
            .fetch_add(span_to_raw_nanos(stime), Ordering::Relaxed);
    }

    fn child_time(&self) -> (TimeSpan, TimeSpan) {
        (
            TimeSpan::from_nanos(self.child_utime_ns.load(Ordering::Relaxed)),
            TimeSpan::from_nanos(self.child_stime_ns.load(Ordering::Relaxed)),
        )
    }
}

fn span_to_raw_nanos(span: TimeSpan) -> u64 {
    span.as_nanos_u64_saturating()
}

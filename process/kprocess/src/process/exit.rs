// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU8, Ordering};

use kpoll::{Completion, PollSet};
use ksignal::Signo;
use ktime_types::TimeSpan;

use super::{INIT_PROC, Process};
use crate::{Pid, process_domain};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(super) enum ProcessExitState {
    Running = 0,
    Zombie  = 1,
    Dead    = 2,
}

impl ProcessExitState {
    fn try_from_raw(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Self::Running),
            1 => Some(Self::Zombie),
            2 => Some(Self::Dead),
            _ => None,
        }
    }

    fn raw(self) -> u8 {
        self as u8
    }
}

pub(super) struct AtomicProcessExitState(AtomicU8);

impl AtomicProcessExitState {
    pub(super) fn new(state: ProcessExitState) -> Self {
        Self(AtomicU8::new(state.raw()))
    }

    fn load(&self, ordering: Ordering) -> ProcessExitState {
        ProcessExitState::try_from_raw(self.0.load(ordering))
            .expect("invalid process exit state encoding")
    }

    fn compare_exchange(
        &self,
        current: ProcessExitState,
        new: ProcessExitState,
        success_order: Ordering,
        failure_order: Ordering,
    ) -> Result<ProcessExitState, ProcessExitState> {
        self.0
            .compare_exchange(current.raw(), new.raw(), success_order, failure_order)
            .map(|raw| {
                ProcessExitState::try_from_raw(raw).expect("invalid process exit state encoding")
            })
            .map_err(|raw| {
                ProcessExitState::try_from_raw(raw).expect("invalid process exit state encoding")
            })
    }

    fn store(&self, state: ProcessExitState, ordering: Ordering) {
        self.0.store(state.raw(), ordering);
    }
}

/// Process-exit publication mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessExitPublication {
    /// Publish the process as a waitable zombie for the parent.
    WaitableZombie,
    /// Skip the waitable zombie state and detach the process during exit.
    DetachedAutoreap,
}

impl ProcessExitPublication {
    fn autoreap(self) -> bool {
        matches!(self, Self::DetachedAutoreap)
    }
}

pub(crate) struct ProcessExitTransition<T> {
    pub(crate) parent: Option<Arc<Process>>,
    pub(crate) exit_signal: Option<Signo>,
    pub(crate) prepared_sigchld: Option<T>,
    pub(crate) autoreaped: bool,
    pub(crate) reparented_zombie_parent: Option<Arc<Process>>,
}

struct ExitParentSnapshot {
    parent: Option<Arc<Process>>,
    exit_signal: Option<Signo>,
}

impl ExitParentSnapshot {
    fn same_contract(&self, current: &Self) -> bool {
        self.exit_signal == current.exit_signal
            && match (&self.parent, &current.parent) {
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                (None, None) => true,
                _ => false,
            }
    }
}

/// Whether a wait scan should only observe or also consume a waitable child.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitReapMode {
    /// Report a waitable child without removing it from the parent relation.
    Peek,
    /// Consume one waitable zombie and detach it from the parent relation.
    Consume,
}

/// Which children a wait operation may observe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitChildSelector {
    /// Match any child process.
    Any,
    /// Match the child with this PID.
    Pid(Pid),
    /// Match children in this process group.
    Pgid(Pid),
}

/// Which child-exit signal class a wait operation may observe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitChildKind {
    /// Match children that report exit with `SIGCHLD`.
    Default,
    /// Match clone children that report exit with a signal other than `SIGCHLD`.
    Clone,
    /// Match both default and clone children.
    Any,
}

/// Result of scanning a process's children for a waitable exit.
#[derive(Debug)]
pub enum WaitChildScan {
    /// A matching waitable child was found.
    Ready(WaitedChild),
    /// No child matched the wait selector.
    NoMatchingChild,
    /// At least one child matched, but none is currently waitable.
    NoWaitableChild,
}

/// Stable wait result captured while holding the process-domain transaction lock.
#[derive(Debug)]
pub struct WaitedChild {
    process: Arc<Process>,
    pid: Pid,
    exit_code: i32,
    total_utime: TimeSpan,
    total_stime: TimeSpan,
    consumed: bool,
}

impl WaitedChild {
    /// Returns the waited process identity.
    pub fn process(&self) -> &Arc<Process> {
        &self.process
    }

    /// Returns the waited PID.
    pub fn pid(&self) -> Pid {
        self.pid
    }

    /// Returns the process exit code.
    pub fn exit_code(&self) -> i32 {
        self.exit_code
    }

    /// Returns the total user/kernel CPU time to charge to the parent.
    pub fn total_cpu_time(&self) -> (TimeSpan, TimeSpan) {
        (self.total_utime, self.total_stime)
    }

    /// Returns whether this wait result consumed the child.
    pub fn was_consumed(&self) -> bool {
        self.consumed
    }
}

impl Process {
    /// Returns `true` once the [`Process`] has exited.
    ///
    /// This remains true after the waitable zombie has been consumed so pidfd
    /// and live-process checks can continue to observe a stable exited state.
    pub fn is_exited(&self) -> bool {
        self.exit_state() != ProcessExitState::Running
    }

    /// Returns `true` if the process can currently be consumed by `wait*()`.
    pub fn is_waitable_zombie(&self) -> bool {
        self.exit_state() == ProcessExitState::Zombie
    }

    pub(super) fn exit_state(&self) -> ProcessExitState {
        self.exit_state.load(Ordering::Acquire)
    }

    pub(super) fn exit_state_locked(
        &self,
        _domain: &process_domain::ProcessDomainWriteGuard<'_>,
    ) -> ProcessExitState {
        self.exit_state.load(Ordering::Acquire)
    }

    pub(crate) fn is_exited_locked(
        &self,
        domain: &process_domain::ProcessDomainWriteGuard<'_>,
    ) -> bool {
        self.exit_state_locked(domain) != ProcessExitState::Running
    }

    /// Returns the child-exit wait event for this process.
    pub fn child_exit_event(&self) -> &Arc<PollSet> {
        self.lifecycle.child_exit_event()
    }

    pub(crate) fn notify_child_exit(&self) {
        self.lifecycle.notify_child_exit();
    }

    /// Returns the process-exit event for this process.
    pub fn exit_event(&self) -> &Arc<Completion> {
        self.lifecycle.exit_event()
    }

    /// Adds exited-thread CPU time to this process's accumulated counters.
    pub(crate) fn accumulate_exited_thread_time(&self, utime: TimeSpan, stime: TimeSpan) {
        self.lifecycle.accumulate_exited_thread_time(utime, stime);
    }

    /// Returns accumulated exited-thread user and kernel time.
    pub fn exited_thread_time(&self) -> (TimeSpan, TimeSpan) {
        self.lifecycle.exited_thread_time()
    }

    /// Adds reaped-child CPU time to this process's accumulated counters.
    pub(crate) fn accumulate_child_time(&self, utime: TimeSpan, stime: TimeSpan) {
        self.lifecycle.accumulate_child_time(utime, stime);
    }

    /// Returns accumulated reaped-children user and kernel time.
    pub fn child_time(&self) -> (TimeSpan, TimeSpan) {
        self.lifecycle.child_time()
    }

    /// Terminates the [`Process`] with the requested publication mode.
    pub(crate) fn exit_with_publication(self: &Arc<Self>, publication: ProcessExitPublication) {
        let transition = self.finish_exit_in_process_domain(publication, |_| None::<((), bool)>);
        if let Some(parent) = transition.reparented_zombie_parent {
            parent.notify_child_exit();
        }
    }

    /// Finishes process exit with child-exit signal policy prepared outside the
    /// process-domain write transaction.
    ///
    /// `prepare_sigchld` may be called more than once: if the sampled parent
    /// contract changes before the final commit, the prepared result is
    /// discarded and the exit path retries against the current parent.
    ///
    /// The closure must therefore be retry-safe. Any value returned for a
    /// non-committed retry attempt is dropped without a separate cancellation
    /// callback, so returned preparations must own cleanup through normal
    /// `Drop` or be cheap snapshots. Side effects inside the closure must be
    /// safe to observe even when the attempt that produced them is retried.
    pub(crate) fn finish_exit_in_process_domain<T>(
        self: &Arc<Self>,
        publication: ProcessExitPublication,
        prepare_sigchld: impl Fn(&Arc<Process>) -> Option<(T, bool)>,
    ) -> ProcessExitTransition<T> {
        loop {
            let snapshot = self.exit_parent_snapshot();
            let prepared = self.prepare_child_exit_signal_for_snapshot(&snapshot, &prepare_sigchld);
            let autoreap = publication.autoreap()
                || prepared
                    .as_ref()
                    .is_some_and(|(_, should_autoreap)| *should_autoreap);
            let prepared_sigchld = prepared.map(|(prepared, _)| prepared);

            let (transition, notify_exit, retry) = {
                let domain = process_domain::write_lock();
                let current = self.exit_parent_snapshot_locked();
                if !snapshot.same_contract(&current)
                    && self.exit_state_locked(&domain) == ProcessExitState::Running
                {
                    (
                        ProcessExitTransition {
                            parent: current.parent,
                            exit_signal: current.exit_signal,
                            prepared_sigchld: None,
                            autoreaped: false,
                            reparented_zombie_parent: None,
                        },
                        false,
                        true,
                    )
                } else {
                    let (state_changed, reparented_zombie_parent) =
                        self.finish_exit_locked(&domain, autoreap, current.parent.as_ref());
                    let prepared_sigchld = if state_changed {
                        prepared_sigchld
                    } else {
                        None
                    };

                    (
                        ProcessExitTransition {
                            parent: current.parent,
                            exit_signal: current.exit_signal,
                            prepared_sigchld,
                            autoreaped: autoreap && state_changed,
                            reparented_zombie_parent,
                        },
                        state_changed,
                        false,
                    )
                }
            };

            if notify_exit {
                self.lifecycle.notify_exit();
            }
            if !retry {
                return transition;
            }
        }
    }

    fn exit_parent_snapshot(&self) -> ExitParentSnapshot {
        let _domain = process_domain::read_lock();
        self.exit_parent_snapshot_locked()
    }

    fn exit_parent_snapshot_locked(&self) -> ExitParentSnapshot {
        let (parent, exit_signal) = self.parent_relation.snapshot();
        ExitParentSnapshot {
            parent,
            exit_signal,
        }
    }

    fn prepare_child_exit_signal_for_snapshot<T>(
        &self,
        snapshot: &ExitParentSnapshot,
        prepare_sigchld: impl Fn(&Arc<Process>) -> Option<(T, bool)>,
    ) -> Option<(T, bool)> {
        if snapshot.exit_signal == Some(Signo::SIGCHLD) {
            snapshot.parent.as_ref().and_then(prepare_sigchld)
        } else {
            None
        }
    }

    fn finish_exit_locked(
        self: &Arc<Self>,
        domain: &process_domain::ProcessDomainWriteGuard<'_>,
        autoreap: bool,
        parent: Option<&Arc<Process>>,
    ) -> (bool, Option<Arc<Process>>) {
        // TODO: child subreaper
        let reaper = INIT_PROC.get().unwrap();

        if Arc::ptr_eq(self, reaper) {
            return (false, None);
        }

        let exit_state = if autoreap {
            ProcessExitState::Dead
        } else {
            ProcessExitState::Zombie
        };
        if self
            .exit_state
            .compare_exchange(
                ProcessExitState::Running,
                exit_state,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return (false, None);
        }

        if autoreap && let Some(parent) = parent {
            Self::remove_child_from_parent_locked(domain, parent, self);
            self.clear_published_identity_locked(domain);
        }

        let mut reparented_zombie_parent = None;
        loop {
            let Some(old_slot) = self.children.lock().pop_front() else {
                break;
            };
            let Some(child) = old_slot.snapshot() else {
                continue;
            };
            old_slot.clear();
            child.attach_to_parent_slot_locked(domain, reaper, Some(Signo::SIGCHLD));
            if child.exit_state_locked(domain) == ProcessExitState::Dead {
                continue;
            }
            if child.exit_state_locked(domain) == ProcessExitState::Zombie {
                reparented_zombie_parent = Some(reaper.clone());
            }
        }

        (true, reparented_zombie_parent)
    }

    /// Frees a waitable zombie [`Process`]. Removes it from the parent.
    ///
    /// This method panics if the [`Process`] is not in the waitable zombie
    /// state. Processes already marked `Dead` are handled by non-waitable reap
    /// paths and must not enter this path.
    #[cfg(unittest)]
    pub(crate) fn free(&self) {
        assert!(
            self.reap_waitable_zombie_from_parent(),
            "only waitable zombie process can be freed, pid: {}, current state: {:?}, parent: \
             {:?}, linked_in_parent: {}",
            self.pid(),
            self.exit_state(),
            self.parent().map(|parent| parent.pid()),
            self.is_linked_in_current_parent()
        );
    }

    #[cfg(unittest)]
    fn is_linked_in_current_parent(&self) -> bool {
        let _domain = process_domain::read_lock();
        let Some(parent) = self.parent_relation.parent() else {
            return false;
        };
        parent.children.lock().iter().any(|slot| {
            slot.snapshot()
                .is_some_and(|child| core::ptr::eq(child.as_ref(), self))
        })
    }

    pub(crate) fn reap_waitable_zombie_from_parent(&self) -> bool {
        let domain = process_domain::write_lock();
        if self.exit_state_locked(&domain) != ProcessExitState::Zombie {
            return false;
        }

        let Some(parent) = self.parent_relation.parent() else {
            return false;
        };

        if !Self::remove_child_from_parent_locked(&domain, &parent, self) {
            return false;
        }

        self.exit_state
            .store(ProcessExitState::Dead, Ordering::Release);
        self.clear_published_identity_locked(&domain);
        true
    }

    pub(crate) fn scan_waitable_child(
        self: &Arc<Self>,
        selector: WaitChildSelector,
        kind: WaitChildKind,
        mode: WaitReapMode,
    ) -> WaitChildScan {
        let domain = process_domain::write_lock();
        let (matched, ready_child) = {
            let children = self.children.lock();
            let mut matched = false;
            let mut ready_child = None;

            for slot in children.iter() {
                let Some(child) = slot.snapshot() else {
                    continue;
                };
                if !Self::wait_selector_matches_child(&child, selector, kind) {
                    continue;
                }
                matched = true;
                if child.exit_state_locked(&domain) == ProcessExitState::Zombie {
                    ready_child = Some((slot.pid, child.clone()));
                    break;
                }
            }
            (matched, ready_child)
        };

        let Some((pid, child)) = ready_child else {
            return if matched {
                WaitChildScan::NoWaitableChild
            } else {
                WaitChildScan::NoMatchingChild
            };
        };

        let exit_code = child.exit_code();
        let (thread_utime, thread_stime) = child.exited_thread_time();
        let (child_utime, child_stime) = child.child_time();
        let waited = WaitedChild {
            process: child.clone(),
            pid,
            exit_code,
            total_utime: thread_utime.saturating_add(child_utime),
            total_stime: thread_stime.saturating_add(child_stime),
            consumed: mode == WaitReapMode::Consume,
        };

        if mode == WaitReapMode::Consume {
            Self::remove_child_from_parent_locked(&domain, self, &child);
            child
                .exit_state
                .store(ProcessExitState::Dead, Ordering::Release);
            child.clear_published_identity_locked(&domain);
        }

        WaitChildScan::Ready(waited)
    }

    fn wait_selector_matches_child(
        child: &Process,
        selector: WaitChildSelector,
        kind: WaitChildKind,
    ) -> bool {
        let selector_matches = match selector {
            WaitChildSelector::Any => true,
            WaitChildSelector::Pid(pid) => child.pid() == pid,
            WaitChildSelector::Pgid(pgid) => child.group_membership.group().pgid() == pgid,
        };
        if !selector_matches {
            return false;
        }

        match kind {
            WaitChildKind::Any => true,
            WaitChildKind::Default => child.parent_relation.exit_signal() == Some(Signo::SIGCHLD),
            WaitChildKind::Clone => child.parent_relation.exit_signal() != Some(Signo::SIGCHLD),
        }
    }

    pub(crate) fn detach_dead_from_parent(&self) -> bool {
        let domain = process_domain::write_lock();
        if self.exit_state_locked(&domain) != ProcessExitState::Dead {
            return false;
        }

        let Some(parent) = self.parent_relation.parent() else {
            return false;
        };

        let detached = Self::remove_child_from_parent_locked(&domain, &parent, self);
        if detached {
            self.clear_published_identity_locked(&domain);
        }
        detached
    }
}

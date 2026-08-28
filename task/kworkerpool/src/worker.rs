// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Worker-slot state and runtime callback traits.

use ktime_types::MonotonicInstant;

use crate::{ActionBatch, RunnableClaimer, WorkerId};

/// Lifecycle state of one worker slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerState {
    /// No runtime task is associated with the slot.
    Empty,
    /// The manager reserved the slot and is creating a runtime task.
    Creating,
    /// The runtime task exists and may be selected for work.
    Idle,
    /// Idle retirement has been requested but may still be cancelled.
    RetireRequested,
    /// The runtime task has begun exit and cannot be recovered.
    Exiting,
    /// A wake has been selected and the worker must re-enter the pool loop.
    ///
    /// This state is wait-ready even if the runnable queue is observed empty:
    /// the worker loop owns the transition back to `Idle`, and runtime wake
    /// delivery is allowed to be repeated while runnable work remains.
    Preparing,
    /// The worker popped an entry and waits for external claim validation.
    Claiming,
    /// The worker owns a current execution.
    Running,
    /// The worker owns a current execution but is blocked in a sleepable wait.
    Sleeping,
}

/// Current-execution concurrency accounting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionAccounting {
    /// A running worker counts toward pool concurrency.
    Normal,
    /// A long-running callback no longer counts toward pool concurrency.
    CpuIntensive,
}

/// Pool-local token for one worker-slot execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerExecutionToken(usize);

impl WorkerExecutionToken {
    const FIRST: Self = Self(1);

    /// First valid execution token for one worker slot.
    pub const fn first() -> Self {
        Self::FIRST
    }

    /// Returns the next token generation for the same worker slot.
    pub fn next_generation(self) -> Self {
        Self(self.0.wrapping_add(1).max(1))
    }

    /// Creates a token from a runtime-owned task-local value.
    pub const fn from_usize(token: usize) -> Self {
        Self(token)
    }

    /// Returns the numeric token value.
    pub const fn as_usize(self) -> usize {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CurrentExecution {
    token: WorkerExecutionToken,
    pub(crate) started_at: MonotonicInstant,
    accounting: ExecutionAccounting,
}

impl CurrentExecution {
    fn new(token: WorkerExecutionToken, started_at: MonotonicInstant) -> Self {
        Self {
            token,
            started_at,
            accounting: ExecutionAccounting::Normal,
        }
    }

    pub(crate) const fn token(self) -> WorkerExecutionToken {
        self.token
    }

    pub(crate) const fn accounting(self) -> ExecutionAccounting {
        self.accounting
    }
}

/// Runtime hooks used by the pool-owned worker task loop.
///
/// The worker pool owns the worker thread's scheduling loop: park decisions,
/// runnable FIFO popping, execution-token creation, stale-entry retry, and
/// completion accounting. This trait supplies only the environment-specific
/// pieces that the core pool cannot know. `P` is a copyable pool handle, not a
/// second pool owner:
///
/// - current time;
/// - how a worker blocks until its pool-local wake source is signalled;
/// - how returned wake/timer actions are applied after the pool lock is
///   dropped;
/// - how an accepted opaque entry is executed.
///
/// Implementors also provide [`RunnableClaimer`]. The worker loop calls the
/// claim hook after dropping the pool lock, then commits or discards the
/// reserved candidate back in the pool.
pub trait WorkerRuntime<P>: RunnableClaimer
where
    P: Copy,
{
    /// Returns the current monotonic time used for worker-pool accounting.
    fn now(&self) -> MonotonicInstant;

    /// Blocks until this worker should retry the pool-owned park boundary.
    fn wait_for_worker_work(&mut self, pool: P, worker: WorkerId);

    /// Applies actions returned by pool state transitions.
    ///
    /// The worker-pool core computes actions while holding the pool lock, but
    /// calls this hook only after that lock has been dropped.
    fn handle_worker_actions(&mut self, pool: P, actions: ActionBatch);

    /// Runs one claimed entry after it has been removed from the pool runnable
    /// FIFO and marked as the worker's current execution.
    fn run_claimed_work(
        &mut self,
        pool: P,
        worker: WorkerId,
        token: WorkerExecutionToken,
        work: Self::Accepted,
    );
}

/// One worker slot owned by a worker pool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Worker<ThreadRef> {
    pub(crate) state: WorkerState,
    generation: WorkerExecutionToken,
    pub(crate) current: Option<CurrentExecution>,
    pub(crate) idle_since: Option<MonotonicInstant>,
    pub(crate) thread_ref: Option<ThreadRef>,
}

impl<ThreadRef> Worker<ThreadRef> {
    pub(crate) const fn new() -> Self {
        Self {
            state: WorkerState::Empty,
            generation: WorkerExecutionToken::FIRST,
            current: None,
            idle_since: None,
            thread_ref: None,
        }
    }

    pub(crate) fn set_thread_ref(&mut self, thread_ref: ThreadRef) {
        self.thread_ref = Some(thread_ref);
    }

    pub(crate) fn clear_thread_ref(&mut self) -> Option<ThreadRef> {
        self.thread_ref.take()
    }

    pub(crate) fn next_execution(&mut self, started_at: MonotonicInstant) -> CurrentExecution {
        self.generation = self.generation.next_generation();
        let current = CurrentExecution::new(self.generation, started_at);
        self.current = Some(current);
        self.idle_since = None;
        current
    }

    pub(crate) fn reserve_execution_token(&mut self) -> WorkerExecutionToken {
        self.generation = self.generation.next_generation();
        self.generation
    }

    pub(crate) fn reserved_token_matches(&self, token: WorkerExecutionToken) -> bool {
        self.generation == token
    }

    pub(crate) fn start_reserved_execution(
        &mut self,
        token: WorkerExecutionToken,
        started_at: MonotonicInstant,
    ) -> Option<CurrentExecution> {
        if self.generation != token {
            return None;
        }
        let current = CurrentExecution::new(token, started_at);
        self.current = Some(current);
        self.idle_since = None;
        Some(current)
    }

    pub(crate) fn current_matches(&self, token: WorkerExecutionToken) -> bool {
        self.current.is_some_and(|current| current.token == token)
    }

    pub(crate) fn mark_cpu_intensive(&mut self) {
        if let Some(current) = self.current.as_mut() {
            current.accounting = ExecutionAccounting::CpuIntensive;
        }
    }

    pub(crate) fn clear_execution(&mut self) {
        self.current = None;
        self.idle_since = None;
    }
}

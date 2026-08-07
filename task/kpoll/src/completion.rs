// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Completion-style readiness primitive for poll-based waiters.

use kspin::SpinNoIrq;

use crate::{PollContext, PollRegisterError, PollSet};

const COMPLETE_ALL: usize = usize::MAX;
const COMPLETE_TOKEN_MAX: usize = COMPLETE_ALL - 1;

/// A low-level completion source with Linux-like token semantics.
///
/// `Completion` records completion tokens and provides a [`PollSet`]-backed
/// wake source for waiters. It does not block the current task by itself; task
/// code should use [`Self::try_wait`], [`Self::register`], then recheck
/// [`Self::try_wait`] before returning `Poll::Pending`.
///
/// Unlike Linux `complete()`, [`Self::complete`] wakes every currently
/// registered poll waiter because [`PollSet`] is a broadcast source. Correct
/// waiters must recheck [`Self::try_wait`]; only one waiter can consume the
/// token produced by one `complete()` call.
pub struct Completion {
    state: SpinNoIrq<CompletionState>,
    waiters: PollSet,
}

struct CompletionState {
    done: usize,
}

impl CompletionState {
    const fn new() -> Self {
        Self { done: 0 }
    }

    fn try_wait(&mut self) -> bool {
        match self.done {
            0 => false,
            COMPLETE_ALL => true,
            _ => {
                self.done -= 1;
                true
            }
        }
    }

    fn complete(&mut self) {
        if self.done < COMPLETE_TOKEN_MAX {
            self.done += 1;
        }
    }

    fn complete_all(&mut self) {
        self.done = COMPLETE_ALL;
    }

    fn is_completed(&self) -> bool {
        self.done != 0
    }

    fn reinit(&mut self) {
        self.done = 0;
    }
}

impl Completion {
    /// Creates a completion in the not-yet-completed state.
    pub fn new() -> Self {
        Self {
            state: SpinNoIrq::new(CompletionState::new()),
            waiters: PollSet::new(),
        }
    }

    /// Attempts to consume one completion token without blocking.
    ///
    /// Returns `false` when the completion has no available token. After
    /// [`Self::complete_all`], this method keeps returning `true` until
    /// [`Self::reinit`] is called.
    pub fn try_wait(&self) -> bool {
        self.state.lock().try_wait()
    }

    /// Returns whether at least one completion token is currently available.
    ///
    /// This method is a state query only and does not consume a token.
    pub fn is_completed(&self) -> bool {
        self.state.lock().is_completed()
    }

    /// Adds one completion token and wakes registered waiters.
    ///
    /// The returned value is the number of waiters woken by the underlying
    /// [`PollSet`]. It is intended for diagnostics and tests, not as a count of
    /// waiters that consumed the token.
    pub fn complete(&self) -> usize {
        {
            let mut state = self.state.lock();
            state.complete();
        }
        self.waiters.wake()
    }

    /// Marks this completion permanently complete and wakes registered waiters.
    ///
    /// Call [`Self::reinit`] before reusing the completion. As with Linux
    /// `reinit_completion`, callers must ensure all waiters woken by
    /// `complete_all` have finished observing the old completion state before
    /// resetting it.
    pub fn complete_all(&self) -> usize {
        self.complete_all_defer_wake().wake()
    }

    /// Marks this completion permanently complete and returns its wake source.
    ///
    /// This is for callers that must update another lock-protected predicate
    /// and the completion state atomically, but still need to invoke wakeups
    /// after releasing their outer lock. The returned [`PollSet`] must be woken
    /// after the caller drops that outer lock.
    pub fn complete_all_defer_wake(&self) -> PollSet {
        {
            let mut state = self.state.lock();
            state.complete_all();
        }
        self.waiters.clone()
    }

    /// Resets the completion to the not-yet-completed state.
    ///
    /// This does not clear registered waiters. Callers must ensure resetting
    /// the state cannot race with waiters that still rely on a previous
    /// [`Self::complete_all`] observation.
    pub fn reinit(&self) {
        self.state.lock().reinit();
    }

    /// Registers the current logical wait for completion wakeups.
    ///
    /// Waiters must use a check/register/recheck sequence:
    ///
    /// 1. call [`Self::try_wait`];
    /// 2. call `register(context)` if no token is available;
    /// 3. call [`Self::try_wait`] again before returning `Poll::Pending`.
    ///
    /// The second check closes the race where a completion happens between the
    /// first check and waiter registration.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying poll registration cannot be stored.
    pub fn register(&self, context: &mut PollContext<'_>) -> Result<(), PollRegisterError> {
        context.register(&self.waiters)
    }
}

impl Default for Completion {
    fn default() -> Self {
        Self::new()
    }
}

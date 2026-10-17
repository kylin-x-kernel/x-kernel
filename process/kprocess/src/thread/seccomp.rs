// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-thread seccomp mode and classic-BPF filter chain.

use alloc::{sync::Arc, vec, vec::Vec};

use kerrno::{KError, KResult};
use kseccomp::{Action, Decision, SeccompData, SeccompFilter};
use kspin::SpinNoIrq;
use linux_sysno::Sysno;

/// Maximum number of instructions across one thread's whole filter chain
/// (Linux `MAX_INSNS_PER_PATH`), counting a 4-instruction penalty per filter.
const MAX_INSNS_PER_PATH: usize = (1 << 18) / core::mem::size_of::<kseccomp::SockFilter>();

/// Whether the thread runs `SECCOMP_MODE_DISABLED`, `STRICT`, or `FILTER`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SeccompMode {
    /// No seccomp filter is installed (`SECCOMP_MODE_DISABLED`).
    Disabled,
    /// Only the strict-mode allowlist applies (`SECCOMP_MODE_STRICT`).
    Strict,
    /// The installed filter chain applies (`SECCOMP_MODE_FILTER`).
    Filter,
}

/// The mode and chain move as one value so a reader can never observe a mode
/// that disagrees with the chain it describes.
#[derive(Debug)]
struct SeccompState {
    mode: SeccompMode,
    filters: Vec<Arc<SeccompFilter>>,
}

/// The thread-local seccomp evaluation state.
///
/// The mode and the ordered filter chain are mutated only through the
/// `seccomp(2)` syscall on the owning thread (or a `TSYNC` broadcast),
/// while syscall entry evaluates against the chain. The `SpinNoIrq` guard
/// keeps both mutation and read paths cheap on the syscall hot path; the
/// evaluation path never allocates, so it stays safe under that lock.
#[derive(Debug)]
pub struct SeccompContext {
    state: SpinNoIrq<SeccompState>,
}

impl SeccompContext {
    /// Returns a fresh disabled context.
    pub fn new() -> Self {
        Self {
            state: SpinNoIrq::new(SeccompState {
                mode: SeccompMode::Disabled,
                filters: Vec::new(),
            }),
        }
    }

    /// Returns the current mode.
    pub(crate) fn mode(&self) -> SeccompMode {
        self.state.lock().mode
    }

    /// Returns the Linux `SECCOMP_MODE_*` number reported by
    /// `prctl(PR_GET_SECCOMP)`.
    pub fn linux_mode(&self) -> u32 {
        match self.mode() {
            SeccompMode::Disabled => 0,
            SeccompMode::Strict => 1,
            SeccompMode::Filter => 2,
        }
    }

    /// Returns whether the fixed strict allowlist is the active policy.
    ///
    /// Linux terminates a strict-mode victim with `SIGKILL` (not the `SIGSYS`
    /// a filter kill delivers), so the caller needs to distinguish the two.
    pub fn is_strict_allowlist(&self) -> bool {
        self.mode() == SeccompMode::Strict
    }

    /// Installs strict mode.
    ///
    /// Strict mode is implemented as the kernel's fixed allowlist filter
    /// (Linux `seccomp` strict allowlist: `read`, `write`, `_exit`,
    /// `rt_sigreturn`), evaluated with the normal BPF engine.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` when the thread is already in filter mode: Linux
    /// `seccomp_may_assign_mode` refuses to change an installed mode, and the
    /// strict allowlist is less restrictive than an arbitrary filter chain.
    pub fn set_strict(&self) -> KResult<()> {
        let mut state = self.state.lock();
        match state.mode {
            // Re-selecting the same mode is idempotent in Linux.
            SeccompMode::Strict => Ok(()),
            SeccompMode::Filter => Err(KError::InvalidInput),
            SeccompMode::Disabled => {
                let allow = [Sysno::read, Sysno::write, Sysno::exit, Sysno::rt_sigreturn];
                let nrs = allow.map(|sysno| sysno.id() as u32);
                let filter =
                    SeccompFilter::strict_allowlist(&nrs).map_err(|_| KError::InvalidInput)?;
                state.filters = vec![Arc::new(filter)];
                state.mode = SeccompMode::Strict;
                Ok(())
            }
        }
    }

    /// Appends one filter to the chain and switches to filter mode.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` in strict mode (Linux refuses to extend an
    /// installed strict mode with a filter) and `NoMemory` when the chain
    /// would exceed Linux `MAX_INSNS_PER_PATH`.
    pub fn append_filter(&self, filter: Arc<SeccompFilter>) -> KResult<()> {
        let mut state = self.state.lock();
        if state.mode == SeccompMode::Strict {
            return Err(KError::InvalidInput);
        }
        // Linux `MAX_INSNS_PER_PATH`: the whole chain (with a 4-instruction
        // penalty per linked filter) is bounded so evaluation cost stays finite.
        let total: usize = state
            .filters
            .iter()
            .map(|installed| installed.instructions().len() + 4)
            .sum::<usize>()
            + filter.instructions().len()
            + 4;
        if total > MAX_INSNS_PER_PATH {
            return Err(KError::NoMemory);
        }
        state.filters.push(filter);
        state.mode = SeccompMode::Filter;
        Ok(())
    }

    /// Copies the mode and a shallow-cloned filter chain from another
    /// context. Used by `fork`/`clone` (children inherit seccomp state).
    pub fn inherit_from(&self, parent: &Self) {
        // Snapshot before taking this context's lock so no path holds two
        // contexts' locks at once.
        let (mode, filters) = {
            let parent_state = parent.state.lock();
            (parent_state.mode, parent_state.filters.clone())
        };
        let mut state = self.state.lock();
        state.mode = mode;
        state.filters = filters;
    }

    /// Reports whether this context can be brought to `caller`'s chain by a
    /// `TSYNC` without becoming more permissive.
    ///
    /// Mirrors Linux `seccomp_can_sync_threads`: a thread must be disabled, or
    /// its chain must be a prefix of the caller's, so synchronisation only ever
    /// adds restrictions.
    pub fn is_syncable_with(&self, caller: &Self) -> bool {
        let caller_chain = caller.state.lock().filters.clone();
        can_adopt(&self.state.lock(), &caller_chain)
    }

    /// Adopts `caller`'s chain for a `TSYNC` broadcast.
    ///
    /// Re-validates under this context's own lock (a sibling may have installed
    /// a filter since the caller's pre-check), so a restriction can never be
    /// removed: an unsynchronisable sibling keeps its own stricter chain and
    /// this returns `false`. Mode and chain are updated together.
    pub fn sync_from(&self, caller: &Self) -> bool {
        let caller_chain = caller.state.lock().filters.clone();
        let mut state = self.state.lock();
        if !can_adopt(&state, &caller_chain) {
            return false;
        }
        state.filters = caller_chain;
        if state.mode == SeccompMode::Disabled {
            state.mode = SeccompMode::Filter;
        }
        true
    }

    /// Evaluates the installed filters against a `seccomp_data` snapshot.
    ///
    /// With no filters installed the decision is always [`Decision::Allow`].
    /// Otherwise every filter runs and the highest-precedence result wins.
    /// The fold allocates nothing: this runs on every syscall entry while the
    /// chain lock is held.
    pub fn evaluate(&self, data: &SeccompData) -> Decision {
        let state = self.state.lock();
        kseccomp::highest_priority(
            state.filters.iter().map(|filter| {
                Action::from_ret(kseccomp::run(filter.instructions(), data.as_bytes()))
            }),
        )
    }
}

/// Returns whether `state` may adopt `caller_chain` without losing a
/// restriction, mirroring Linux `seccomp_can_sync_threads`.
fn can_adopt(state: &SeccompState, caller_chain: &[Arc<SeccompFilter>]) -> bool {
    match state.mode {
        SeccompMode::Disabled => true,
        SeccompMode::Filter => is_prefix(&state.filters, caller_chain),
        // Linux refuses to synchronise a thread that is in strict mode.
        SeccompMode::Strict => false,
    }
}

/// Returns whether `mine` is a prefix of `theirs` by filter identity, so
/// adopting `theirs` only appends filters.
fn is_prefix(mine: &[Arc<SeccompFilter>], theirs: &[Arc<SeccompFilter>]) -> bool {
    mine.len() <= theirs.len()
        && mine
            .iter()
            .zip(theirs.iter())
            .all(|(mine, theirs)| Arc::ptr_eq(mine, theirs))
}

impl Default for SeccompContext {
    fn default() -> Self {
        Self::new()
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/seccomp.rs");

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process-owned limits and task-owned detachable descriptor-table bindings.
//!
//! [`ProcessResources`] owns process-wide limits. [`TaskFiles`] owns one task's
//! binding to a possibly shared descriptor table. Both require task context.

#![no_std]

extern crate alloc;

mod files;
use alloc::sync::Arc;

pub use files::TaskFiles;
use kerrno::{KError, KResult};
use krlimit::{Rlimit, Rlimits};
use ksync::RwLock;
use linux_raw_sys::general::{RLIM_NLIMITS, RLIMIT_NOFILE};

/// Process-owned resource state.
///
/// Shared by every thread in one process runtime.
pub struct ProcessResources {
    /// Per-process resource limits.
    pub rlimits: RwLock<Rlimits>,
}

impl ProcessResources {
    /// Creates a new process resource set with default limits.
    ///
    /// `user_stack_size` is the configured user stack capacity in bytes,
    /// passed to [`Rlimits::new`].
    pub fn new(user_stack_size: usize) -> Arc<Self> {
        Arc::new(Self {
            rlimits: RwLock::new(Rlimits::new(user_stack_size)),
        })
    }

    /// Returns the current RLIMIT_NOFILE soft cap.
    pub fn max_nofile(&self) -> u64 {
        self.rlimits.read()[RLIMIT_NOFILE].current
    }

    /// Returns the current limit for a specific resource.
    ///
    /// # Errors
    ///
    /// Returns `KError::InvalidInput` when `resource >= RLIM_NLIMITS`.
    pub fn rlimit(&self, resource: u32) -> KResult<Rlimit> {
        if resource >= RLIM_NLIMITS {
            return Err(KError::InvalidInput);
        }

        Ok(self.rlimits.read()[resource])
    }

    /// Returns the old limit and optionally replaces it in one atomic operation.
    ///
    /// `None` returns a checked snapshot through [`Self::rlimit`] under a limits
    /// read lock without changing the pair. `Some(new_limit)` returns the exact
    /// pair replaced. For an update, validation, the privilege check and the
    /// replacement share one limits write lock: `may_raise_hard_limit` is the
    /// syscall layer's `CAP_SYS_RESOURCE` decision (Linux `do_prlimit()`), and
    /// raising the hard limit without it fails against the CURRENT limit — a
    /// concurrent update between a caller-side snapshot and this call cannot
    /// be bypassed. A failed update returns no snapshot and changes no state.
    ///
    /// Requires sleepable task context. This method does not access user memory;
    /// callers must copy inputs before calling and publish outputs after it returns.
    ///
    /// # Errors
    ///
    /// Returns `KError::InvalidInput` for an out-of-range resource or soft > hard.
    /// Returns `KError::OperationNotPermitted` when the update would raise the
    /// hard limit and `may_raise_hard_limit` is false.
    pub fn update_rlimit_and_get_old(
        &self,
        resource: u32,
        new_limit: Option<Rlimit>,
        may_raise_hard_limit: bool,
    ) -> KResult<Rlimit> {
        let Some(new_limit) = new_limit else {
            return self.rlimit(resource);
        };

        if resource >= RLIM_NLIMITS {
            return Err(KError::InvalidInput);
        }

        let mut limits = self.rlimits.write();
        let limit = &mut limits[resource];
        let old_limit = *limit;
        if new_limit.current > new_limit.max {
            return Err(KError::InvalidInput);
        }
        // Linux `do_prlimit()`: a `RLIMIT_NOFILE` hard limit beyond
        // `sysctl_nr_open` is EPERM before the capability check. The check
        // precedes the capability test, so even a privileged caller cannot
        // raise it past the ceiling — but a caller that stays under it may
        // raise the limit past what this kernel can actually allocate, since
        // the fd table is fixed; see `krlimit::SYSCTL_NR_OPEN_DEFAULT`.
        if resource == RLIMIT_NOFILE && new_limit.max > krlimit::SYSCTL_NR_OPEN_DEFAULT {
            return Err(KError::OperationNotPermitted);
        }
        // Linux `do_prlimit()` rejects `new_max > old_max` without
        // `CAP_SYS_RESOURCE`; because the soft limit is validated against the
        // new hard limit above, that single check also covers a soft limit
        // beyond the current hard limit.
        if !may_raise_hard_limit && new_limit.max > old_limit.max {
            return Err(KError::OperationNotPermitted);
        }
        *limit = new_limit;

        Ok(old_limit)
    }
}

unittest::path_mod_test!(tests, "../ut-tests/lib.rs");

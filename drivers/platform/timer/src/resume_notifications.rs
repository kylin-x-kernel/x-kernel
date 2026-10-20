// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Independent per-CPU resume requests, without a machine-word mask limit.

use core::sync::atomic::{AtomicBool, Ordering};

pub(super) struct ResumeNotifications<const CPUS: usize> {
    pending: [AtomicBool; CPUS],
}

impl<const CPUS: usize> ResumeNotifications<CPUS> {
    pub(super) const fn new() -> Self {
        Self {
            pending: [const { AtomicBool::new(false) }; CPUS],
        }
    }

    /// Publish peer requests before the caller sends the recovery IPI.
    ///
    /// `active_cpus` must cover every present logical ID: IDs must be
    /// contiguous in `0..active_cpus` within the configured capacity.
    /// Invalid topology leaves all pending flags unchanged.
    pub(super) fn mark_peers(&self, active_cpus: usize, current_cpu: usize) -> bool {
        if active_cpus > CPUS || current_cpu >= active_cpus || active_cpus <= 1 {
            return false;
        }
        for (cpu, pending) in self.pending[..active_cpus].iter().enumerate() {
            if cpu != current_cpu {
                // Publish before sending the IPI. Each CPU consumes only its
                // own flag; a new request after consumption stays pending.
                pending.store(true, Ordering::Release);
            }
        }
        true
    }

    /// Consume this CPU's request once; repeated pending requests coalesce.
    pub(super) fn take(&self, cpu: usize) -> bool {
        self.pending
            .get(cpu)
            .is_some_and(|pending| pending.swap(false, Ordering::AcqRel))
    }
}

#[cfg(unittest)]
unittest::path_mod_test!(tests, "../ut-tests/resume_notifications.rs");

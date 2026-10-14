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

#[cfg(any(test, unittest))]
#[cfg_attr(unittest, unittest::mod_test)]
mod tests {
    #[cfg(unittest)]
    use unittest::def_test;

    use super::ResumeNotifications;

    #[cfg_attr(test, test)]
    #[cfg_attr(unittest, def_test)]
    fn peers_consume_independent_requests() {
        let requests = ResumeNotifications::<4>::new();
        assert!(requests.mark_peers(3, 1));
        assert!(!requests.take(1));
        assert!(requests.take(0));
        assert!(!requests.take(0));
        assert!(requests.take(2));
        assert!(!requests.take(3));
    }

    #[cfg_attr(test, test)]
    #[cfg_attr(unittest, def_test)]
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

    #[cfg_attr(test, test)]
    #[cfg_attr(unittest, def_test)]
    fn repeated_requests_coalesce_until_consumed() {
        let requests = ResumeNotifications::<2>::new();
        assert!(requests.mark_peers(2, 0));
        assert!(requests.mark_peers(2, 0));
        assert!(requests.take(1));
        assert!(!requests.take(1));
        assert!(requests.mark_peers(2, 0));
        assert!(requests.take(1));
    }

    #[cfg_attr(test, test)]
    #[cfg_attr(unittest, def_test)]
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

    #[cfg_attr(test, test)]
    #[cfg_attr(unittest, def_test)]
    fn invalid_topology_does_not_change_pending_requests() {
        let requests = ResumeNotifications::<2>::new();
        assert!(requests.mark_peers(2, 0));
        assert!(!requests.mark_peers(3, 0));
        assert!(!requests.mark_peers(2, 2));
        assert!(!requests.take(usize::MAX));
        assert!(!requests.take(0));
        assert!(requests.take(1));
    }

    #[cfg_attr(test, test)]
    #[cfg_attr(unittest, def_test)]
    fn single_cpu_needs_no_remote_notification() {
        let requests = ResumeNotifications::<1>::new();
        assert!(!requests.mark_peers(1, 0));
        assert!(!requests.take(0));
    }
}

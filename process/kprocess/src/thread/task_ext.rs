// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kbandwidth::Bandwidth;
use kcpu_id_map::LogicalCpuId;
use ktask::{TaskInner, UserTaskRuntime};

use super::Thread;

/// Helper trait to access the thread from a task.
pub trait AsThread {
    /// Tries to get the thread from the task.
    fn try_as_thread(&self) -> Option<&Thread>;

    /// Returns the thread from the task, panicking if it is a kernel task.
    ///
    /// # Panics
    ///
    /// Panics when the task has no matching user Thread runtime.
    fn as_thread(&self) -> &Thread {
        self.try_as_thread().expect("kernel task")
    }
}

impl UserTaskRuntime for Thread {
    fn fatal_signal_pending(&self) -> bool {
        self.signal_manager().fatal_signal_pending()
    }

    fn task_number(&self) -> Arc<kidentity::PidHandle> {
        Thread::task_number(self)
    }

    fn tid(&self) -> u32 {
        Thread::tid(self)
    }

    fn identity_token(&self) -> kidentity::TaskIdentityToken {
        Thread::identity_token(self)
    }

    fn cpu_bandwidth(&self) -> Option<Arc<Bandwidth>> {
        self.cgroup()
            .and_then(|cgroup| cgroup.effective_cpu_bandwidth())
    }

    fn cpu_weight(&self) -> u32 {
        self.cgroup()
            .map_or(100, |cgroup| cgroup.effective_cpu_weight())
    }

    fn cgroup_cpus(&self) -> Option<ktask::KCpuMask> {
        kcgroup::refresh_root_cpuset_universe();
        let mut mask = ktask::KCpuMask::new();
        match self.cgroup() {
            Some(cgroup) => {
                let cpus = cgroup.cpuset_effective_cpus().ok()?;
                let count = cpus.iter().count();
                // A root cgroup created before secondary CPUs were discovered
                // may still expose only the boot CPU. Do not pin new tasks to
                // that stale root universe; child cgroups keep their explicit
                // narrow mask.
                if cgroup.parent().is_none() && count == 1 && kcpu_id_map::nr_cpus() > 1 {
                    for cpu in 0..kcpu_id_map::nr_cpus() {
                        mask.set(cpu, true);
                    }
                } else {
                    for cpu in cpus.iter() {
                        mask.set(cpu, true);
                    }
                }
            }
            None => {
                // A task without explicit cgroup membership is in the root
                // cgroup. It must not inherit the boot CPU's narrow affinity.
                for cpu in 0..kcpu_id_map::nr_cpus() {
                    mask.set(cpu, true);
                }
            }
        }
        Some(mask)
    }

    fn on_periodic_tick(&self, user_tick: bool, tick_ns: u64) {
        // The owning CPU charges one tick to the interrupted context's raw
        // counter, then requests coalesced CPU-timer work for the next user
        // return. Switch boundaries do not settle time.
        self.account_tick(user_tick, tick_ns);
        self.request_cpu_timer_work();
    }

    fn set_user_mm_resident_cpu(&self, cpu_id: LogicalCpuId) {
        self.set_process_mm_resident_cpu(cpu_id);
    }

    fn switch_page_table_root(&self) -> Option<karch::HwPageTableRoot> {
        #[cfg(target_arch = "aarch64")]
        {
            Some(self.process_page_table_hw_root())
        }

        #[cfg(not(target_arch = "aarch64"))]
        {
            None
        }
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

impl AsThread for TaskInner {
    fn try_as_thread(&self) -> Option<&Thread> {
        self.user_runtime()?.as_any().downcast_ref::<Thread>()
    }
}

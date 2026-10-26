// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Concrete task storage and notification adapters for RCU.

use kcpu_id_map::LogicalCpuId;
use kerrno::KResult;
use krcu::host::{
    CurrentReader, PreparedGpTask, RcuGpTaskLaunchIf, RcuNotifyIf, RcuReaderStorageIf, RcuSoftirqIf,
};

#[kiface::provide]
impl RcuGpTaskLaunchIf {
    fn start_gp_task(entry: PreparedGpTask) {
        let task = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
            move || entry.run(),
            "rcu_gp".into(),
            kbuild_config::TASK_STACK_SIZE,
        ));
        ktask::activate_task(&task);
    }
}

#[kiface::provide]
impl RcuReaderStorageIf {
    fn with_current_reader(body_fn: &mut dyn FnMut(CurrentReader<'_>)) {
        let task = ktask::current();
        // SAFETY: current retains this stack's task borrow through the call,
        // including migration and strictly nested handlers. The body cannot
        // retain this lifetime. RCU, not this adapter, controls scope admission
        // and enrollment cleanup under the reader's additional contracts.
        let reader = unsafe { task.rcu_reader_for_integration() };
        // SAFETY: reader is borrowed from retained current above; the higher-
        // ranked body cannot save this task-local certificate beyond the call.
        let current = unsafe { CurrentReader::new(reader) };
        body_fn(current);
    }
}

#[kiface::provide]
impl RcuSoftirqIf {
    fn install(action_fn: fn()) -> bool {
        kirq::softirq::open_softirq(kirq::softirq::SoftirqVec::Rcu, action_fn)
    }

    fn raise_local() {
        kirq::softirq::raise_softirq(kirq::softirq::SoftirqVec::Rcu);
    }
}

#[kiface::provide]
impl RcuNotifyIf {
    fn notify_rcu(cpu: LogicalCpuId) -> KResult<()> {
        #[cfg(feature = "ipi")]
        {
            kipi::notify_rcu(cpu).map_err(|_| kerrno::KError::BadState)
        }
        #[cfg(not(feature = "ipi"))]
        {
            if cpu != khal::percpu::this_cpu_id() {
                return Err(kerrno::KError::BadState);
            }
            kirq::softirq::raise_softirq(kirq::softirq::SoftirqVec::Rcu);
            Ok(())
        }
    }
}

unittest::path_mod_test!(tests_context, "../ut-tests/rcu_context.rs");
unittest::path_mod_test!(rcu_support, "../ut-tests/rcu_support.rs");
unittest::path_mod_test!(tests_owned, "../ut-tests/rcu_owned.rs");
unittest::path_mod_test!(tests_read, "../ut-tests/rcu_read.rs");
unittest::path_mod_test!(tests_cleanup, "../ut-tests/rcu_cleanup.rs");
unittest::path_mod_test!(tests_service, "../ut-tests/rcu_service.rs");
#[cfg(feature = "smp")]
unittest::path_mod_test!(tests_implicit, "../ut-tests/rcu_implicit.rs");
unittest::path_mod_test!(tests_callback, "../ut-tests/rcu_callback.rs");
unittest::path_mod_test!(tests_drain, "../ut-tests/rcu_drain.rs");
unittest::path_mod_test!(tests_service_startup, "../ut-tests/rcu_service_startup.rs");
unittest::path_mod_test!(tests_synchronize, "../ut-tests/rcu_synchronize.rs");

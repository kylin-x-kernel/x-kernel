// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Prepared notification identity, independent of service and source lifetime.

use alloc::{sync::Arc, task::Wake};
use core::task::Waker;

use kcpu_id_map::LogicalCpuId;
use kirq::softirq::{self, SoftirqVec};

use super::{BottomHalfPrepareError, BottomHalfPriority};

struct BottomHalfWake {
    cpu: LogicalCpuId,
    priority: BottomHalfPriority,
}

pub(super) fn prepare(
    cpu: LogicalCpuId,
    priority: BottomHalfPriority,
) -> Result<Arc<Waker>, BottomHalfPrepareError> {
    if kcpu_id_map::raw_cpu_id(cpu).is_none() {
        return Err(BottomHalfPrepareError::CpuUnavailable);
    }
    Ok(Arc::new(Waker::from(Arc::new(BottomHalfWake {
        cpu,
        priority,
    }))))
}

impl Wake for BottomHalfWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        // Keep the local-vs-remote decision valid through local pending publication.
        let _cpu = kspin::NoPreempt::new();
        if khal::percpu::this_cpu_id() == self.cpu {
            softirq::raise_softirq(match self.priority {
                BottomHalfPriority::Normal => SoftirqVec::Tasklet,
                BottomHalfPriority::High => SoftirqVec::High,
            });
        } else {
            let result = match self.priority {
                BottomHalfPriority::Normal => kipi::notify_bottom_half(self.cpu),
                BottomHalfPriority::High => kipi::notify_highpri_bottom_half(self.cpu),
            };
            // Preparation validated the immutable boot map. Failure here is an
            // invariant violation, not a recoverable loss of accepted source demand.
            result.expect("prepared BH target remains present without CPU hotplug");
        }
    }
}

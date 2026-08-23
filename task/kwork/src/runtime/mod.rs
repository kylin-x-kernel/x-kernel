// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Runtime instances for built-in workerqueue lanes.
//!
//! This module owns concrete queue instances such as `system_percpu_wq`,
//! `system_wq` compatibility aliases, and `system_long_wq`, plus bottom-half
//! runtime lanes that mirror Linux's `system_bh_wq` family. The per-CPU
//! `(kind, cpu)` bindings connect those instances to their worker-pool and
//! `pool_workqueue` state.

mod bh;
mod system;

use bh::bh_wq_cpu;
pub use bh::{
    BottomHalfPoolBinding, BottomHalfWorkQueueKind, system_bh_highpri_wq,
    system_bh_highpri_wq_for_cpu, system_bh_wq, system_bh_wq_for_cpu,
};
pub(crate) use bh::{BottomHalfWake, bh_wq_kind_cpu};
use kcpu_id_map::LogicalCpuId;
use system::system_wq_cpu;
pub use system::{
    INITIAL_SYSTEM_WORKERS_PER_CPU, MAX_SYSTEM_WORKERS_PER_CPU, SystemPoolBinding,
    SystemWorkQueueKind, schedule_long_work, schedule_long_work_on, schedule_work,
    schedule_work_on, system_long_wq, system_long_wq_for_cpu, system_percpu_wq,
    system_percpu_wq_for_cpu, system_wq, system_wq_for_cpu,
};
pub(crate) use system::{TaskPoolBinding, TaskPoolWake, system_pool_for_cpu};

use crate::WorkQueue;

pub(crate) fn static_array_index<T>(array: &'static [T], item: &'static T) -> Option<usize> {
    static_array_index_by_key(array, core::ptr::from_ref(item).addr())
}

pub(crate) fn static_array_index_by_key<T>(array: &'static [T], key: usize) -> Option<usize> {
    let elem_size = core::mem::size_of::<T>();
    if elem_size == 0 {
        return None;
    }

    let base = array.as_ptr().addr();
    let byte_len = core::mem::size_of_val(array);
    let end = base.checked_add(byte_len)?;
    if key < base || key >= end {
        return None;
    }

    let offset = key - base;
    if !offset.is_multiple_of(elem_size) {
        return None;
    }
    let index = offset / elem_size;
    (index < array.len()).then_some(index)
}

/// Returns the CPU of a built-in static runtime queue.
///
/// Core queue code uses this only to reject reconfiguration of built-in
/// runtime instances. The concrete `system_*` / BH family remains owned by the
/// runtime module.
pub(crate) fn builtin_queue_cpu(queue: &'static WorkQueue) -> Option<LogicalCpuId> {
    system_wq_cpu(queue).or_else(|| bh_wq_cpu(queue))
}

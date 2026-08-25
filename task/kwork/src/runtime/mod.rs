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

pub use bh::{BottomHalfPoolBinding, BottomHalfWorkQueueKind, system_bh_highpri_wq, system_bh_wq};
pub(crate) use bh::{BottomHalfWake, bh_queue_cpu_is_valid, bh_wq_kind};
pub use system::{
    INITIAL_SYSTEM_WORKERS_PER_CPU, MAX_SYSTEM_WORKERS_PER_CPU, SystemPoolBinding, SystemPoolKind,
    SystemWorkQueueKind, schedule_long_work, schedule_long_work_on, schedule_work,
    schedule_work_on, system_long_wq, system_percpu_wq, system_wq,
};
pub(crate) use system::{
    TaskPoolBinding, TaskPoolWake, system_pool_for_cpu, system_queue_cpu_is_valid, system_wq_kind,
};

use crate::WorkQueue;

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

/// Returns whether `queue` is owned by the built-in runtime.
pub(crate) fn is_builtin_queue(queue: &'static WorkQueue) -> bool {
    system_wq_kind(queue).is_some() || bh_wq_kind(queue).is_some()
}

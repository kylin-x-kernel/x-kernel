// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ktask thread references and naming helpers.

use alloc::{format, string::String, sync::Arc};

use kcpu_id_map::{KCpuMask, KCpuMaskExt, LogicalCpuId};
use kpoll::PollEvent;

use crate::{PoolId, WorkerId};

/// ktask-backed thread reference attached to one worker-pool slot.
#[derive(Clone)]
pub struct WorkerThreadRef {
    task: Option<ktask::KtaskRef>,
    wake_source: Arc<PollEvent>,
}

impl WorkerThreadRef {
    /// Creates a live ktask-backed worker or manager thread reference.
    pub fn new(task: ktask::KtaskRef, wake_source: Arc<PollEvent>) -> Self {
        Self {
            task: Some(task),
            wake_source,
        }
    }

    /// Creates a non-task placeholder for execution contexts driven elsewhere.
    pub fn placeholder() -> Self {
        Self {
            task: None,
            wake_source: Arc::new(PollEvent::new()),
        }
    }

    /// Returns the underlying task, if this slot is backed by ktask.
    pub fn task(&self) -> Option<ktask::KtaskRef> {
        self.task.clone()
    }

    /// Wakes the pool-local wait source observed by this thread.
    pub fn wake(&self) {
        if let Some(task) = &self.task {
            ktask::wake_task(task, true);
        }
        let _ = self.wake_source.notify();
    }

    /// Returns the wait source used by the worker or manager loop.
    pub fn wake_source(&self) -> &PollEvent {
        &self.wake_source
    }
}

impl Default for WorkerThreadRef {
    fn default() -> Self {
        Self::placeholder()
    }
}

/// Prepares a CPU-bound pidless kernel thread without activating it.
pub fn prepare_bound_kthread<F>(cpu_id: LogicalCpuId, name: String, f: F) -> ktask::KtaskRef
where
    F: FnOnce() + Send + 'static,
{
    let task = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
        f,
        name,
        kbuild_config::TASK_STACK_SIZE,
    ));
    task.set_cpumask(KCpuMask::one_shot_logical(cpu_id));
    task
}

/// Runtime-visible worker thread name.
pub fn worker_name(pool_id: PoolId, worker_id: WorkerId, pool_name: &str) -> String {
    format!(
        "kworkerpool/{}/{}:{}",
        pool_name,
        pool_id.cpu().as_usize(),
        worker_id.as_usize()
    )
}

/// Runtime-visible manager thread name.
pub fn manager_name(pool_id: PoolId, pool_name: &str) -> String {
    format!(
        "kworkerpool/{}/{}/manager",
        pool_name,
        pool_id.cpu().as_usize()
    )
}

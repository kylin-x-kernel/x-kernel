// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel worker-pool scheduling and ktask runtime.
//!
//! This crate owns worker-pool state, runnable/deferred pool queues, and
//! lifecycle management decisions. It also owns ktask-backed worker and
//! manager task loops. Built-in system pool instances live in `kwork`.
//!
//! # Module Map
//!
//! - [`WorkerPool`] is the core state machine.
//! - [`PoolEntry`] is the opaque entry stored by a pool.
//! - [`WorkerPoolPolicy`] configures concurrency, dynamic creation, retirement,
//!   and CPU-intensive accounting.
//! - [`KtaskWorkerPool`] adds ktask-backed worker and manager threads.
//! - [`WorkerTask`] and [`ManagerTask`] are the task-loop bodies.
//! - [`WorkerRuntime`] and [`RunnableClaimer`] let a product layer validate
//!   entries and run callbacks without exposing product state to the pool.
//!
//! # Basic flow
//!
//! ```rust
//! use kcpu_id_map::LogicalCpuId;
//! use ktime_types::TimeSpan;
//! use kworkerpool::{
//!     EntryKey, EntryOwner, EntryPayload, EntrySource, PoolEntry, PoolId, PoolKind, WorkerPool,
//!     WorkerPoolPolicy, WorkerPoolPolicyConfig,
//! };
//!
//! let policy = WorkerPoolPolicy::new(WorkerPoolPolicyConfig {
//!     min_workers: 1,
//!     initial_workers: 1,
//!     max_workers: 4,
//!     idle_retire_after: Some(TimeSpan::from_secs(30)),
//!     create_retry_delay: TimeSpan::from_millis(10),
//!     cpu_intensive_threshold: TimeSpan::from_millis(10),
//!     manager_managed: true,
//!     dynamic_create: true,
//!     idle_retire: true,
//! });
//!
//! let pool_id = PoolId::new(PoolKind::new(0), LogicalCpuId::new(0));
//! let mut pool: WorkerPool<(), 4, 128> = WorkerPool::new(pool_id, policy);
//! let entry = PoolEntry::new(
//!     EntrySource::new(1),
//!     EntryOwner::new(1),
//!     EntryKey::new(1),
//!     EntryPayload::new(1),
//! );
//! let _actions = pool
//!     .enqueue_runnable(entry, ktask::monotonic_time())
//!     .unwrap();
//! ```

#![cfg_attr(not(test), no_std)]

extern crate alloc;
#[macro_use]
extern crate log;

mod action;
mod entry;
mod execution;
mod id;
mod ktask_pool;
mod manager;
mod policy;
mod pool;
mod queue;
mod task_loop;
#[cfg(unittest)]
mod tests;
mod thread;
mod worker;

pub use action::{ActionBatch, ImmediateAction, ManagementAction};
pub use entry::{EntryKey, EntryOwner, EntryPayload, EntrySource};
pub use execution::{WorkerPoolExecutionContext, decode_task_context, encode_task_context};
pub use id::{PoolId, PoolKind, WorkerId};
pub use ktask_pool::{
    ExecutionTickResult, KtaskWorkerPool, PoolNameResolver, WorkerRuntimeFactory,
    start_manager_task,
};
pub use manager::{
    ManagerRuntime, ManagerTarget, ManagerTask, run_worker_pool_manager_pass,
    run_worker_pool_manager_set_pass,
};
pub use policy::{WorkerPoolPolicy, WorkerPoolPolicyConfig};
pub use pool::{
    ManagementComplete, ParkDecision, RunnableCandidate, RunnableCandidateDiscard,
    RunnableCandidateResult, RunnableClaim, RunnableClaimer, WorkerPool, WorkerPoolError,
    WorkerPoolSnapshot,
};
pub use queue::{PoolEntry, QueueRemoveResult};
pub use task_loop::{CurrentWorkerPoolExecutionGuard, WorkerTask};
pub use thread::{WorkerThreadRef, manager_name, prepare_bound_kthread, worker_name};
pub use worker::{ExecutionAccounting, WorkerExecutionToken, WorkerRuntime, WorkerState};

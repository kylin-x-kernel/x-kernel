// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Caller-owned queue creation and lifecycle authority, without a second queue model.

use alloc::sync::Arc;

pub use kworkerpool_ng::BottomHalfPriority;
pub use kworkqueue_ng::service::Queue as WorkQueue;
pub use services::{ExecutionKind, QueueCreateError};

use crate::{QueueConfig, services};

/// Creates an independent named queue using initialized shared execution services.
///
/// Requires sleepable task context. The supplied configuration fixes CPU policy,
/// allowed boot CPUs and per-CPU release allowance; it does not configure workers.
/// Preparation may allocate, but later prepared submissions do not. The name is
/// diagnostic identity, not a unique routing key; it need not outlive this call.
///
/// The returned Arc grants queue-wide flush and close_and_drain authority. Neither
/// operation stops shared workers or other queues. Before shutdown, stop/join
/// producers and synchronously cancel every associated DelayedWork, including idle
/// ones; shutdown does not discover or drain timer-only requests. Work storage stays
/// caller-owned and must outlive its asynchronous accesses. Dropping the Arc is not
/// an implicit shutdown. There is no conversion from DefaultQueue to this authority.
///
/// # Errors
/// Warns and returns an identifiable context/readiness/CPU binding error before
/// exposing a queue. Failure does not accept any work or stop existing services.
/// Invalid configuration is rejected earlier by QueueConfig::new.
///
/// # Panics
/// Propagates fatal allocation failures and violated internal provider invariants.
///
/// # Examples
/// ```no_run
/// use core::pin::pin;
///
/// use kworkng::{CpuPolicy, ExecutionKind, QueueConfig, Work, create_queue};
/// // After kernel NG initialization, in sleepable context with logical CPU 0 present.
/// let config = QueueConfig::new(&[0.into()], CpuPolicy::CurrentCpu, 1).unwrap();
/// let queue = create_queue("device-maintenance", config, ExecutionKind::Normal).unwrap();
/// let work = pin!(Work::with_callback(|| {}));
/// // SAFETY: this sole producer retains the pinned Work and captures through drain.
/// unsafe { queue.queue_work(work.as_ref(), Some(0.into())) }.unwrap();
/// queue.flush().unwrap();
/// work.cancel_sync().unwrap();
/// queue.close_and_drain().unwrap();
/// ```
pub fn create_queue(
    name: &str,
    config: QueueConfig,
    kind: ExecutionKind,
) -> Result<Arc<WorkQueue>, QueueCreateError> {
    let result = services::prepare_queue(name, config, kind);
    if let Err(error) = &result {
        log::warn!("[kworkng] queue '{name}' creation failed: {error:?}");
    }
    result
}

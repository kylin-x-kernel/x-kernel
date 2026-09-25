// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Execution-provider construction contracts, separate from caller-owned Queue APIs.

use alloc::sync::Arc;
use core::task::Waker;

use kcpu_id_map::LogicalCpuId;
use kworkerpool_ng::PartitionDispatch;

use crate::QueueConfig;
#[cfg(feature = "stall-check")]
pub use crate::diagnostics::Directory;
pub use crate::{
    queue::{Queue, QueuePartitionSource},
    work::{execution::Execution, wait::WaitError},
};

/// Prepares a one-shot provider Work with an action after terminal completion.
/// Cancellation does not invoke retirement. The retirement action is outside the
/// Work's flush/accounting scope and may release the enclosing Work allocation.
/// This is a provider contract, not a general user callback API.
///
/// # Safety
/// Publish this Work at most once, through a provider compatible with both actions.
/// Retain context and Work across publication and terminal invocation on any CPU.
/// The function/context pair must be valid, thread-safe and invoked only here.
/// If it frees Work, exclude all other Work access first, including observers
/// that could retain its address. Handle failed admission/cancellation separately.
///
/// # Panics
/// Construction does not panic; execution propagates callback/retirement panics.
pub unsafe fn prepare_retiring_work(
    context: *const (),
    retire: unsafe fn(*const ()),
) -> crate::Work<fn()> {
    let mut work = crate::Work::with_callback((|| {}) as fn());
    work.set_retirement(crate::work::Retirement {
        context,
        call: retire,
    });
    work
}

/// Binds all allowed CPUs to matching execution-category providers before exposure.
/// Call in sleepable task context; retain providers through ordinary drain and source
/// detachment. Kicks must be IRQ-compatible and retain no Queue/Work. This does not
/// initialize providers or certify subsystem readiness.
///
/// # Panics
/// Propagates provider closure panics and fatal kernel allocation failures.
pub fn bind_queue(
    config: QueueConfig,
    service: impl FnMut(LogicalCpuId) -> (Arc<PartitionDispatch<QueuePartitionSource>>, Arc<Waker>),
) -> Arc<Queue> {
    Queue::with_dispatch(config, service)
}

/// Binds execution and named per-CPU diagnostics before exposing a queue.
/// Has the same context, notification and provider lifetime contract as bind_queue.
/// Copies the name during preparation; the directory retains weak records, not Queue.
///
/// # Panics
/// Propagates provider closure panics and fatal kernel allocation failures.
#[cfg(feature = "stall-check")]
pub fn bind_queue_with_diagnostics(
    config: QueueConfig,
    name: &str,
    directory: &Directory,
    service: impl FnMut(LogicalCpuId) -> (Arc<PartitionDispatch<QueuePartitionSource>>, Arc<Waker>),
) -> Arc<Queue> {
    Queue::with_dispatch_and_diagnostics(config, name, directory, service)
}

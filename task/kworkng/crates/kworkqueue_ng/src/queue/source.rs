// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Concrete partition acquisition and protected demand publication.
//!
//! An immutable registration belongs to QueuePartition. Its payload refers back to Queue
//! weakly, so neither an idle source nor queued demand creates an owning cycle.
//! Publication only changes dispatch state. The returned service-only Waker is
//! invoked and released outside management locks, without retaining registration.

use alloc::sync::{Arc, Weak};
use core::task::Waker;

use kcpu_id_map::LogicalCpuId;
use kworkerpool_ng::{PartitionDispatch, PartitionSource, PartitionWorkSource, SourceRemainder};

use super::Queue;
use crate::work::execution::Execution;

/// One queue-owned binding to its CPU/category service.
pub(super) struct QueueSource {
    dispatch: Arc<PartitionDispatch<QueuePartitionSource>>,
    source: Arc<PartitionSource<QueuePartitionSource>>,
    kick: Arc<Waker>,
}

/// Weak management access; never owns the externally stored Work.
pub struct QueuePartitionSource {
    queue: Weak<Queue>,
    cpu: LogicalCpuId,
}

impl QueueSource {
    pub(super) fn detachment_completion(&self) -> Arc<kpoll::Completion> {
        self.source.detachment_completion()
    }

    /// The caller drained ordinary Work and holds no management lock. Dispatch
    /// detachment may complete waiters, including concurrent queue shutdowns.
    pub(super) fn detach(&self) {
        self.dispatch.detach(&self.source);
    }

    /// Prepare before exposing Queue. kick must retain only independent service
    /// state, never Queue, Work or source registration, and must be IRQ-compatible.
    /// Waking requests service; it must not invoke business callbacks inline.
    pub(super) fn new(
        queue: Weak<Queue>,
        cpu: LogicalCpuId,
        dispatch: Arc<PartitionDispatch<QueuePartitionSource>>,
        kick: Arc<Waker>,
    ) -> Self {
        let source = dispatch.create_source(QueuePartitionSource { queue, cpu });
        Self {
            dispatch,
            source,
            kick,
        }
    }

    /// Called under partition protection, with no Work guard held. Arc cloning
    /// does not invoke the provider's RawWaker clone callback.
    pub(super) fn publish(&self) -> Arc<Waker> {
        self.dispatch.notify(&self.source);
        self.kick.clone()
    }
}

impl PartitionWorkSource for QueuePartitionSource {
    type Execution = Execution;

    fn acquire(&self) -> (Option<Execution>, SourceRemainder) {
        let Some(queue) = self.queue.upgrade() else {
            return (None, SourceRemainder::Empty);
        };
        // The accepted running slot retains Queue, while unsafe publication keeps
        // Work/captures live. Neither relies on this temporary upgraded reference.
        let (execution, has_more) = queue.claim_with_demand(self.cpu);
        let remainder = if has_more {
            SourceRemainder::More
        } else {
            SourceRemainder::Empty
        };
        (execution, remainder)
    }
}

unittest::path_mod_test!(source_unittest, "../../ut-tests/queue_source_unittest.rs");

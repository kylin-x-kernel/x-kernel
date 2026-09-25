// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Optional callback identity, borrowed from Work only while its callback runs.

use alloc::sync::Arc;
use core::marker::PhantomPinned;

use intrusive_collections::{UnsafeRef, intrusive_adapter, linked_list::AtomicLink};
use kspin::SpinNoIrq;

use super::QueuePartition;

/// One embedded diagnostic node. Pending successors use a different Work link.
/// The key is comparison-only; it never owns or dereferences a task.
pub(crate) struct CallbackNode {
    link: AtomicLink,
    task: SpinNoIrq<Option<u64>>,
    _pin: PhantomPinned,
}

impl CallbackNode {
    pub(crate) fn new() -> Self {
        Self {
            link: AtomicLink::new(),
            task: SpinNoIrq::new(None),
            _pin: PhantomPinned,
        }
    }

    pub(crate) fn runs_on(&self, task: u64) -> bool {
        *self.task.lock() == Some(task)
    }

    /// Run a callback with task identity and optional original-partition membership.
    /// No guard escapes; callback entry/exit do not allocate. No management lock
    /// is held across the body. This does not grant permission to sleep in BH.
    ///
    /// # Safety
    /// Keep this initialized node stable, shared and live through the body and
    /// unlink. There must be exactly one running occurrence of the enclosing Work;
    /// partition must be its original running partition. The body must not exit
    /// the task without unwinding: abandoning Work execution is forbidden.
    pub(crate) unsafe fn run<R>(
        &self,
        partition: Option<Arc<QueuePartition>>,
        body: impl FnOnce() -> R,
    ) -> R {
        let task = ktask::current().owner_key();
        {
            let mut key = self.task.lock();
            assert!(key.is_none(), "overlapping callback identity");
            *key = Some(task);
        }
        if let Some(partition) = &partition {
            let mut state = partition.state.lock();
            assert!(!self.link.is_linked(), "callback node already linked");
            // SAFETY: the executor retains stable shared Work through this scope.
            // The unique callback owns this node; partition protection serializes
            // list access and the private guard unlinks before the scope ends.
            let node = unsafe { UnsafeRef::from_raw(self as *const Self) };
            state.callbacks.push_back(node);
        }
        let _guard = CallbackGuard {
            node: self,
            partition,
        };
        body()
    }
}

struct CallbackGuard<'a> {
    node: &'a CallbackNode,
    partition: Option<Arc<QueuePartition>>,
}

impl Drop for CallbackGuard<'_> {
    fn drop(&mut self) {
        if let Some(partition) = &self.partition {
            let mut state = partition.state.lock();
            // SAFETY: run inserted this unique node into this original partition.
            // This private guard retains both borrows/ownership through removal;
            // the partition lock excludes every list traversal and mutation.
            let mut cursor = unsafe { state.callbacks.cursor_mut_from_ptr(self.node) };
            assert!(cursor.remove().is_some());
        }
        assert!(self.node.task.lock().take().is_some());
    }
}

intrusive_adapter!(pub(super) CallbackAdapter = UnsafeRef<CallbackNode>: CallbackNode { link => AtomicLink });

impl QueuePartition {
    pub(in crate::queue) fn has_callback_on(&self, task: u64) -> bool {
        self.state
            .lock()
            .callbacks
            .iter()
            .any(|node| node.runs_on(task))
    }
}

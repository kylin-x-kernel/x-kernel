// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec::Vec};

use kpoll::{Completion, PollSet};

/// Maximum barriers linked to one pending entry or running worker slot.
///
/// This is intentionally separate from `WORKQUEUE_PENDING_CAP`: pending
/// capacity counts work entries, while this counts flush waiters attached to a
/// single observed instance. Reusing pending capacity here would multiply
/// fixed arrays (`pending entries × barriers per entry`) and inflate every
/// worker pool.
pub(crate) const MAX_WORK_BARRIERS_PER_SLOT: usize = 8;

#[derive(Clone)]
pub(crate) struct WorkBarrier {
    inner: Arc<WorkBarrierInner>,
}

struct WorkBarrierInner {
    done: Completion,
}

impl WorkBarrier {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(WorkBarrierInner {
                done: Completion::new(),
            }),
        }
    }

    pub(crate) fn completion(&self) -> &Completion {
        &self.inner.done
    }

    #[cfg(unittest)]
    pub(crate) fn same_barrier(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    fn complete_defer_wake(&self) -> PollSet {
        self.inner.done.complete_all_defer_wake()
    }
}

/// Fixed-capacity storage for barriers attached to one work position.
///
/// Linux represents flush barriers by list position (`WORK_STRUCT_LINKED` on
/// pending work or `worker->scheduled` for running work). X-Kernel keeps the
/// same ownership split but uses a bounded array so attaching a barrier never
/// grows an unbounded `Vec` under workqueue locks.
pub(crate) struct WorkBarrierQueue {
    /// Fixed slots that hold attached barriers without growing under locks.
    entries: [Option<WorkBarrier>; MAX_WORK_BARRIERS_PER_SLOT],
    /// Number of occupied prefix slots in `entries`.
    len: usize,
}

impl WorkBarrierQueue {
    pub(crate) const fn new() -> Self {
        Self {
            entries: [const { None }; MAX_WORK_BARRIERS_PER_SLOT],
            len: 0,
        }
    }

    pub(crate) fn push_front_linked_barrier(&mut self, barrier: WorkBarrier) -> bool {
        if self.len == MAX_WORK_BARRIERS_PER_SLOT {
            return false;
        }
        let mut index = self.len;
        while index != 0 {
            self.entries[index] = self.entries[index - 1].take();
            index -= 1;
        }
        self.entries[0] = Some(barrier);
        self.len += 1;
        true
    }

    fn push_back_preserving_order(&mut self, barrier: WorkBarrier) -> bool {
        if self.len == MAX_WORK_BARRIERS_PER_SLOT {
            return false;
        }
        self.entries[self.len] = Some(barrier);
        self.len += 1;
        true
    }

    pub(crate) fn append_from_vec(&mut self, barriers: Vec<WorkBarrier>) -> bool {
        if self.len + barriers.len() > MAX_WORK_BARRIERS_PER_SLOT {
            return false;
        }
        for barrier in barriers {
            let pushed = self.push_back_preserving_order(barrier);
            debug_assert!(pushed);
        }
        true
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn take_all(&mut self) -> Vec<WorkBarrier> {
        let mut taken = Vec::new();
        for index in 0..self.len {
            if let Some(barrier) = self.entries[index].take() {
                taken.push(barrier);
            }
        }
        self.len = 0;
        taken
    }
}

pub(crate) fn complete_barriers_defer_wake(barriers: Vec<WorkBarrier>) -> Vec<PollSet> {
    barriers
        .into_iter()
        .map(|barrier| barrier.complete_defer_wake())
        .collect()
}

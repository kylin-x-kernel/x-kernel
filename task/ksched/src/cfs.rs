// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{collections::BTreeMap, sync::Arc};
use core::{
    ops::Deref,
    sync::atomic::{AtomicIsize, Ordering},
};

use crate::{BaseScheduler, CurrentDisposition};

/// task for CFS
pub struct CFSTask<T> {
    inner: T,
    init_vruntime: AtomicIsize,
    delta: AtomicIsize,
    nice: AtomicIsize,
    id: AtomicIsize,
}

// https://elixir.bootlin.com/linux/latest/source/include/linux/sched/prio.h

const NICE_RANGE_POS: usize = 19; // MAX_NICE in Linux
const NICE_RANGE_NEG: usize = 20; // -MIN_NICE in Linux, the range of nice is [MIN_NICE, MAX_NICE]

// https://elixir.bootlin.com/linux/latest/source/kernel/sched/core.c

const NICE2WEIGHT_POS: [isize; NICE_RANGE_POS + 1] = [
    1024, 820, 655, 526, 423, 335, 272, 215, 172, 137, 110, 87, 70, 56, 45, 36, 29, 23, 18, 15,
];
const NICE2WEIGHT_NEG: [isize; NICE_RANGE_NEG + 1] = [
    1024, 1277, 1586, 1991, 2501, 3121, 3906, 4904, 6100, 7620, 9548, 11916, 14949, 18705, 23254,
    29154, 36291, 46273, 56483, 71755, 88761,
];

impl<T> CFSTask<T> {
    /// new with default values
    pub const fn new(inner: T) -> Self {
        Self {
            inner,
            init_vruntime: AtomicIsize::new(0_isize),
            delta: AtomicIsize::new(0_isize),
            nice: AtomicIsize::new(0_isize),
            id: AtomicIsize::new(0_isize),
        }
    }

    fn get_weight(&self) -> isize {
        let nice = self.nice.load(Ordering::Acquire);
        if nice >= 0 {
            NICE2WEIGHT_POS[nice as usize]
        } else {
            NICE2WEIGHT_NEG[(-nice) as usize]
        }
    }

    fn get_id(&self) -> isize {
        self.id.load(Ordering::Acquire)
    }

    fn get_vruntime(&self) -> isize {
        if self.nice.load(Ordering::Acquire) == 0 {
            self.init_vruntime.load(Ordering::Acquire) + self.delta.load(Ordering::Acquire)
        } else {
            self.init_vruntime.load(Ordering::Acquire)
                + self.delta.load(Ordering::Acquire) * 1024 / self.get_weight()
        }
    }

    fn set_vruntime(&self, v: isize) {
        self.init_vruntime.store(v, Ordering::Release);
    }

    // Simple Implementation: no change in vruntime.
    // Only modifying priority of current process is supported currently.
    fn set_priority(&self, nice: isize) {
        let current_init_vruntime = self.get_vruntime();
        self.init_vruntime
            .store(current_init_vruntime, Ordering::Release);
        self.delta.store(0, Ordering::Release);
        self.nice.store(nice, Ordering::Release);
    }

    fn set_id(&self, id: isize) {
        self.id.store(id, Ordering::Release);
    }

    fn task_tick(&self) {
        self.delta.fetch_add(1, Ordering::Release);
    }

    /// Returns a reference to the inner task struct.
    pub const fn inner(&self) -> &T {
        &self.inner
    }
}

impl<T> Deref for CFSTask<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

/// A simple [Completely Fair Scheduler][1] (CFS).
///
/// [1]: https://en.wikipedia.org/wiki/Completely_Fair_Scheduler
pub struct CFScheduler<T> {
    ready_queue: BTreeMap<(isize, isize), Arc<CFSTask<T>>>, // (vruntime, taskid)
    min_vruntime: Option<AtomicIsize>,
    id_pool: AtomicIsize,
}

impl<T> CFScheduler<T> {
    /// Creates a new empty [`CFScheduler`].
    pub const fn new() -> Self {
        Self {
            ready_queue: BTreeMap::new(),
            min_vruntime: None,
            id_pool: AtomicIsize::new(0_isize),
        }
    }

    /// get the name of scheduler
    pub fn scheduler_name() -> &'static str {
        "Completely Fair"
    }

    /// Recompute `min_vruntime` from the ready queue's earliest key.
    fn refresh_min_vruntime(&mut self) {
        if let Some(((min_vruntime, _), _)) = self.ready_queue.first_key_value() {
            self.min_vruntime = Some(AtomicIsize::new(*min_vruntime));
        } else {
            self.min_vruntime = None;
        }
    }
}

impl<T> BaseScheduler for CFScheduler<T> {
    type SchedItem = Arc<CFSTask<T>>;

    fn init(&mut self) {}

    fn add_task(&mut self, task: Self::SchedItem) {
        if self.min_vruntime.is_none() {
            self.min_vruntime = Some(AtomicIsize::new(0_isize));
        }
        let vruntime = self.min_vruntime.as_mut().unwrap().load(Ordering::Acquire);
        let taskid = self.id_pool.fetch_add(1, Ordering::Release);
        task.set_vruntime(vruntime);
        task.set_id(taskid);
        self.ready_queue.insert((vruntime, taskid), task);
        self.refresh_min_vruntime();
    }

    fn remove_task(&mut self, task: &Self::SchedItem) -> Option<Self::SchedItem> {
        if let Some((_, tmp)) = self
            .ready_queue
            .remove_entry(&(task.get_vruntime(), task.get_id()))
        {
            self.refresh_min_vruntime();
            Some(tmp)
        } else {
            None
        }
    }

    fn pick_next_task(&mut self) -> Option<Self::SchedItem> {
        let task = self.ready_queue.pop_first().map(|(_, v)| v)?;
        // Ready queue no longer holds the running task; keep min_vruntime in sync.
        self.refresh_min_vruntime();
        Some(task)
    }

    fn enqueue_task(&mut self, task: Self::SchedItem) {
        let taskid = self.id_pool.fetch_add(1, Ordering::Release);
        task.set_id(taskid);
        // Key is evaluated before `task` moves into `insert`; no clone needed.
        self.ready_queue.insert((task.get_vruntime(), taskid), task);
        self.refresh_min_vruntime();
    }

    fn leave_current(&mut self, current: Self::SchedItem, disposition: CurrentDisposition) {
        match disposition {
            CurrentDisposition::Yield | CurrentDisposition::Preempt => {
                self.enqueue_task(current);
            }
            CurrentDisposition::Block | CurrentDisposition::Migrate | CurrentDisposition::Exit => {
                // Running CFS tasks are already off the ready queue; departing
                // without requeue only needs to keep min_vruntime coherent.
                self.refresh_min_vruntime();
            }
        }
    }

    fn task_tick(&mut self, current: &Self::SchedItem) -> bool {
        current.task_tick();
        if self.ready_queue.is_empty() {
            return false;
        }
        self.min_vruntime.is_none()
            || current.get_vruntime() > self.min_vruntime.as_mut().unwrap().load(Ordering::Acquire)
    }

    fn set_priority(&mut self, task: &Self::SchedItem, prio: isize) -> bool {
        if (-20..=19).contains(&prio) {
            task.set_priority(prio);
            true
        } else {
            false
        }
    }
}

impl<T> Default for CFScheduler<T> {
    fn default() -> Self {
        Self::new()
    }
}

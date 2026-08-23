// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::{QueueInstanceCompletion, WorkColor};

/// Linux-like accounting state for one logical queue to execution-pool binding.
///
/// This is the X-Kernel counterpart of the active/in-flight/color fields in
/// Linux `struct pool_workqueue`. Pending entries live in the shared pool; this
/// state remains queue-local for max-active throttling and flush accounting.
pub(crate) struct WorkQueuePoolState {
    max_active: usize,
    nr_active: usize,
    nr_running: usize,
    work_color: WorkColor,
    flush_color: Option<WorkColor>,
    flush_id: Option<usize>,
    #[cfg(unittest)]
    next_flush_id: usize,
    nr_in_flight: [usize; WorkColor::COUNT],
}

/// Captured accounting data for one committed queue instance.
///
/// The color is sampled before the work is made visible in a queue, matching
/// Linux's rule that a work item belongs to the workqueue color active at
/// enqueue time. The idle bit is carried with the same snapshot so the caller
/// can reinitialize queue-idle waiters only for the empty -> non-empty edge.
pub(crate) struct WorkQueuePoolAccountingCommit {
    was_idle: bool,
    color: WorkColor,
}

impl WorkQueuePoolAccountingCommit {
    pub(crate) fn capture(binding: &WorkQueuePoolState, was_idle: bool) -> Self {
        Self {
            was_idle,
            color: binding.work_color(),
        }
    }

    pub(crate) fn color(&self) -> WorkColor {
        self.color
    }

    pub(crate) fn commit(self, binding: &mut WorkQueuePoolState) -> bool {
        binding.inc_in_flight(self.color);
        self.was_idle
    }
}

impl WorkQueuePoolState {
    pub(crate) const fn new() -> Self {
        Self {
            max_active: usize::MAX,
            nr_active: 0,
            nr_running: 0,
            work_color: WorkColor::DEFAULT,
            flush_color: None,
            flush_id: None,
            #[cfg(unittest)]
            next_flush_id: 1,
            nr_in_flight: [0; WorkColor::COUNT],
        }
    }

    pub(crate) fn configure_max_active(&mut self, max_active: usize) {
        self.max_active = max_active.max(1);
    }

    pub(crate) fn reset_active_to_running(&mut self) -> usize {
        self.nr_active = self.nr_running;
        self.max_active.saturating_sub(self.nr_running)
    }

    pub(crate) fn can_activate(&self) -> bool {
        self.nr_active < self.max_active
    }

    pub(crate) fn add_active(&mut self) {
        self.nr_active += 1;
    }

    pub(crate) fn remove_active(&mut self) {
        self.nr_active = self.nr_active.saturating_sub(1);
    }

    pub(crate) fn start_running(&mut self) {
        self.nr_running += 1;
    }

    pub(crate) fn finish_active_work(&mut self) {
        self.nr_running = self.nr_running.saturating_sub(1);
        self.nr_active = self.nr_active.saturating_sub(1);
    }

    pub(crate) fn has_running(&self) -> bool {
        self.nr_running != 0
    }

    pub(crate) fn has_in_flight(&self) -> bool {
        self.nr_in_flight.iter().any(|count| *count != 0)
    }

    pub(crate) fn is_idle(&self) -> bool {
        !self.has_running() && !self.has_in_flight()
    }

    #[cfg(unittest)]
    pub(crate) fn active_count_for_tests(&self) -> usize {
        self.nr_active
    }

    pub(crate) fn work_color(&self) -> WorkColor {
        self.work_color
    }

    #[cfg(unittest)]
    pub(crate) fn advance_work_color(&mut self) -> WorkColor {
        let color = self.work_color;
        self.work_color = self.work_color.next();
        color
    }

    pub(crate) fn set_work_color(&mut self, color: WorkColor) {
        self.work_color = color;
    }

    pub(crate) fn has_in_flight_color(&self, color: WorkColor) -> bool {
        self.nr_in_flight[color.index()] != 0
    }

    #[cfg(unittest)]
    pub(crate) fn has_active_flush(&self) -> bool {
        self.flush_color.is_some()
    }

    #[cfg(unittest)]
    pub(crate) fn begin_flush(&mut self, color: WorkColor) -> Option<usize> {
        if self.nr_in_flight[color.index()] == 0 {
            return None;
        }
        let flush_id = self.next_flush_id;
        self.next_flush_id = self.next_flush_id.wrapping_add(1).max(1);
        self.flush_color = Some(color);
        self.flush_id = Some(flush_id);
        Some(flush_id)
    }

    #[cfg(unittest)]
    pub(crate) fn is_flush_active(&self, flush_id: usize) -> bool {
        self.flush_id == Some(flush_id)
    }

    pub(crate) fn inc_in_flight(&mut self, color: WorkColor) {
        self.nr_in_flight[color.index()] += 1;
    }

    pub(crate) fn dec_in_flight(&mut self, color: WorkColor) -> (bool, bool) {
        let count = &mut self.nr_in_flight[color.index()];
        if *count == 0 {
            return (false, false);
        }
        *count -= 1;
        if self.flush_color == Some(color) && *count == 0 {
            self.flush_color = None;
            self.flush_id = None;
            return (true, true);
        }
        (false, *count == 0)
    }

    pub(crate) fn complete_work_and_linked_barriers(
        &mut self,
        color: WorkColor,
        barrier_count: usize,
    ) -> QueueInstanceCompletion {
        let (mut flush_completed, mut color_drained) = self.dec_in_flight(color);
        for _ in 0..barrier_count {
            let (barrier_flush_completed, barrier_color_drained) = self.dec_in_flight(color);
            flush_completed |= barrier_flush_completed;
            color_drained |= barrier_color_drained;
        }
        QueueInstanceCompletion::new(
            self.is_idle(),
            flush_completed,
            color_drained.then_some(color),
        )
    }

    #[cfg(unittest)]
    pub(crate) fn in_flight_for_tests(&self, color: WorkColor) -> usize {
        self.nr_in_flight[color.index()]
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kpoll::{Completion, PollEvent};

use crate::{WorkColor, WorkQueuePoolBinding, WorkqueueError, WorkqueueSyncWaitIf};

/// Lazily allocated wait sources for sleepable queue synchronization.
///
/// Queueing remains allocation-free and IRQ-safe. The completion/event objects
/// only exist after a caller uses a sleepable API such as flush or destroy.
pub(crate) struct WorkQueueSyncState {
    idle: Completion,
    flush_event: PollEvent,
}

impl WorkQueueSyncState {
    pub(crate) fn new() -> Self {
        let idle = Completion::new();
        idle.complete_all();
        Self {
            idle,
            flush_event: PollEvent::new(),
        }
    }

    pub(crate) fn idle_completion(&self) -> &Completion {
        &self.idle
    }

    pub(crate) fn flush_event(&self) -> &PollEvent {
        &self.flush_event
    }
}

#[derive(Clone, Copy)]
pub(crate) enum QueueColorFlush {
    Done,
    Wait(WorkColor),
    Overflow(WorkColor),
}

pub(crate) fn prepare_queue_color_flush(bindings: &[WorkQueuePoolBinding]) -> QueueColorFlush {
    let queue = bindings
        .first()
        .map(|binding| binding.owner())
        .expect("queue flush should have at least one per-cpu binding");
    let queue = queue.queue();
    let mut queue_state = queue.state.lock();
    let mut guards = alloc::vec::Vec::with_capacity(bindings.len());
    for binding in bindings {
        guards.push(binding.state().lock());
    }

    let flush_color = guards
        .first()
        .map_or(WorkColor::DEFAULT, |binding| binding.work_color());
    let next_color = flush_color.next();
    let next_color_in_flight = guards
        .iter()
        .any(|binding| binding.has_in_flight_color(next_color));
    if next_color_in_flight {
        return QueueColorFlush::Overflow(next_color);
    }

    let should_wait = guards
        .iter()
        .any(|binding| binding.has_in_flight_color(flush_color));
    for binding in &mut guards {
        binding.set_work_color(next_color);
    }
    if should_wait {
        queue_state.arm_queue_flush_color(flush_color);
        QueueColorFlush::Wait(flush_color)
    } else {
        QueueColorFlush::Done
    }
}

pub(crate) fn wait_for_queue_color_flush(
    bindings: &[WorkQueuePoolBinding],
    color: WorkColor,
) -> Result<(), WorkqueueError> {
    let queue = bindings
        .first()
        .map(|binding| binding.owner())
        .expect("queue flush should have at least one per-cpu binding");
    let sync = queue.queue().sync();
    loop {
        let observed_generation = sync.flush_event().generation();
        if queue_color_is_drained(bindings, color) {
            return Ok(());
        }
        WorkqueueSyncWaitIf::wait_for_completion_or_event(
            sync.idle_completion(),
            sync.flush_event(),
            observed_generation,
        )
        .map_err(|_| WorkqueueError::WaitFailed)?;
    }
}

fn queue_color_is_drained(bindings: &[WorkQueuePoolBinding], color: WorkColor) -> bool {
    bindings
        .iter()
        .all(|binding| !binding.state().lock().has_in_flight_color(color))
}

pub(crate) struct QueueInstanceCompletion {
    is_idle: bool,
    flush_completed: bool,
    drained_color: Option<WorkColor>,
}

impl QueueInstanceCompletion {
    pub(crate) fn new(
        is_idle: bool,
        flush_completed: bool,
        drained_color: Option<WorkColor>,
    ) -> Self {
        Self {
            is_idle,
            flush_completed,
            drained_color,
        }
    }

    pub(crate) fn is_idle(&self) -> bool {
        self.is_idle
    }

    pub(crate) fn flush_completed(&self) -> bool {
        self.flush_completed
    }

    pub(crate) fn drained_color(&self) -> Option<WorkColor> {
        self.drained_color
    }
}

#[derive(Default)]
pub(crate) struct QueueWake {
    idle: Option<kpoll::PollSet>,
    flush: Option<kpoll::PollSet>,
}

impl QueueWake {
    pub(crate) fn new(idle: Option<kpoll::PollSet>, flush: Option<kpoll::PollSet>) -> Self {
        Self { idle, flush }
    }

    pub(crate) fn wake(self) {
        if let Some(idle) = self.idle {
            let _ = idle.wake();
        }
        if let Some(flush) = self.flush {
            let _ = flush.wake();
        }
    }
}

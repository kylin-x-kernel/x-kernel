// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Reusable work-item state without callback storage.

use core::{
    num::NonZeroUsize,
    sync::atomic::{AtomicUsize, Ordering},
};

use kspin::SpinNoIrq;

use crate::{EntryKey, EntryOwner, WorkColor, WorkInstanceId, WorkKey, id::PendingRecordId};

/// A reusable work item.
pub struct Work {
    state: SpinNoIrq<WorkState>,
    key_generation: AtomicUsize,
}

impl Work {
    pub const fn new() -> Self {
        Self {
            state: SpinNoIrq::new(WorkState::new()),
            key_generation: AtomicUsize::new(0),
        }
    }

    pub fn key(&self) -> WorkKey {
        WorkKey::from_parts(self, self.key_generation())
    }

    fn key_generation(&self) -> NonZeroUsize {
        if let Some(generation) = NonZeroUsize::new(self.key_generation.load(Ordering::Acquire)) {
            return generation;
        }

        let generation = NEXT_WORK_KEY_GENERATION
            .fetch_add(1, Ordering::AcqRel)
            .checked_add(1)
            .expect("work key generation exhausted");
        match self.key_generation.compare_exchange(
            0,
            generation,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => NonZeroUsize::new(generation).expect("work key generation is non-zero"),
            Err(current) => NonZeroUsize::new(current).expect("work key generation is non-zero"),
        }
    }

    pub(crate) fn state(&self) -> &SpinNoIrq<WorkState> {
        &self.state
    }

    pub fn status(&self) -> WorkStatus {
        self.state.lock().kind.status()
    }

    pub fn disable(&self) -> Result<usize, DisableWorkError> {
        let mut state = self.state.lock();
        state.disable_depth = state
            .disable_depth
            .checked_add(1)
            .ok_or(DisableWorkError::Overflow)?;
        Ok(state.disable_depth)
    }

    pub fn enable(&self) -> Result<usize, EnableWorkError> {
        let mut state = self.state.lock();
        if state.disable_depth == 0 {
            return Err(EnableWorkError::NotDisabled);
        }
        state.disable_depth -= 1;
        Ok(state.disable_depth)
    }

    pub fn is_disabled(&self) -> bool {
        self.state.lock().disable_depth != 0
    }
}

static NEXT_WORK_KEY_GENERATION: AtomicUsize = AtomicUsize::new(0);

impl Default for Work {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) struct WorkState {
    pub disable_depth: usize,
    next_instance: WorkInstanceId,
    pub kind: WorkStateKind,
}

impl WorkState {
    const fn new() -> Self {
        Self {
            disable_depth: 0,
            next_instance: WorkInstanceId::FIRST,
            kind: WorkStateKind::Idle,
        }
    }

    pub(crate) fn alloc_instance(&mut self) -> WorkInstanceId {
        let instance = self.next_instance;
        self.next_instance = self.next_instance.next();
        instance
    }
}

pub(crate) enum WorkStateKind {
    Idle,
    DelayedPending { target_owner: EntryOwner },
    Pending(PendingWorkState),
    Running(RunningWorkState),
}

impl WorkStateKind {
    fn status(&self) -> WorkStatus {
        match self {
            Self::Idle => WorkStatus::Idle,
            Self::DelayedPending { .. } => WorkStatus::DelayedPending,
            Self::Pending(_) => WorkStatus::Pending,
            Self::Running(_) => WorkStatus::Running,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct PendingWorkState {
    pub owner: EntryOwner,
    pub record: PendingRecordId,
    pub instance: WorkInstanceId,
}

#[derive(Clone, Copy)]
pub(crate) struct RunningWorkState {
    pub owner: EntryOwner,
    pub key: EntryKey,
    pub instance: WorkInstanceId,
    pub color: WorkColor,
    pub active: bool,
    pub worker_id: usize,
    pub worker_token: usize,
    pub canceling: bool,
    pub requeue: Option<RequeueWorkState>,
}

#[derive(Clone, Copy)]
pub(crate) struct RequeueWorkState {
    pub owner: EntryOwner,
    pub record: PendingRecordId,
    pub instance: WorkInstanceId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkStatus {
    Idle,
    DelayedPending,
    Pending,
    Running,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DisableWorkError {
    Overflow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnableWorkError {
    NotDisabled,
}

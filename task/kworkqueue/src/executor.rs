// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Executor-facing operations returned by the workqueue state machine.

use crate::{BindingId, EntryKey, EntryOwner, EntryPayload};

/// Entry submitted to the execution backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecutorEntry {
    pub binding: BindingId,
    pub owner: EntryOwner,
    pub key: EntryKey,
    pub payload: EntryPayload,
}

impl ExecutorEntry {
    pub(crate) const fn new(
        binding: BindingId,
        owner: EntryOwner,
        key: EntryKey,
        payload: EntryPayload,
    ) -> Self {
        Self {
            binding,
            owner,
            key,
            payload,
        }
    }
}

/// Operation the workqueue core asks the execution backend to perform.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutorOp {
    EnqueueRunnable(ExecutorEntry),
    EnqueueInactive(ExecutorEntry),
    Remove(ExecutorEntry),
    PromoteInactive { owner: EntryOwner, budget: usize },
}

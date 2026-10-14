// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Permanent ordinary admission closure and all-partition cleanup drain.

use alloc::vec::Vec;

use ktask::future::PreparedTaskWait;

use super::{Queue, control::COLOR_COUNT};
use crate::work::{
    observers::{CompletionObserver, CompletionObservers},
    wait::WaitError,
};

impl Queue {
    /// Closes admission and waits for all existing ordinary color obligations.
    ///
    /// Includes source detachment, but not provider stop. Diagnostic references
    /// retain only observation data and need no shutdown join.
    /// The caller must already stop delayed producers and drain their timers.
    /// Optional diagnostics reject a callback waiting on its own Queue. Context validation
    /// applies even if already closed/empty. All preparation precedes closure;
    /// repeated/concurrent callers retain independent Work receipts and wait on
    /// the same permanent source receipts with their own registrations.
    ///
    /// # Errors
    /// Returns context, identity-validation or registration errors before closure.
    /// No recoverable failure or interruption is permitted after closure.
    ///
    /// # Panics
    /// Propagates fatal allocation and violated internal observer-count invariants.
    pub fn close_and_drain(&self) -> Result<(), WaitError> {
        let mut task_wait = PreparedTaskWait::prepare().map_err(|_| WaitError::InvalidContext)?;
        #[cfg(feature = "self-wait-check")]
        self.reject_callback_self_wait()?;
        let waker = task_wait.waker();
        let mut receipts = Vec::new();
        let mut prepared = Vec::new();
        let mut source_receipts = Vec::new();
        for partition in &self.partitions {
            if let Some(receipt) = partition.source_detachment_completion() {
                let registration = receipt
                    .register_owned(&waker)
                    .map_err(WaitError::Registration)?;
                source_receipts.push((receipt, registration));
            }
        }
        for _ in &self.partitions {
            for _ in 0..COLOR_COUNT {
                let (receipt, token) = CompletionObserver::prepare();
                let registration = receipt.register(&waker).map_err(WaitError::Registration)?;
                receipts.push((receipt, registration));
                prepared.push(token);
            }
        }

        self.close_admission();
        let mut empty = CompletionObservers::new();
        for partition in &self.partitions {
            for color in 0..COLOR_COUNT {
                let token = prepared.pop().expect("one observer per partition/color");
                // The admission barrier includes a submitter that sampled open
                // before closure but had not yet published its count.
                if let Err(token) = partition.observe_batch(color, token) {
                    empty.attach(token);
                }
            }
        }
        empty.complete();
        while !receipts.iter().all(|(receipt, _)| receipt.is_completed()) {
            task_wait.park();
        }
        for partition in &self.partitions {
            partition.detach_source();
        }
        while !source_receipts
            .iter()
            .all(|(receipt, _)| receipt.is_completed())
        {
            task_wait.park();
        }
        Ok(())
    }
}

#[cfg(feature = "self-wait-check")]
impl Queue {
    /// Call only after sleepable-context validation, before any wait-scope effects.
    pub(super) fn reject_callback_self_wait(&self) -> Result<(), WaitError> {
        let task = ktask::current().owner_key();
        if self
            .partitions
            .iter()
            .any(|partition| partition.has_callback_on(task))
        {
            Err(WaitError::SelfWait)
        } else {
            Ok(())
        }
    }
}

unittest::path_mod_test!(
    shutdown_unittest,
    "../../ut-tests/queue_shutdown_unittest.rs"
);

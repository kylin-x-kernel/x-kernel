// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Common acceptance cutoff with bounded color reuse and independent old receipts.

use ktask::completion::CompletionWait;

use super::{Queue, color::ColorCompletion, control::COLOR_COUNT};
use crate::work::{observers::CompletionObservers, wait::WaitError};

impl Queue {
    /// Waits for ordinary acceptance before one common cutoff, including cleanup.
    ///
    /// Later acceptance does not extend the wait. A full color ring delays the
    /// cutoff, not admission; shutdown does not need this serializer or a color.
    /// Timer-only requests are not ordinary participations. Providers must keep
    /// draining; this is not cancellation, shutdown or proof of final idleness.
    ///
    /// # Errors
    /// Context and optional self-wait errors precede cutoff effects.
    ///
    /// # Panics
    /// Propagates fatal allocation, notification and batch-accounting invariant failures.
    pub fn flush(&self) -> Result<(), WaitError> {
        self.flush_with(|| {}, |_| {})
    }

    fn flush_with(
        &self,
        before_capture: impl FnOnce(),
        after_publish: impl FnOnce(usize),
    ) -> Result<(), WaitError> {
        let task_wait = CompletionWait::prepare().map_err(|_| WaitError::InvalidContext)?;
        #[cfg(feature = "self-wait-check")]
        self.reject_callback_self_wait()?;
        let (own, mut tokens) =
            ColorCompletion::prepare(self.partitions.len(), &self.color_progress);
        before_capture();
        let observed = loop {
            let mut sealed = self.sealed_colors.lock();
            let observed = sealed.clone();
            // Snapshot Arcs defer final receipt destruction past unlock.
            // A terminal local receipt has already left its in-flight observer chain.
            for (index, receipt) in observed.iter().enumerate() {
                if receipt
                    .as_ref()
                    .is_some_and(|receipt| receipt.is_completed())
                {
                    sealed[index] = None;
                }
            }
            let current = self.current_color();
            let next = (1..COLOR_COUNT)
                .map(|offset| (current + offset) % COLOR_COUNT)
                .find(|&color| sealed[color].is_none());
            let Some(next) = next else {
                drop(sealed);
                task_wait.wait_until(&self.color_progress, || {
                    observed
                        .iter()
                        .flatten()
                        .any(|receipt| receipt.is_completed())
                });
                continue;
            };
            assert!(sealed[current].is_none());
            sealed[current] = Some(own.clone());
            assert_eq!(self.publish_color(next), current);
            after_publish(current);
            let mut empty = CompletionObservers::new();
            for partition in &self.partitions {
                // Crossing this guard joins acceptors that sampled the old color
                // before cutoff but had not yet committed their local accounting.
                let token = tokens.pop().expect("one token per partition");
                if let Err(token) = partition.observe_batch(current, token) {
                    empty.attach(token);
                }
            }
            // No later flusher can recycle a partially visited color. Empty
            // completions and task wakeups are delivered after setup unlock.
            drop(sealed);
            empty.complete();
            break observed;
        };
        own.wait(&task_wait);
        for receipt in observed.iter().flatten() {
            receipt.wait(&task_wait);
        }
        Ok(())
    }
}

unittest::path_mod_test!(flush_unittest, "../../ut-tests/queue_flush_unittest.rs");

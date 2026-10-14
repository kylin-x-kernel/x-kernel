// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Common acceptance cutoff with bounded color reuse and independent old receipts.

use alloc::{sync::Arc, vec::Vec};
use core::task::Waker;

use kpoll::{PollRegisterError, PollRegistration};
use ktask::future::PreparedTaskWait;

use super::{Queue, color::ColorCompletion, control::COLOR_COUNT};
use crate::work::{
    observers::{CompletionObserver, CompletionObservers},
    wait::WaitError,
};

impl Queue {
    /// Waits for ordinary acceptance before one common cutoff, including cleanup.
    ///
    /// Later acceptance does not extend the wait. A full color ring delays the
    /// cutoff, not admission; shutdown does not need this serializer or a color.
    /// Timer-only requests are not ordinary participations. Providers must keep
    /// draining; this is not cancellation, shutdown or proof of final idleness.
    ///
    /// # Errors
    /// Context, optional self-wait and registration errors precede cutoff effects.
    ///
    /// # Panics
    /// Propagates fatal allocation, notification and batch-accounting invariant failures.
    pub fn flush(&self) -> Result<(), WaitError> {
        self.flush_with(CompletionObserver::register, |_| {})
    }

    // Private deterministic test hooks: registration failure and a late old-color
    // commit before partition barriers. No business hook is exposed by flush.
    fn flush_with(
        &self,
        mut register: impl FnMut(
            &CompletionObserver,
            &Waker,
        ) -> Result<PollRegistration, PollRegisterError>,
        after_publish: impl FnOnce(usize),
    ) -> Result<(), WaitError> {
        let mut task_wait = PreparedTaskWait::prepare().map_err(|_| WaitError::InvalidContext)?;
        #[cfg(feature = "self-wait-check")]
        self.reject_callback_self_wait()?;
        let waker = task_wait.waker();
        let (own, mut tokens) = ColorCompletion::prepare(self.partitions.len());
        let _own_registrations = own
            .register_with(&waker, &mut register)
            .map_err(WaitError::Registration)?;
        let (observed, _registrations) = loop {
            let observed = self.sealed_colors.lock().clone();
            let mut registrations = Vec::new();
            for receipt in observed.iter().flatten() {
                registrations.extend(
                    receipt
                        .register_with(&waker, &mut register)
                        .map_err(WaitError::Registration)?,
                );
            }
            let mut sealed = self.sealed_colors.lock();
            if !sealed
                .iter()
                .zip(&observed)
                .all(|(live, old)| match (live, old) {
                    (None, None) => true,
                    (Some(live), Some(old)) => Arc::ptr_eq(live, old),
                    _ => false,
                })
            {
                drop(sealed);
                continue;
            }
            // Snapshot Arcs defer final receipt/registration destruction past unlock.
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
                // Every occupied receipt is registered. Completion publishes a
                // durable predicate and wakes us even if no further submit occurs.
                drop(sealed);
                task_wait.park();
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
            // completions and any Waker callbacks are delivered after setup unlock.
            drop(sealed);
            empty.complete();
            break (observed, registrations);
        };
        while !own.is_completed()
            || !observed
                .iter()
                .flatten()
                .all(|receipt| receipt.is_completed())
        {
            task_wait.park();
        }
        Ok(())
    }
}

unittest::path_mod_test!(flush_unittest, "../../ut-tests/queue_flush_unittest.rs");

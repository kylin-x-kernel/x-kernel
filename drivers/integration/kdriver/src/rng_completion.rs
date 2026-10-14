// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel provider for RNG read-completion waiting.

use alloc::sync::Arc;

use driver_base::{DriverError, DriverResult};
use hwrng::completion::RngCompletion;
use kpoll::{PollRegisterError, PollSet};
use ktask::future::PreparedTaskWait;

/// Creates a shared completion capability for injection into one RNG driver.
///
/// Construct in task context. Allocation follows the kernel's Arc OOM policy.
/// The capability owns wake registrations, not request state or DMA storage.
pub fn new_rng_completion() -> Arc<dyn RngCompletion> {
    Arc::new(HostRngCompletion {
        waiters: PollSet::new(),
    })
}

struct HostRngCompletion {
    waiters: PollSet,
}

impl RngCompletion for HostRngCompletion {
    fn wait_until(&self, ready: &mut dyn FnMut() -> bool) -> DriverResult {
        let mut task_wait = PreparedTaskWait::prepare().map_err(|_| DriverError::InvalidInput)?;
        if ready() {
            return Ok(());
        }

        let waker = task_wait.waker();

        loop {
            // Recheck after registration to avoid a missed wake.
            // notify() removes this registration when it wakes the task.
            // After park(), drop any remaining registration and retry.
            let registration = self.waiters.register(&waker).map_err(registration_error)?;

            if ready() {
                return Ok(());
            }
            task_wait.park();
            drop(registration);
        }
    }

    fn notify(&self) {
        self.waiters.wake();
    }
}

fn registration_error(error: PollRegisterError) -> DriverError {
    match error {
        PollRegisterError::NoMemory | PollRegisterError::IdExhausted => DriverError::NoMemory,
        PollRegisterError::InvalidState => DriverError::InvalidInput,
    }
}

unittest::path_mod_test!(
    rng_completion_unittest,
    "../ut-tests/rng_completion_unittest.rs"
);

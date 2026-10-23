// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel provider for RNG read-completion waiting.

use alloc::sync::Arc;

use driver_base::{DriverError, DriverResult};
use hwrng::completion::RngCompletion;
use kcompletion::WaitQueue;
use ktask::completion::CompletionWait;

/// Creates a shared completion capability for injection into one RNG driver.
///
/// Construct in task context. Allocation follows the kernel's Arc OOM policy.
/// The capability owns the queue; waiting stacks own scoped nodes. It owns
/// neither request state nor DMA storage.
pub fn new_rng_completion() -> Arc<dyn RngCompletion> {
    Arc::new(HostRngCompletion {
        waiters: WaitQueue::new(),
    })
}

struct HostRngCompletion {
    waiters: WaitQueue,
}

impl RngCompletion for HostRngCompletion {
    fn wait_until(&self, ready: &mut dyn FnMut() -> bool) -> DriverResult {
        let task_wait = CompletionWait::prepare().map_err(|_| DriverError::InvalidInput)?;
        task_wait.wait_until(&self.waiters, ready);
        Ok(())
    }

    fn notify(&self) {
        self.waiters.notify_all();
    }
}

unittest::path_mod_test!(
    rng_completion_unittest,
    "../ut-tests/rng_completion_unittest.rs"
);

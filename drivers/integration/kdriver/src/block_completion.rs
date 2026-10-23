// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel waiting provider for portable block transactions.

use alloc::{boxed::Box, sync::Arc};

use block::completion::{BlockSignals, BlockWaitProvider, BlockWaiter};
use driver_base::{DriverError, DriverResult};
use kcompletion::Completion;
use ktask::completion::CompletionWait;

pub(crate) fn prepare_block_wait() -> DriverResult<Box<dyn BlockWaiter>> {
    Ok(Box::new(HostBlockWaiter::prepare()?))
}

pub(crate) struct HostBlockWaitProvider;

impl BlockWaitProvider for HostBlockWaitProvider {
    fn validate_context(&self) -> DriverResult {
        if ktask::current_may_uninit().is_none() || !ktask::is_sleepable_context() {
            return Err(DriverError::InvalidInput);
        }
        Ok(())
    }

    fn prepare_wait(&self) -> Box<dyn BlockWaiter> {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: BioWait validates the current task \
                      context before calling the provider; allocation is infallible"
        )]
        prepare_block_wait().expect("validated bio wait context")
    }
}

struct HostSignals {
    terminal: Completion,
}

impl BlockSignals for HostSignals {
    fn notify_completion(&self) {
        self.terminal.complete_all();
    }
}

struct HostBlockWaiter {
    signals: Arc<HostSignals>,
    task: CompletionWait,
}

impl HostBlockWaiter {
    fn prepare() -> DriverResult<Self> {
        let task = CompletionWait::prepare().map_err(|_| DriverError::InvalidInput)?;
        let signals = Arc::new(HostSignals {
            terminal: Completion::new(),
        });
        Ok(Self { signals, task })
    }
}

impl BlockWaiter for HostBlockWaiter {
    fn signals(&self) -> Arc<dyn BlockSignals> {
        self.signals.clone()
    }

    fn wait_completion(&mut self) {
        self.task.wait(&self.signals.terminal);
    }
}

unittest::path_mod_test!(
    block_completion_unittest,
    "../ut-tests/block_completion_unittest.rs"
);

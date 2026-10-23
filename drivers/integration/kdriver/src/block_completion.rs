// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel waiting provider for portable block transactions.

use alloc::{boxed::Box, sync::Arc};
use core::marker::PhantomData;

use block::completion::{BlockSignals, BlockWaitProvider, BlockWaiter};
use driver_base::{DriverError, DriverResult};
use kpoll::Completion;
use ktask::{KtaskRef, future::PreparedTaskWait};

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
    task: KtaskRef,
    _task_bound: PhantomData<*mut ()>,
}

impl HostBlockWaiter {
    fn prepare() -> DriverResult<Self> {
        HostBlockWaitProvider.validate_context()?;
        let signals = Arc::new(HostSignals {
            terminal: Completion::new(),
        });
        Ok(Self {
            signals,
            task: ktask::current().clone(),
            _task_bound: PhantomData,
        })
    }

    fn wait_completion_before_park(&mut self, mut before_park: impl FnMut(&PreparedTaskWait)) {
        assert!(ktask::current().ptr_eq(&self.task));
        assert!(ktask::is_sleepable_context());
        // Inline completion needs no task waker or wait registration. The
        // permanent completion also retains a signal arriving before enrollment.
        if self.signals.terminal.try_wait() {
            return;
        }
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: task identity and sleepable context \
                      were checked above; this private source has exactly one registration, which \
                      fits PollSet inline storage and cannot exhaust registration IDs"
        )]
        let (mut task, _registration) = {
            let task = PreparedTaskWait::prepare().expect("validated waiting task");
            let registration = self
                .signals
                .terminal
                .register_owned(&task.waker())
                .expect("one private completion waiter fits inline storage");
            (task, registration)
        };
        // Recheck after enrollment; retain the same registration through
        // spurious task wakes. Neither notification nor waiting resets it.
        while !self.signals.terminal.try_wait() {
            before_park(&task);
            task.park();
        }
    }
}

impl BlockWaiter for HostBlockWaiter {
    fn signals(&self) -> Arc<dyn BlockSignals> {
        self.signals.clone()
    }

    fn wait_completion(&mut self) {
        self.wait_completion_before_park(|_| {});
    }
}

unittest::path_mod_test!(
    block_completion_unittest,
    "../ut-tests/block_completion_unittest.rs"
);

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel waiting provider for portable block transactions.

use alloc::{boxed::Box, sync::Arc};

use block::completion::{BlockSignals, BlockWaiter};
use driver_base::{DriverError, DriverResult};
use kpoll::{Completion, PollRegisterError, PollRegistration};
use ktask::future::PreparedTaskWait;

#[cfg_attr(all(not(unittest), not(feature = "virtio-blk")), expect(dead_code))]
pub(crate) fn prepare_block_wait() -> DriverResult<Box<dyn BlockWaiter>> {
    Ok(Box::new(HostBlockWaiter::prepare()?))
}

struct HostSignals {
    admission: Completion,
    terminal: Completion,
}

impl BlockSignals for HostSignals {
    fn notify_admission(&self) {
        self.admission.complete();
    }

    fn notify_completion(&self) {
        self.terminal.complete_all();
    }
}

struct HostBlockWaiter {
    // Cancel the registration before releasing either the source or the task.
    _terminal_registration: PollRegistration,
    signals: Arc<HostSignals>,
    task: PreparedTaskWait,
}

impl HostBlockWaiter {
    fn prepare() -> DriverResult<Self> {
        let task = PreparedTaskWait::prepare().map_err(|_| DriverError::InvalidInput)?;
        let signals = Arc::new(HostSignals {
            admission: Completion::new(),
            terminal: Completion::new(),
        });
        let waker = task.waker();
        let registration = signals
            .terminal
            .register_owned(&waker)
            .map_err(registration_error)?;
        Ok(Self {
            _terminal_registration: registration,
            signals,
            task,
        })
    }
}

impl BlockWaiter for HostBlockWaiter {
    fn signals(&self) -> Arc<dyn BlockSignals> {
        self.signals.clone()
    }

    fn wait_admission(&mut self) -> DriverResult<()> {
        let waker = self.task.waker();
        loop {
            if self.signals.admission.try_wait() {
                return Ok(());
            }
            let _registration = self
                .signals
                .admission
                .register_owned(&waker)
                .map_err(registration_error)?;
            if self.signals.admission.try_wait() {
                return Ok(());
            }
            self.task.park();
        }
    }

    fn wait_completion(&mut self) {
        // The private terminal source never resets. Keep its initial
        // registration even when admission or an unrelated task wake fires.
        while !self.signals.terminal.try_wait() {
            self.task.park();
        }
    }
}

fn registration_error(error: PollRegisterError) -> DriverError {
    match error {
        PollRegisterError::NoMemory | PollRegisterError::IdExhausted => DriverError::NoMemory,
        PollRegisterError::InvalidState => DriverError::InvalidInput,
    }
}

unittest::path_mod_test!(
    block_completion_unittest,
    "../ut-tests/block_completion_unittest.rs"
);

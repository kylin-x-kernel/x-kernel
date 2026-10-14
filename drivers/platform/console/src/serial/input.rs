// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Bounded UART receive storage, exclusively accessed under the port lock.

use alloc::{collections::TryReserveError, sync::Arc};
use core::task::Waker;

use ringbuf::{
    LocalRb,
    storage::Heap,
    traits::{Consumer, Producer},
};

const INPUT_CAPACITY_BYTES: usize = 4096;
pub(super) const RECEIVE_BUDGET_BYTES: usize = 256;

pub(super) struct SerialInput {
    bytes: LocalRb<Heap<u8>>,
    pub(super) dropped_bytes: u64,
    pub(super) waker: Option<Arc<Waker>>,
}

impl SerialInput {
    pub(super) fn try_new() -> Result<Self, TryReserveError> {
        Ok(Self {
            bytes: LocalRb::try_new(INPUT_CAPACITY_BYTES)?,
            dropped_bytes: 0,
            waker: None,
        })
    }

    /// Drains at most one receive budget, including bytes lost to overflow.
    /// The source must be nonblocking and must not reenter the port lock.
    pub(super) fn collect(&mut self, mut receive: impl FnMut() -> Option<u8>) -> usize {
        let mut received = 0;
        for _ in 0..RECEIVE_BUDGET_BYTES {
            let Some(byte) = receive() else { break };
            received += 1;
            if self.bytes.try_push(byte).is_err() {
                self.dropped_bytes = self.dropped_bytes.saturating_add(1);
            }
        }
        received
    }

    /// Consumes older buffered bytes before sampling the hardware tail.
    pub(super) fn read(&mut self, output: &mut [u8], receive: impl FnMut() -> Option<u8>) -> usize {
        let copied = self.bytes.pop_slice(output);
        if copied == output.len() {
            return copied;
        }
        self.collect(receive);
        copied + self.bytes.pop_slice(&mut output[copied..])
    }
}

unittest::path_mod_test!(input_unittest, "../../ut-tests/serial_input_unittest.rs");

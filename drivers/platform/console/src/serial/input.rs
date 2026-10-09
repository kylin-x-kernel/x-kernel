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

#[unittest::mod_test]
mod tests {
    use unittest::{assert, assert_eq, def_test};

    use super::*;

    #[def_test]
    fn receive_budget_leaves_hardware_tail_for_next_round() {
        let mut input = SerialInput::try_new().unwrap();
        let mut source = (0..RECEIVE_BUDGET_BYTES + 3).map(|n| n as u8);
        assert_eq!(input.collect(|| source.next()), RECEIVE_BUDGET_BYTES);
        assert_eq!(source.len(), 3);
        let mut output = [0; RECEIVE_BUDGET_BYTES + 3];
        assert_eq!(input.read(&mut output, || source.next()), output.len());
        for (n, byte) in output.iter().enumerate() {
            assert_eq!(*byte, n as u8);
        }
        assert_eq!(input.dropped_bytes, 0);
    }

    #[def_test]
    fn empty_read_does_not_sample_hardware_or_consume_buffer() {
        let mut input = SerialInput::try_new().unwrap();
        assert_eq!(input.collect(|| None), 0);
        assert_eq!(input.collect(|| Some(42)), RECEIVE_BUDGET_BYTES);
        assert_eq!(
            input.read(&mut [], || panic!("unexpected hardware read")),
            0
        );
        let mut output = [0];
        assert_eq!(input.read(&mut output, || None), 1);
        assert_eq!(output, [42]);
    }

    #[def_test]
    fn overflow_preserves_old_bytes_and_recovers_after_consumption() {
        let mut input = SerialInput::try_new().unwrap();
        let mut source = (0..INPUT_CAPACITY_BYTES).map(|n| n as u8);
        while source.len() > 0 {
            input.collect(|| source.next());
        }
        let mut excess = [99; 7].into_iter();
        assert_eq!(input.collect(|| excess.next()), 7);
        assert_eq!(input.dropped_bytes, 7);

        let mut prefix = [0; 13];
        assert_eq!(input.read(&mut prefix, || None), prefix.len());
        let mut newer = [201; 13].into_iter();
        assert_eq!(input.collect(|| newer.next()), 13);
        let mut rest = [0; INPUT_CAPACITY_BYTES];
        assert_eq!(input.read(&mut rest, || None), rest.len());
        for (n, byte) in prefix
            .iter()
            .chain(rest[..INPUT_CAPACITY_BYTES - 13].iter())
            .enumerate()
        {
            assert_eq!(*byte, n as u8);
        }
        assert_eq!(&rest[INPUT_CAPACITY_BYTES - 13..], &[201; 13]);
        assert_eq!(input.dropped_bytes, 7);
        assert_eq!(input.read(&mut rest, || None), 0);
    }

    #[def_test]
    fn tail_sampling_is_bounded_even_with_a_large_output_slice() {
        let mut input = SerialInput::try_new().unwrap();
        let mut output = [0; RECEIVE_BUDGET_BYTES * 2];
        let mut samples = 0;
        assert_eq!(
            input.read(&mut output, || {
                samples += 1;
                Some(17)
            }),
            RECEIVE_BUDGET_BYTES
        );
        assert_eq!(samples, RECEIVE_BUDGET_BYTES);
        assert!(output[..RECEIVE_BUDGET_BYTES].iter().all(|b| *b == 17));
    }

    #[def_test]
    fn separate_inputs_and_saturating_overflow_counts() {
        let mut a = SerialInput::try_new().unwrap();
        let mut b = SerialInput::try_new().unwrap();
        for _ in 0..INPUT_CAPACITY_BYTES / RECEIVE_BUDGET_BYTES {
            a.collect(|| Some(1));
        }
        a.dropped_bytes = u64::MAX - 1;
        a.collect(|| Some(2));
        assert_eq!(a.dropped_bytes, u64::MAX);
        assert_eq!(b.collect(|| None), 0);
        assert_eq!(b.dropped_bytes, 0);
        let mut output = [0];
        assert_eq!(b.read(&mut output, || None), 0);
    }
}

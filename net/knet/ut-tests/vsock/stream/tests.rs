// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;

use kerrno::KError;
use kio::{IoBuf, Read};
use unittest::{assert, assert_eq};

use super::*;

struct RecordingReader {
    remaining: usize,
    requested: Vec<usize>,
}

impl Read for RecordingReader {
    fn read(&mut self, buf: &mut [u8]) -> KResult<usize> {
        let count = self.remaining.min(buf.len());
        buf[..count].fill(0x5a);
        self.remaining -= count;
        self.requested.push(buf.len());
        Ok(count)
    }
}

impl IoBuf for RecordingReader {
    fn remaining(&self) -> usize {
        self.remaining
    }
}

#[unittest::def_test]
fn read_tx_chunk_reads_up_to_vsock_packet_limit() {
    let payload_len = 2 * VSOCK_SEND_CHUNK_SIZE;
    let mut src = RecordingReader {
        remaining: payload_len,
        requested: Vec::new(),
    };
    let mut tx_state = TxState::new();

    let first = read_tx_chunk(&mut src, &mut tx_state, VSOCK_SEND_CHUNK_SIZE).unwrap();
    assert_eq!(first, VSOCK_SEND_CHUNK_SIZE);
    assert_eq!(src.requested, [VSOCK_SEND_CHUNK_SIZE]);
    assert_eq!(tx_state.buffer.len(), VSOCK_SEND_CHUNK_SIZE);

    let second = read_tx_chunk(&mut src, &mut tx_state, VSOCK_SEND_CHUNK_SIZE).unwrap();
    assert_eq!(second, VSOCK_SEND_CHUNK_SIZE);
    assert_eq!(
        src.requested,
        [VSOCK_SEND_CHUNK_SIZE, VSOCK_SEND_CHUNK_SIZE]
    );
    assert_eq!(src.remaining, 0);
}

#[unittest::def_test]
fn read_tx_chunk_preserves_small_final_chunk() {
    let mut src = RecordingReader {
        remaining: 31,
        requested: Vec::new(),
    };
    let mut tx_state = TxState::new();

    assert_eq!(read_tx_chunk(&mut src, &mut tx_state, VSOCK_SEND_CHUNK_SIZE), Ok(31));
    assert_eq!(src.requested, [31]);
    assert_eq!(read_tx_chunk(&mut src, &mut tx_state, VSOCK_SEND_CHUNK_SIZE), Ok(0));
}

#[unittest::def_test]
fn chunk_send_accepts_a_complete_packet() {
    assert_eq!(send_tx_chunk(b"abc", |_| Ok(3)), Ok(3));
}

#[unittest::def_test]
fn chunk_send_rejects_partial_acceptance() {
    assert!(matches!(
        send_tx_chunk(b"abc", |_| Ok(2)),
        Err(KError::InvalidInput)
    ));
}

#[unittest::def_test]
fn chunk_send_without_progress_is_an_io_error() {
    assert!(matches!(send_tx_chunk(b"abc", |_| Ok(0)), Err(KError::Io)));
}

#[unittest::def_test]
fn chunk_send_forwards_would_block() {
    assert!(matches!(
        send_tx_chunk(b"abc", |_| Err(KError::WouldBlock)),
        Err(KError::WouldBlock)
    ));
}

#[unittest::def_test]
fn effective_send_chunk_clamps_zero_to_one() {
    // When peer_buf_alloc == 0 the chunk must be at least 1 so the send
    // enters the credit-wait path instead of returning Ok(0) (EOF) or
    // InvalidInput.  Removing .max(1) makes this return 0 → test fails.
    assert_eq!(effective_send_chunk(0), 1);
    assert_eq!(effective_send_chunk(1), 1);
    assert_eq!(effective_send_chunk(64 * 1024), 64 * 1024);
    assert_eq!(effective_send_chunk(256 * 1024), 64 * 1024);
}

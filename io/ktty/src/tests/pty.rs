// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;
use core::{future::poll_fn, task::Poll};

use kpoll::PollRegistrations;
use ktask::future::block_on;
use unittest::def_test;

use super::{PtyDriver, create_pty_pair};
use crate::port::TtyWrite;

fn read_bytes(tty: &PtyDriver, length: usize) -> Vec<u8> {
    let mut received = Vec::new();
    let mut registrations = PollRegistrations::new();
    while received.len() < length {
        block_on(poll_fn(|cx| {
            let mut input = tty.input.lock();
            input.register_rx(&mut registrations.context(cx)).unwrap();
            if input.poll_read() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }));
        let mut bytes = [0; 80];
        let count = tty.input.lock().read(&mut bytes).unwrap();
        received.extend_from_slice(&bytes[..count]);
    }
    received
}

#[def_test]
fn pty_work_preserves_input_echo_and_output_space_resume() {
    let (master, slave) = create_pty_pair();
    let mut input = alloc::vec![b'a'; 700];
    input.push(b'\n');
    master.writer.write(&input);
    assert_eq!(read_bytes(&slave, input.len()), input);

    // Echo is produced by the slave work and remains readable at the master.
    let echo = read_bytes(&master, input.len() + 1);
    assert_eq!(&echo[..700], &input[..700]);
    assert_eq!(&echo[700..], b"\r\n");

    slave.writer.write(b"reply");
    assert_eq!(read_bytes(&master, 5), b"reply");
}

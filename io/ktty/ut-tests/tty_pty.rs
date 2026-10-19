// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, task::Wake, vec::Vec};
use core::{
    future::{Future, poll_fn},
    pin::pin,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll, Waker},
};

use kerrno::KError;
use kpoll::{IoEvents, PollRegistrations, Pollable};
use ktask::future::block_on;
use linux_raw_sys::general::{TCIFLUSH, TCIOFLUSH, TCOFLUSH};
use unittest::def_test;

use super::{
    PtyDriver, Tty, TtyKind, create_pty_pair,
    pty::{PtyReader, PtyWriter},
};
use crate::{
    port::{InputIngress, ProcessMode, TtyConfig, TtyWrite},
    terminal::Terminal,
};

// Manual processing keeps queue occupancy deterministic while using the real
// PTY transport, output discipline, poll registration and blocking-write loop.
fn manual_pty_pair() -> (Arc<PtyDriver>, Arc<PtyDriver>) {
    let master_input = Arc::new(InputIngress::default());
    let slave_input = Arc::new(InputIngress::default());
    let terminal = Arc::new(Terminal::default());
    let master = Tty::new(
        terminal.clone(),
        TtyKind::PtyMaster,
        TtyConfig {
            reader: PtyReader::new(master_input.clone()),
            writer: PtyWriter::new(slave_input.clone()),
            process_mode: ProcessMode::None(master_input.readable()),
        },
    );
    let slave = Tty::new(
        terminal,
        TtyKind::Terminal,
        TtyConfig {
            reader: PtyReader::new(slave_input),
            writer: PtyWriter::new(master_input),
            process_mode: ProcessMode::Manual,
        },
    );
    (master, slave)
}

#[derive(Default)]
struct WriterWake(AtomicBool);

impl Wake for WriterWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.store(true, Ordering::Relaxed);
    }
}

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
    master.output.lock().write(&input).unwrap();
    assert_eq!(read_bytes(&slave, input.len()), input);

    // Echo is produced by the slave work and remains readable at the master.
    let echo = read_bytes(&master, input.len() + 1);
    assert_eq!(&echo[..700], &input[..700]);
    assert_eq!(&echo[700..], b"\r\n");

    slave.output.lock().write(b"reply").unwrap();
    assert_eq!(read_bytes(&master, 5), b"reply");
}

#[def_test]
fn pty_blocking_write_delivers_more_than_ingress_capacity() {
    let (master, slave) = manual_pty_pair();
    let capacity = master.writer.write_room();
    let mut data = alloc::vec![b'a'; capacity * 2 + 64];
    for line in data.chunks_mut(64) {
        *line.last_mut().unwrap() = b'\n';
    }
    let wake = Arc::new(WriterWake::default());
    let waker = Waker::from(wake.clone());
    let mut context = Context::from_waker(&waker);
    let mut write = pin!(master.write_output(false, &data));
    assert!(write.as_mut().poll(&mut context).is_pending());
    assert_eq!(master.writer.write_room(), 0);
    assert!(!master.poll().contains(IoEvents::OUT));

    let mut received = Vec::new();
    let mut completed = false;
    for _ in 0..data.len() {
        let mut bytes = [0; 80];
        let mut input = slave.input.lock();
        assert!(input.poll_read());
        let count = input.read(&mut bytes).unwrap();
        drop(input);
        assert!(count != 0);
        received.extend_from_slice(&bytes[..count]);
        if !completed {
            if received.len() == 64 {
                assert!(wake.0.load(Ordering::Relaxed));
            }
            if let Poll::Ready(result) = write.as_mut().poll(&mut context) {
                assert_eq!(result, Ok(data.len()));
                completed = true;
            }
        }
        if received.len() == data.len() {
            break;
        }
    }
    assert!(completed);
    assert_eq!(received, data);
}

#[def_test]
fn pty_nonblocking_write_returns_prefix_and_preserves_suffix() {
    let (master, slave) = manual_pty_pair();
    let capacity = slave.writer.write_room();
    let mut data = alloc::vec![b'x'; capacity + 137];
    for (index, byte) in data.iter_mut().enumerate() {
        *byte = b'a' + (index % 26) as u8;
    }
    assert_eq!(block_on(slave.write_output(true, &data)), Ok(capacity));
    assert_eq!(
        block_on(slave.write_output(true, &data[capacity..])),
        Err(KError::WouldBlock)
    );
    assert!(!slave.poll().contains(IoEvents::OUT));
    let mut received = read_bytes(&master, capacity);
    assert_eq!(
        block_on(slave.write_output(true, &data[capacity..])),
        Ok(137)
    );
    received.extend_from_slice(&read_bytes(&master, 137));
    assert_eq!(received, data);
}

#[def_test]
fn pty_slave_read_wakes_blocked_master_writer() {
    let (master, slave) = manual_pty_pair();
    let capacity = master.writer.write_room();
    let mut data = alloc::vec![b'a'; capacity];
    data[63] = b'\n';
    assert_eq!(block_on(master.write_output(true, &data)), Ok(capacity));
    let wake = Arc::new(WriterWake::default());
    let waker = Waker::from(wake.clone());
    let mut context = Context::from_waker(&waker);
    let mut write = pin!(master.write_output(false, b"suffix\n"));
    assert!(write.as_mut().poll(&mut context).is_pending());
    assert!(!wake.0.load(Ordering::Relaxed));
    assert_eq!(read_bytes(&slave, 64), data[..64]);
    assert!(wake.0.load(Ordering::Relaxed));
    assert_eq!(write.as_mut().poll(&mut context), Poll::Ready(Ok(7)));
    let mut expected = data[64..].to_vec();
    expected.extend_from_slice(b"suffix\n");
    assert_eq!(read_bytes(&slave, expected.len()), expected);
}

#[def_test]
fn pty_onlcr_waits_for_a_complete_crlf_unit() {
    let (master, slave) = manual_pty_pair();
    let capacity = slave.writer.write_room();
    let prefix = alloc::vec![b'x'; capacity - 1];
    assert_eq!(
        block_on(slave.write_output(true, &prefix)),
        Ok(prefix.len())
    );
    assert_eq!(
        block_on(slave.write_output(true, b"\n")),
        Err(KError::WouldBlock)
    );
    assert_eq!(slave.writer.write_room(), 1);
    assert!(!slave.poll().contains(IoEvents::OUT));
    let wake = Arc::new(WriterWake::default());
    let waker = Waker::from(wake.clone());
    let mut context = Context::from_waker(&waker);
    let mut write = pin!(slave.write_output(false, b"\n"));
    assert!(write.as_mut().poll(&mut context).is_pending());
    let mut received = [0; 1];
    assert_eq!(master.input.lock().read(&mut received), Ok(1));
    assert_eq!(&received, b"x");
    assert!(wake.0.load(Ordering::Relaxed));
    assert_eq!(write.as_mut().poll(&mut context), Poll::Ready(Ok(1)));
    let mut expected = prefix[1..].to_vec();
    expected.extend_from_slice(b"\r\n");
    assert_eq!(read_bytes(&master, expected.len()), expected);
}

#[def_test]
fn pty_output_flush_discards_peer_ingress_and_wakes_writer() {
    let (master, slave) = manual_pty_pair();
    for (writer, peer) in [(&master, &slave), (&slave, &master)] {
        let capacity = writer.writer.write_room();
        let data = alloc::vec![b'x'; capacity];
        assert_eq!(block_on(writer.write_output(true, &data)), Ok(capacity));
        let wake = Arc::new(WriterWake::default());
        let waker = Waker::from(wake.clone());
        let mut context = Context::from_waker(&waker);
        let mut write = pin!(writer.write_output(false, b"next\n"));
        assert!(write.as_mut().poll(&mut context).is_pending());
        writer.flush_queues(TCOFLUSH).unwrap();
        assert_eq!(writer.writer.write_room(), capacity);
        assert!(wake.0.load(Ordering::Relaxed));
        assert!(!peer.input.lock().poll_read());
        assert_eq!(write.as_mut().poll(&mut context), Poll::Ready(Ok(5)));
        let expected = if writer.kind == TtyKind::PtyMaster {
            b"next\n".as_slice()
        } else {
            b"next\r\n".as_slice()
        };
        assert_eq!(read_bytes(peer, expected.len()), expected);
        // Remove slave echo before exercising the reverse direction.
        master.flush_queues(TCIFLUSH).unwrap();
    }
}

#[def_test]
fn pty_input_and_combined_flush_obey_queue_ownership() {
    for selector in [TCIFLUSH, TCIOFLUSH] {
        for flush_master in [false, true] {
            let (master, slave) = manual_pty_pair();
            assert_eq!(block_on(master.write_output(true, b"to slave\n")), Ok(9));
            assert_eq!(block_on(slave.write_output(true, b"to master")), Ok(9));
            let (local, peer) = if flush_master {
                (&master, &slave)
            } else {
                (&slave, &master)
            };
            local.flush_queues(selector).unwrap();
            assert!(!local.input.lock().poll_read());
            if selector == TCIOFLUSH {
                assert!(!peer.input.lock().poll_read());
            } else {
                assert!(peer.input.lock().poll_read());
            }
        }
    }
}

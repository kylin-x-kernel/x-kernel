// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::{Arc, Weak};
use core::task::Waker;

use kpoll::PollSet;
use kspin::SpinNoPreempt;
use ringbuf::{
    Cons, HeapRb, Prod,
    traits::{Consumer, Producer},
};

use super::{PtyPair, Tty, TtyKind};
use crate::{
    port::{ProcessMode, TtyConfig, TtyRead, TtyWrite},
    terminal::Terminal,
};

const PTY_BUF_SIZE: usize = 4096;

/// Pseudo-terminal driver
pub type PtyDriver = Tty<PtyReader, PtyWriter>;

type Buffer = Arc<HeapRb<u8>>;

/// PTY reader consuming data from ring buffer
pub struct PtyReader(Cons<Buffer>);

impl PtyReader {
    pub fn new(buffer: Buffer) -> Self {
        Self(Cons::new(buffer))
    }
}

impl TtyRead for PtyReader {
    fn read(&mut self, buf: &mut [u8]) -> usize {
        self.0.pop_slice(buf)
    }
}

/// PTY writer producing data into ring buffer
#[derive(Clone)]
pub struct PtyWriter(Arc<SpinNoPreempt<Prod<Buffer>>>, PtyNotification);

#[derive(Clone)]
enum PtyNotification {
    Input(Waker),
    Readable(Arc<PollSet>),
}

impl PtyWriter {
    fn new(buffer: Buffer, notification: PtyNotification) -> Self {
        Self(
            Arc::new(SpinNoPreempt::new(Prod::new(buffer))),
            notification,
        )
    }
}

impl TtyWrite for PtyWriter {
    fn write(&self, buf: &[u8]) {
        let read = self.0.lock().push_slice(buf);
        match &self.1 {
            PtyNotification::Input(waker) => waker.wake_by_ref(),
            PtyNotification::Readable(poll_rx) => {
                poll_rx.wake();
            }
        }
        if read < buf.len() {
            warn!("Discarding {} bytes written to pty", buf.len() - read);
        }
    }
}

/// Creates connected master/slave endpoints with task-context slave input work.
///
/// Requires sleepable context after normal workqueue initialization. Device-node
/// publication and open-file lifetime are owned by the caller.
///
/// # Panics
/// Panics if work preparation is unavailable or allocation fails.
#[cfg(unittest)]
pub(super) fn create_pty_pair() -> (Arc<PtyDriver>, Arc<PtyDriver>) {
    create_endpoints(None)
}

pub(super) fn create_endpoints(pair: Option<Weak<PtyPair>>) -> (Arc<PtyDriver>, Arc<PtyDriver>) {
    let master_to_slave = Arc::new(HeapRb::new(PTY_BUF_SIZE));
    let slave_to_master = Arc::new(HeapRb::new(PTY_BUF_SIZE));
    let poll_rx_master = Arc::new(PollSet::new());

    let terminal = Arc::new(Terminal::default());

    let slave = Tty::new_with_pair(
        terminal.clone(),
        TtyKind::Terminal,
        TtyConfig {
            reader: PtyReader::new(master_to_slave.clone()),
            writer: PtyWriter::new(
                slave_to_master.clone(),
                PtyNotification::Readable(poll_rx_master.clone()),
            ),
            process_mode: ProcessMode::Work(khal::percpu::this_cpu_id()),
        },
        pair.clone(),
    );

    let input_waker = slave
        .input
        .lock()
        .input_waker()
        .expect("PTY slave uses input work");
    let master = Tty::new_with_pair(
        terminal,
        TtyKind::PtyMaster,
        TtyConfig {
            reader: PtyReader::new(slave_to_master),
            writer: PtyWriter::new(master_to_slave, PtyNotification::Input(input_waker.clone())),
            process_mode: ProcessMode::None(poll_rx_master),
        },
        pair,
    );

    // Close the notification-attachment window even if input already arrived.
    input_waker.wake_by_ref();
    (master, slave)
}

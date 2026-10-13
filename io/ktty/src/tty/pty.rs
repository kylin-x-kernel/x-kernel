// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::{Arc, Weak};

use kerrno::KResult;
use kpoll::{PollContext, PollRegisterError};

use super::{PtyPair, Tty, TtyKind};
use crate::{
    port::{InputIngress, ProcessMode, TtyConfig, TtyRead, TtyWrite},
    terminal::Terminal,
};

/// Pseudo-terminal driver
pub type PtyDriver = Tty<PtyReader, PtyWriter>;

/// PTY reader sharing the receiving port's bounded ingress.
pub struct PtyReader(Arc<InputIngress>);

impl PtyReader {
    pub fn new(ingress: Arc<InputIngress>) -> Self {
        Self(ingress)
    }
}

impl TtyRead for PtyReader {
    fn read(&mut self, buf: &mut [u8]) -> usize {
        self.0.read(buf)
    }

    fn input_ingress(&self) -> Option<Arc<InputIngress>> {
        Some(self.0.clone())
    }
}

/// PTY writer pushing only the accepted prefix into the peer's ingress.
#[derive(Clone)]
pub struct PtyWriter(Arc<InputIngress>);

impl PtyWriter {
    pub(super) fn new(ingress: Arc<InputIngress>) -> Self {
        Self(ingress)
    }
}

impl TtyWrite for PtyWriter {
    fn write(&self, buf: &[u8]) -> KResult<usize> {
        self.0.push_input(buf)
    }

    fn write_room(&self) -> usize {
        self.0.available_input_room()
    }

    fn register_tx(&self, context: &mut PollContext<'_>) -> Result<(), PollRegisterError> {
        self.0.register_space(context)
    }

    fn notify_tx(&self) {
        self.0.notify_space();
    }

    fn flush_output(&self) {
        self.0.clear();
    }

    fn is_closed(&self) -> bool {
        self.0.is_closed()
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
    let master_to_slave = Arc::new(InputIngress::default());
    let slave_to_master = Arc::new(InputIngress::default());
    let poll_rx_master = slave_to_master.readable();

    let terminal = Arc::new(Terminal::default());

    let slave = Tty::new_with_pair(
        terminal.clone(),
        TtyKind::Terminal,
        TtyConfig {
            reader: PtyReader::new(master_to_slave.clone()),
            writer: PtyWriter::new(slave_to_master.clone()),
            process_mode: ProcessMode::Work(khal::percpu::this_cpu_id()),
        },
        pair.clone(),
    );

    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: the slave was constructed above with \
                  ProcessMode::Work, which always has an input waker"
    )]
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
            writer: PtyWriter::new(master_to_slave),
            process_mode: ProcessMode::None(poll_rx_master),
        },
        pair,
    );

    // Close the notification-attachment window even if input already arrived.
    input_waker.wake_by_ref();
    (master, slave)
}

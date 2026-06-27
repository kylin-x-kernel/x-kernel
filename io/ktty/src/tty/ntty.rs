// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::{AtomicU8, Ordering};

use console_driver::runtime::{active_console_id, read_active_console, write_active_console};
use klazy::lazy_static;
use ktask::future::register_irq_waker;

use super::Tty;
use crate::terminal::ldisc::{ProcessMode, TtyConfig, TtyRead, TtyWrite};

/// Native TTY driver using console I/O
pub type NTtyDriver = Tty<Console, Console>;

/// Console reader/writer for native TTY
#[derive(Clone, Copy)]
pub struct Console;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConsoleBackendState {
    BootDirect      = 0,
    HandoverPending = 1,
    RuntimeOwned    = 2,
}

static CONSOLE_BACKEND_STATE: AtomicU8 = AtomicU8::new(ConsoleBackendState::BootDirect as u8);

fn set_backend_state(state: ConsoleBackendState) {
    CONSOLE_BACKEND_STATE.store(state as u8, Ordering::Release);
}

fn backend_state() -> ConsoleBackendState {
    match CONSOLE_BACKEND_STATE.load(Ordering::Acquire) {
        0 => ConsoleBackendState::BootDirect,
        1 => ConsoleBackendState::HandoverPending,
        2 => ConsoleBackendState::RuntimeOwned,
        _ => ConsoleBackendState::BootDirect,
    }
}

/// Try to attach one runtime console device discovered by the driver core.
///
/// If no suitable console handle is found, the native TTY continues to use
/// the direct `khal::console` path.
pub fn try_handoff_console() {
    if backend_state() == ConsoleBackendState::RuntimeOwned {
        return;
    }

    set_backend_state(ConsoleBackendState::HandoverPending);

    if let Some(id) = active_console_id() {
        set_backend_state(ConsoleBackendState::RuntimeOwned);
        info!(
            "tty: runtime console handoff attached via console subsystem ({:?})",
            id
        );
        return;
    }

    set_backend_state(ConsoleBackendState::BootDirect);
    debug!("tty: no runtime console handoff candidate, keep boot-direct backend");
}

fn read_from_runtime_console(buf: &mut [u8]) -> Option<usize> {
    if backend_state() != ConsoleBackendState::RuntimeOwned {
        return None;
    }

    match read_active_console(buf)? {
        Ok(n) => Some(n),
        Err(err) => {
            warn!(
                "tty: runtime console read failed ({:?}), fallback to boot-direct backend",
                err
            );
            set_backend_state(ConsoleBackendState::BootDirect);
            None
        }
    }
}

fn write_to_runtime_console(buf: &[u8]) -> bool {
    if backend_state() != ConsoleBackendState::RuntimeOwned {
        return false;
    }

    let Some(result) = write_active_console(buf) else {
        set_backend_state(ConsoleBackendState::BootDirect);
        return false;
    };

    match result {
        Ok(_) => true,
        Err(err) => {
            warn!(
                "tty: runtime console write failed ({:?}), fallback to boot-direct backend",
                err
            );
            set_backend_state(ConsoleBackendState::BootDirect);
            false
        }
    }
}

impl TtyRead for Console {
    fn read(&mut self, buf: &mut [u8]) -> usize {
        if let Some(n) = read_from_runtime_console(buf) {
            return n;
        }
        khal::console::read_data(buf)
    }
}
impl TtyWrite for Console {
    fn write(&self, buf: &[u8]) {
        if write_to_runtime_console(buf) {
            return;
        }
        khal::console::write_data(buf);
    }
}

lazy_static! {
    /// The default TTY device.
    pub static ref N_TTY: Arc<NTtyDriver> = new_n_tty();
}

fn new_n_tty() -> Arc<NTtyDriver> {
    Tty::new(
        Arc::default(),
        TtyConfig {
            reader: Console,
            writer: Console,
            process_mode: if let Some(irq) = khal::console::interrupt_id() {
                ProcessMode::External(Box::new(move |waker| register_irq_waker(irq, &waker)) as _)
            } else {
                ProcessMode::Manual
            },
        },
    )
}

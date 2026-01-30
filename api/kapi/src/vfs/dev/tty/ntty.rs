// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc};

use ktask::future::register_irq_waker;
use lazy_static::lazy_static;

use super::Tty;
use crate::terminal::ldisc::{ProcessMode, TtyConfig, TtyRead, TtyWrite};

/// Native TTY driver using console I/O
pub type NTtyDriver = Tty<Console, Console>;

/// Console reader/writer for native TTY
#[derive(Clone, Copy)]
pub struct Console;
impl TtyRead for Console {
    fn read(&mut self, buf: &mut [u8]) -> usize {
        khal::console::read_data(buf)
    }
}
impl TtyWrite for Console {
    fn write(&self, buf: &[u8]) {
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

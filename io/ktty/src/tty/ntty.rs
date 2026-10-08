// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::task::Waker;

use char_driver::CharDevice;
use console_driver::{
    runtime::stdout_console,
    serial::{self, SerialPort},
};
use kclass::{CharDeviceImpl, ClassDevice};
use klazy::lazy_static;
use kspin::SpinNoPreempt;

use super::{Tty, TtyKind};
use crate::port::{ProcessMode, TtyConfig, TtyRead, TtyWrite};

/// Native TTY whose I/O and input notification share one stdout connection.
pub type NTtyDriver = Tty<Arc<ConsoleConnection>, Arc<ConsoleConnection>>;

/// The native terminal's boot port and optional runtime access to that same port.
///
/// Runtime access is adopted in task context. Global console selection does not
/// redirect this established connection. The early port is kernel-lifetime;
/// retaining its runtime wrapper does not provide general hot-unplug support.
pub struct ConsoleConnection {
    boot_port: Option<Arc<SerialPort>>,
    runtime: SpinNoPreempt<Option<ClassDevice<CharDeviceImpl>>>,
}

impl ConsoleConnection {
    fn new() -> Self {
        Self {
            boot_port: serial::stdout_port(),
            runtime: SpinNoPreempt::new(None),
        }
    }

    fn attach_input(&self, waker: Waker) {
        self.boot_port
            .as_ref()
            .expect("console input IRQ requires a stdout port")
            .attach_input_waker(waker)
            .expect("failed to connect console input notification");
    }

    fn try_handoff(&self) {
        if self.boot_port.is_none() || self.runtime.lock().is_some() {
            return;
        }
        // Serial publication guarantees this wrapper adopts the early stdout
        // instance. The active console is not evidence of that identity.
        let Some(handle) = stdout_console() else {
            return;
        };
        let id = handle.id();
        let mut runtime = self.runtime.lock();
        if runtime.is_none() {
            *runtime = Some(handle);
            drop(runtime);
            info!("tty: runtime stdout attached ({id:?})");
        }
    }

    fn runtime_handle(&self) -> Option<ClassDevice<CharDeviceImpl>> {
        let handle = self.runtime.lock().clone()?;
        if handle.is_available() {
            Some(handle)
        } else {
            self.fallback(&handle);
            None
        }
    }

    fn fallback(&self, failed: &ClassDevice<CharDeviceImpl>) {
        let removed = {
            let mut runtime = self.runtime.lock();
            if runtime
                .as_ref()
                .is_some_and(|current| current.id() == failed.id())
            {
                runtime.take()
            } else {
                None
            }
        };
        drop(removed);
    }

    fn read(&self, buf: &mut [u8]) -> usize {
        if let Some(handle) = self.runtime_handle() {
            match handle.read(buf) {
                Ok(count) => return count,
                Err(error) => {
                    self.fallback(&handle);
                    warn!("tty: runtime stdout read failed ({error:?}), using boot port");
                }
            }
        }
        if let Some(port) = &self.boot_port {
            port.read_data(buf)
        } else {
            khal::console::read_data(buf)
        }
    }
}

impl TtyRead for Arc<ConsoleConnection> {
    fn read(&mut self, buf: &mut [u8]) -> usize {
        let count = ConsoleConnection::read(self, buf);
        if let Some(port) = &self.boot_port {
            let dropped = port.take_input_dropped_bytes();
            if dropped != 0 {
                warn!("tty: console receive buffer dropped {dropped} input bytes");
            }
        }
        count
    }
}

impl TtyWrite for Arc<ConsoleConnection> {
    fn write(&self, buf: &[u8]) {
        if let Some(handle) = self.runtime_handle() {
            match handle.write(buf) {
                Ok(_) => return,
                Err(error) => {
                    self.fallback(&handle);
                    warn!("tty: runtime stdout write failed ({error:?}), using boot port");
                }
            }
        }
        if let Some(port) = &self.boot_port {
            port.write_data(buf);
        } else {
            khal::console::write_data(buf);
        }
    }
}

/// Connects native input and adopts runtime access to the same boot stdout port.
///
/// Called in late-init task context after normal workers and device discovery.
/// No runtime candidate leaves direct boot access intact; repeated calls are
/// harmless. Input already collected before attachment receives an initial wake.
pub fn try_handoff_console() {
    N_TTY.writer.try_handoff();
}

lazy_static! {
    /// The default TTY device.
    pub static ref N_TTY: Arc<NTtyDriver> = new_n_tty();
}

fn new_n_tty() -> Arc<NTtyDriver> {
    let has_input_irq = khal::console::interrupt_id().is_some();
    let connection = Arc::new(ConsoleConnection::new());
    let tty = Tty::new(
        Arc::default(),
        TtyKind::Terminal,
        TtyConfig {
            reader: connection.clone(),
            writer: connection.clone(),
            process_mode: if has_input_irq {
                ProcessMode::Work(khal::percpu::this_cpu_id())
            } else {
                ProcessMode::Manual
            },
        },
    );
    if has_input_irq {
        let waker = tty.input.lock().input_waker().unwrap();
        connection.attach_input(waker);
    }
    tty
}

#[cfg(unittest)]
#[path = "../tests/connection.rs"]
mod tests;

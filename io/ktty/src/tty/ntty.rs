// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
};
use core::task::Waker;

use char_driver::CharDevice;
use console_driver::{
    runtime::stdout_console,
    serial::{self, SerialPort},
};
use kclass::{CharDeviceImpl, ClassDevice};
use kerrno::KResult;
use klazy::lazy_static;
use kspin::SpinNoPreempt;
use ktime_types::TimeSpan;
use kworkng::DelayedWorkHandle;

use super::{Tty, TtyKind};
use crate::port::{ProcessMode, TtyConfig, TtyRead, TtyWrite};

/// Native TTY whose I/O and input notification share one stdout connection.
pub type NTtyDriver = Tty<Arc<ConsoleConnection>, Arc<ConsoleConnection>>;

/// The native terminal's boot port and optional runtime access to that same port.
///
/// Runtime access is adopted in task context. Global console selection does not
/// redirect this established connection. The early port is kernel-lifetime;
/// retaining its runtime wrapper does not provide general hot-unplug support.
/// Without an input IRQ, the connection also owns periodic input Work submission.
pub struct ConsoleConnection {
    boot_port: Option<Arc<SerialPort>>,
    runtime: SpinNoPreempt<Option<ClassDevice<CharDeviceImpl>>>,
    input_poller: SpinNoPreempt<Option<Arc<ConsoleInputPoller>>>,
}

// Only consoles without an input IRQ need periodic backend progress. The
// input Work publishes readability; this timer never wakes file waiters.
struct ConsoleInputPoller {
    work: DelayedWorkHandle<Box<dyn Fn() + Send + Sync>>,
}

impl ConsoleInputPoller {
    fn new(input_waker: Waker) -> Arc<Self> {
        Arc::new_cyclic(|weak: &Weak<Self>| {
            let weak = weak.clone();
            let callback: Box<dyn Fn() + Send + Sync> = Box::new(move || {
                if let Some(poller) = weak.upgrade() {
                    input_waker.wake_by_ref();
                    poller.queue();
                }
            });
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: native input Work was already \
                          prepared in sleepable context after normal-worker startup"
            )]
            let work = DelayedWorkHandle::new(callback)
                .expect("console input polling requires normal workers");
            Self { work }
        })
    }

    fn queue(&self) {
        if let Err(error) = self.work.queue_default(TimeSpan::from_millis(10), None) {
            warn!("console input polling could not be queued: {error:?}");
        }
    }
}

impl ConsoleConnection {
    fn new() -> Self {
        Self {
            boot_port: serial::stdout_port(),
            runtime: SpinNoPreempt::new(None),
            input_poller: SpinNoPreempt::new(None),
        }
    }

    fn attach_input(&self, waker: Waker) {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: The singleton N_TTY binds once after \
                      console IRQ initialization has installed the stdout port and prepared its \
                      input buffer"
        )]
        self.boot_port
            .as_ref()
            .expect("console input IRQ requires a stdout port")
            .attach_input_waker(waker)
            .expect("failed to connect console input notification");
    }

    fn poll_input(&self, waker: Waker) {
        let poller = ConsoleInputPoller::new(waker.clone());
        poller.queue();
        *self.input_poller.lock() = Some(poller);
        waker.wake_by_ref();
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
    fn handles_output_processing(&self) -> bool {
        true
    }

    // Native output preserves its existing best-effort contract; reported bytes
    // do not establish a lossless transport or hardware drain guarantee.
    fn write(&self, buf: &[u8]) -> KResult<usize> {
        if let Some(handle) = self.runtime_handle() {
            match handle.write(buf) {
                Ok(_) => return Ok(buf.len()),
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
        Ok(buf.len())
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
            process_mode: ProcessMode::Work(khal::percpu::this_cpu_id()),
        },
    );
    #[expect(
        clippy::unwrap_used,
        reason = "CG-021-no-panic-on-reachable-failure: native TTY uses ProcessMode::Work, which \
                  always installs an input waker"
    )]
    let waker = tty.input.lock().input_waker().unwrap();
    if has_input_irq {
        connection.attach_input(waker);
    } else {
        connection.poll_input(waker);
    }
    tty
}

unittest::path_mod_test!(tests, "../../ut-tests/tty_ntty.rs");

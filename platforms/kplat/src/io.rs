// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform console I/O interface and logging helpers.

use core::fmt::{Arguments, Result, Write};

use kplat_macros::device_interface;

#[device_interface]
pub trait ConsoleIf {
    /// Writes bytes to the platform console.
    fn write_data(buf: &[u8]);

    /// Writes bytes to the console without locking.
    ///
    /// # Note
    /// This default implementation exists **only to satisfy compilation** in the
    /// device-interface macro framework. The function body here will **NOT** be
    /// used at runtime.
    ///
    /// If this method is ever invoked, the platform must provide its own concrete
    /// implementation in the corresponding `impl ConsoleIf`, otherwise a linker
    /// error will occur.
    ///
    /// In other words, this default implementation does **not** provide a real
    /// fallback behavior.
    fn write_data_atomic(buf: &[u8]) {
        Self::write_data(buf)
    }

    /// Reads bytes from the platform console.
    fn read_data(buf: &mut [u8]) -> usize;

    /// Returns the interrupt ID for console input, if any.
    fn interrupt_id() -> Option<usize>;
}

struct Logger;

impl Write for Logger {
    fn write_str(&mut self, s: &str) -> Result {
        write_data(s.as_bytes());
        Ok(())
    }
}

struct AtomicLogger;

impl Write for AtomicLogger {
    fn write_str(&mut self, s: &str) -> Result {
        write_data_atomic(s.as_bytes());
        Ok(())
    }
}

/// Global lock guarding console output.
pub static IO_LOCK: kspin::SpinNoIrq<()> = kspin::SpinNoIrq::new(());

#[doc(hidden)]
pub fn _sys_log(fmt: Arguments) {
    let _l = IO_LOCK.lock();
    Logger.write_fmt(fmt).unwrap();
    drop(_l);
}

#[doc(hidden)]
pub fn _sys_log_atomic(fmt: Arguments) {
    AtomicLogger.write_fmt(fmt).ok();
}

#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => {
        $crate::io::_sys_log(format_args!($($arg)*));
    }
}

#[macro_export]
macro_rules! kprintln {
    () => { $crate::kprint!("\n") };
    ($($arg:tt)*) => {
        $crate::io::_sys_log(format_args!("{}\n", format_args!($($arg)*)));
    }
}

#[macro_export]
macro_rules! kprint_atomic {
    ($($arg:tt)*) => {
        $crate::io::_sys_log_atomic(core::format_args!($($arg)*));
    }
}

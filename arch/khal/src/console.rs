// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::fmt::{Arguments, Result, Write};

#[inline]
pub fn write_data(buf: &[u8]) {
    kiface_console_api::ConsoleIoIf::write_data(buf)
}

/// Attempt best-effort emergency output, including from NMI context.
///
/// Bypasses normal console locks; does not guarantee complete or ordered output.
#[inline]
pub fn write_data_atomic(buf: &[u8]) {
    kiface_console_api::EmergencyConsoleWriteIf::write_data_atomic(buf)
}

#[inline]
pub fn read_data(buf: &mut [u8]) -> usize {
    kiface_console_api::ConsoleIoIf::read_data(buf)
}

/// Prepare optional interrupt input before publishing console consumers.
///
/// Called by the boot task after worker/IRQ services start. Completion fixes
/// the input mode for this boot; unavailable IRQ input falls back to polling.
/// The caller must serialize setup with consumer publication; this is not a
/// concurrent initialization barrier or a runtime retry interface.
pub fn prepare_input() {
    kiface_console_api::ConsoleInputSetupIf::prepare_input()
}

#[inline]
pub fn interrupt_id() -> Option<usize> {
    kiface_console_api::ConsoleInputSetupIf::interrupt_id()
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

kspin::static_lock! {
    pub static IO_LOCK: kspin::SpinNoIrq<()> = kspin::SpinNoIrq::new(());
}

#[doc(hidden)]
pub fn _sys_log(fmt: Arguments) {
    let lock = IO_LOCK.lock();
    #[expect(
        clippy::unwrap_used,
        reason = "CG-021-no-panic-on-reachable-failure: Logger::write_str always succeeds; \
                  formatters must not report errors unless the writer fails"
    )]
    Logger.write_fmt(fmt).unwrap();
    drop(lock);
}

#[doc(hidden)]
pub fn _sys_log_atomic(fmt: Arguments) {
    // Emergency output is best effort; reporting a formatting error here could recurse.
    let _ = AtomicLogger.write_fmt(fmt);
}

#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => {
        $crate::console::_sys_log(format_args!($($arg)*));
    }
}

#[macro_export]
macro_rules! kprintln {
    () => { $crate::kprint!("\n") };
    ($($arg:tt)*) => {
        $crate::console::_sys_log(format_args!("{}\n", format_args!($($arg)*)));
    }
}

/// Format best-effort emergency output without taking normal console locks.
///
/// In NMI context, argument evaluation and `Display`/`Debug` implementations
/// must also avoid locks, allocation, and blocking. Output is not guaranteed
/// to be complete or serialized with other writers.
///
/// # Syntax
///
/// `kprint_atomic!("format string", args...)` accepts a format string literal
/// followed by optional positional or named arguments, as in
/// [`core::format_args!`]. A newline is emitted only if included in the format
/// string or a formatted argument.
///
/// # Example
///
/// ```no_run
/// let cpu_id = 0usize;
/// khal::kprint_atomic!("NMI on CPU {}\n", cpu_id);
/// ```
#[macro_export]
macro_rules! kprint_atomic {
    ($($arg:tt)*) => {
        $crate::console::_sys_log_atomic(core::format_args!($($arg)*));
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::fmt::{Arguments, Result, Write};

/// Provides platform console input, output, and input interrupt discovery.
///
/// Normal I/O may use console locks; emergency output must remain usable when
/// an NMI interrupts a normal I/O operation.
#[kiface::interface]
pub trait ConsoleIf {
    fn write_data(buf: &[u8]);

    /// Attempt emergency output without ordinary locks or allocation.
    ///
    /// Implementations must tolerate NMI interruption of normal console I/O
    /// and bound hardware polling. Output may be interleaved, truncated, or
    /// discarded if the console is unavailable.
    fn write_data_atomic(buf: &[u8]);

    fn read_data(buf: &mut [u8]) -> usize;

    fn interrupt_id() -> Option<usize>;
}

#[inline]
pub fn write_data(buf: &[u8]) {
    ConsoleIf::write_data(buf)
}

/// Attempt best-effort emergency output, including from NMI context.
///
/// Bypasses normal console locks; does not guarantee complete or ordered output.
#[inline]
pub fn write_data_atomic(buf: &[u8]) {
    ConsoleIf::write_data_atomic(buf)
}

#[inline]
pub fn read_data(buf: &mut [u8]) -> usize {
    ConsoleIf::read_data(buf)
}

#[inline]
pub fn interrupt_id() -> Option<usize> {
    ConsoleIf::interrupt_id()
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

pub static IO_LOCK: kspin::SpinNoIrq<()> = kspin::SpinNoIrq::new(());

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

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::fmt::{Arguments, Result, Write};

use crate_interface::{call_interface, def_interface};

#[def_interface]
pub trait ConsoleIf {
    fn write_data(buf: &[u8]);

    fn write_data_atomic(buf: &[u8]) {
        Self::write_data(buf)
    }

    fn read_data(buf: &mut [u8]) -> usize;

    fn interrupt_id() -> Option<usize>;
}

#[inline]
pub fn write_data(buf: &[u8]) {
    call_interface!(ConsoleIf::write_data, buf)
}

#[inline]
pub fn write_data_atomic(buf: &[u8]) {
    call_interface!(ConsoleIf::write_data_atomic, buf)
}

#[inline]
pub fn read_data(buf: &mut [u8]) -> usize {
    call_interface!(ConsoleIf::read_data, buf)
}

#[inline]
pub fn interrupt_id() -> Option<usize> {
    call_interface!(ConsoleIf::interrupt_id)
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

#[macro_export]
macro_rules! kprint_atomic {
    ($($arg:tt)*) => {
        $crate::console::_sys_log_atomic(core::format_args!($($arg)*));
    }
}

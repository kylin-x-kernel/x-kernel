use core::fmt::{Arguments, Result, Write};

use kplat_macros::device_interface;

#[device_interface]
pub trait Terminal {
    fn write_data(buf: &[u8]);

    fn write_data_atomic(buf: &[u8]) {
        Self::write_data(buf)
    }

    fn read_data(buf: &mut [u8]) -> usize;

    #[cfg(feature = "irq")]
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

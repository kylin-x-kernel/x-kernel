#![cfg_attr(not(feature = "std"), no_std)]

extern crate log;

use core::{
    fmt::{self, Write},
    str::FromStr,
};

#[cfg(not(feature = "std"))]
use crate_interface::call_interface;
use log::{Level, LevelFilter, Log, Metadata, Record};
pub use log::{debug, error, info, trace, warn};

#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => {
        $crate::print_fmt(format_args!($($arg)*)).unwrap();
    }
}

#[macro_export]
macro_rules! kprintln {
    () => { $crate::kprint!("\n") };
    ($($arg:tt)*) => {
        $crate::print_fmt(format_args!("{}\n", format_args!($($arg)*))).unwrap();
    }
}

macro_rules! color_fmt {
    ($color_code:expr, $($arg:tt)*) => {
        format_args!("\u{1B}[{}m{}\u{1B}[m", $color_code as u8, format_args!($($arg)*))
    };
}

#[repr(u8)]
#[allow(dead_code)]
enum AnsiColor {
    Black         = 30,
    Red           = 31,
    Green         = 32,
    Yellow        = 33,
    Blue          = 34,
    Magenta       = 35,
    Cyan          = 36,
    White         = 37,
    BrightBlack   = 90,
    BrightRed     = 91,
    BrightGreen   = 92,
    BrightYellow  = 93,
    BrightBlue    = 94,
    BrightMagenta = 95,
    BrightCyan    = 96,
    BrightWhite   = 97,
}

#[crate_interface::def_interface]
pub trait LoggerAdapter {
    fn write_str(s: &str);
    fn now() -> core::time::Duration;
    fn cpu_id() -> Option<usize>;
    fn task_id() -> Option<u64>;
}

struct KernelLogger;

impl Write for KernelLogger {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        cfg_if::cfg_if! {
            if #[cfg(feature = "std")] {
                std::print!("{s}");
            } else {
                call_interface!(LoggerAdapter::write_str, s);
            }
        }
        Ok(())
    }
}

impl Log for KernelLogger {
    #[inline]
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let level = record.level();
        let line = record.line().unwrap_or(0);
        let path = record.target();
        let color = match level {
            Level::Error => AnsiColor::Red,
            Level::Warn => AnsiColor::Yellow,
            Level::Info => AnsiColor::Green,
            Level::Debug => AnsiColor::Cyan,
            Level::Trace => AnsiColor::BrightBlack,
        };

        cfg_if::cfg_if! {
            if #[cfg(feature = "std")] {
                let _ = print_fmt(color_fmt!(
                    AnsiColor::White,
                    "[{time} {path}:{line}] {args}\n",
                    time = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.6f"),
                    path = path,
                    line = line,
                    args = color_fmt!(color, "{}", record.args()),
                ));
            } else {
                let cpu_id = call_interface!(LoggerAdapter::cpu_id);
                let tid = call_interface!(LoggerAdapter::task_id);
                let now = call_interface!(LoggerAdapter::now);

                let _ = match (cpu_id, tid) {
                   (Some(c), Some(t)) => print_fmt(color_fmt!(
                        AnsiColor::White,
                        "[{:>3}.{:06} {c}:{t} {path}:{line}] {args}\n",
                        now.as_secs(),
                        now.subsec_micros(),
                        c = c,
                        t = t,
                        path = path,
                        line = line,
                        args = color_fmt!(color, "{}", record.args()),
                    )),
                    (Some(c), None) => print_fmt(color_fmt!(
                        AnsiColor::White,
                        "[{:>3}.{:06} {c} {path}:{line}] {args}\n",
                        now.as_secs(),
                        now.subsec_micros(),
                        c = c,
                        path = path,
                        line = line,
                        args = color_fmt!(color, "{}", record.args()),
                    )),
                    _ => print_fmt(color_fmt!(
                        AnsiColor::White,
                        "[{:>3}.{:06} {path}:{line}] {args}\n",
                        now.as_secs(),
                        now.subsec_micros(),
                        path = path,
                        line = line,
                        args = color_fmt!(color, "{}", record.args()),
                    )),
                };
            }
        }
    }

    fn flush(&self) {}
}

pub fn print_fmt(args: fmt::Arguments) -> fmt::Result {
    use kspin::SpinNoIrq;
    static LOCK: SpinNoIrq<()> = SpinNoIrq::new(());

    let _guard = LOCK.lock();
    KernelLogger.write_fmt(args)
}

pub fn init_klogger() {
    log::set_logger(&KernelLogger).unwrap();
    log::set_max_level(LevelFilter::Warn);
}

pub fn set_log_level(level: &str) {
    let lf = LevelFilter::from_str(level)
        .ok()
        .unwrap_or(LevelFilter::Off);
    log::set_max_level(lf);
}

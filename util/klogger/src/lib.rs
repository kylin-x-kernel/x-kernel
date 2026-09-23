// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel logging utilities and macros.
#![cfg_attr(not(feature = "std"), no_std)]

extern crate log;

use core::{
    fmt::{self, Write},
    str::FromStr,
};

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
    Black   = 30,
    Red     = 31,
    Green   = 32,
    Yellow  = 33,
    Blue    = 34,
    Magenta = 35,
    Cyan    = 36,
    White   = 37,
}

#[repr(u8)]
#[allow(dead_code)]
#[allow(clippy::enum_variant_names)]
enum AnsiBrightColor {
    BrightBlack   = 90,
    BrightRed     = 91,
    BrightGreen   = 92,
    BrightYellow  = 93,
    BrightBlue    = 94,
    BrightMagenta = 95,
    BrightCyan    = 96,
    BrightWhite   = 97,
}

#[kiface::interface]
pub trait LoggerAdapter {
    fn write_str(s: &str);
    fn now() -> ktime_types::TimeSpan;
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
                LoggerAdapter::write_str(s);
            }
        }
        Ok(())
    }
}

#[inline]
fn get_level_color(level: Level) -> u8 {
    match level {
        Level::Error => AnsiColor::Red as u8,
        Level::Warn => AnsiColor::Yellow as u8,
        Level::Info => AnsiColor::Green as u8,
        Level::Debug => AnsiColor::Cyan as u8,
        Level::Trace => AnsiBrightColor::BrightBlack as u8,
    }
}

impl Log for KernelLogger {
    #[inline]
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn flush(&self) {}

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let line = record.line().unwrap_or(0);
        let path = record.target();
        let compact = path == "unittest";
        let color = get_level_color(record.level());

        cfg_if::cfg_if! {
            if #[cfg(feature = "std")] {
                let _ = if compact {
                    print_fmt(color_fmt!(
                        AnsiColor::White,
                        "[{time}] {args}\n",
                        time = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.6f"),
                        args = color_fmt!(color, "{}", record.args()),
                    ))
                } else {
                    print_fmt(color_fmt!(
                        AnsiColor::White,
                        "[{time} {path}:{line}] {args}\n",
                        time = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.6f"),
                        path = path,
                        line = line,
                        args = color_fmt!(color, "{}", record.args()),
                    ))
                };
            } else {
                let cpu_id = LoggerAdapter::cpu_id();
                let tid = LoggerAdapter::task_id();
                let now = LoggerAdapter::now();

                let _ = match (compact, cpu_id, tid) {
                    (true, Some(c), Some(t)) => print_fmt(color_fmt!(
                        AnsiColor::White,
                        "[{:>3}.{:06} {c}:{t}] {args}\n",
                        now.as_secs(),
                        now.subsec_micros(),
                        c = c,
                        t = t,
                        args = color_fmt!(color, "{}", record.args()),
                    )),
                    (true, Some(c), None) => print_fmt(color_fmt!(
                        AnsiColor::White,
                        "[{:>3}.{:06} {c}] {args}\n",
                        now.as_secs(),
                        now.subsec_micros(),
                        c = c,
                        args = color_fmt!(color, "{}", record.args()),
                    )),
                    (true, _, _) => print_fmt(color_fmt!(
                        AnsiColor::White,
                        "[{:>3}.{:06}] {args}\n",
                        now.as_secs(),
                        now.subsec_micros(),
                        args = color_fmt!(color, "{}", record.args()),
                    )),
                    (false, Some(c), Some(t)) => print_fmt(color_fmt!(
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
                    (false, Some(c), None) => print_fmt(color_fmt!(
                        AnsiColor::White,
                        "[{:>3}.{:06} {c} {path}:{line}] {args}\n",
                        now.as_secs(),
                        now.subsec_micros(),
                        c = c,
                        path = path,
                        line = line,
                        args = color_fmt!(color, "{}", record.args()),
                    )),
                    (false, _, _) => print_fmt(color_fmt!(
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

/// Returns the currently active maximum log-level filter.
///
/// This is the runtime-only ceiling: even when a level is enabled here, the
/// crate's compile-time maximum (if any) still applies on top.
pub fn current_log_level() -> LevelFilter {
    log::max_level()
}

/// Sets the active maximum log-level filter at runtime.
///
/// Takes effect immediately for all subsequent `error!`/`warn!`/`info!`/
/// `debug!`/`trace!` calls; no rebuild or reboot is required.
pub fn set_log_level_filter(level: LevelFilter) {
    log::set_max_level(level);
}

/// Parses a log level from user input, accepting either a level name
/// (`off`/`error`/`warn`/`info`/`debug`/`trace`, ASCII case-insensitive) or
/// a number in X-Kernel's own level order (`0..=5`, higher = more verbose):
///
/// | number | resolved level                |
/// |--------|-------------------------------|
/// | `0`    | [`LevelFilter::Off`]          |
/// | `1`    | [`LevelFilter::Error`]        |
/// | `2`    | [`LevelFilter::Warn`]         |
/// | `3`    | [`LevelFilter::Info`]         |
/// | `4`    | [`LevelFilter::Debug`]        |
/// | `5`    | [`LevelFilter::Trace`]        |
///
/// Values outside `0..=5` are rejected. Whitespace around the input is
/// ignored. Returns `None` for anything else.
pub fn parse_log_level(input: &str) -> Option<LevelFilter> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    if let Ok(number) = input.parse::<u8>() {
        return match number {
            0 => Some(LevelFilter::Off),
            1 => Some(LevelFilter::Error),
            2 => Some(LevelFilter::Warn),
            3 => Some(LevelFilter::Info),
            4 => Some(LevelFilter::Debug),
            5 => Some(LevelFilter::Trace),
            _ => None,
        };
    }
    LevelFilter::from_str(input).ok()
}

/// Maps a level filter back to its canonical number in X-Kernel's own level
/// order (the inverse of [`parse_log_level`]):
/// `off`→`0`, `error`→`1`, `warn`→`2`, `info`→`3`, `debug`→`4`, `trace`→`5`.
pub fn level_number(level: LevelFilter) -> u8 {
    match level {
        LevelFilter::Off => 0,
        LevelFilter::Error => 1,
        LevelFilter::Warn => 2,
        LevelFilter::Info => 3,
        LevelFilter::Debug => 4,
        LevelFilter::Trace => 5,
    }
}

/// Returns the lowercase display name of a level filter
/// (`off`/`error`/`warn`/`info`/`debug`/`trace`).
pub fn level_name(level: LevelFilter) -> &'static str {
    match level {
        LevelFilter::Off => "off",
        LevelFilter::Error => "error",
        LevelFilter::Warn => "warn",
        LevelFilter::Info => "info",
        LevelFilter::Debug => "debug",
        LevelFilter::Trace => "trace",
    }
}

/// Sets the log level by name or level number (`0..=5`); unknown input leaves
/// the current level unchanged.
pub fn set_log_level(level: &str) {
    if let Some(filter) = parse_log_level(level) {
        set_log_level_filter(filter);
    }
}

#[cfg(unittest)]
mod tests {
    extern crate alloc;

    use alloc::string::ToString;

    use log::LevelFilter;
    use unittest::{assert_eq, def_test};

    use super::{
        current_log_level, level_name, level_number, parse_log_level, set_log_level,
        set_log_level_filter,
    };

    #[def_test]
    fn parse_accepts_level_names_case_insensitively() {
        assert_eq!(parse_log_level("off"), Some(LevelFilter::Off));
        assert_eq!(parse_log_level("ERROR"), Some(LevelFilter::Error));
        assert_eq!(parse_log_level("Warn"), Some(LevelFilter::Warn));
        assert_eq!(parse_log_level("info"), Some(LevelFilter::Info));
        assert_eq!(parse_log_level("Debug"), Some(LevelFilter::Debug));
        assert_eq!(parse_log_level("trace"), Some(LevelFilter::Trace));
        assert_eq!(parse_log_level("  trace\n"), Some(LevelFilter::Trace));
    }

    #[def_test]
    fn parse_maps_our_own_level_numbers() {
        assert_eq!(parse_log_level("0"), Some(LevelFilter::Off));
        assert_eq!(parse_log_level("1"), Some(LevelFilter::Error));
        assert_eq!(parse_log_level("2"), Some(LevelFilter::Warn));
        assert_eq!(parse_log_level("3"), Some(LevelFilter::Info));
        assert_eq!(parse_log_level("4"), Some(LevelFilter::Debug));
        assert_eq!(parse_log_level("5"), Some(LevelFilter::Trace));
        assert_eq!(parse_log_level("4\n"), Some(LevelFilter::Debug));
    }

    #[def_test]
    fn parse_rejects_unknown_and_out_of_range_input() {
        assert_eq!(parse_log_level(""), None);
        assert_eq!(parse_log_level("6"), None);
        assert_eq!(parse_log_level("9"), None);
        assert_eq!(parse_log_level("banana"), None);
    }

    #[def_test]
    fn level_number_round_trips_canonical_values() {
        for (level, number) in [
            (LevelFilter::Off, 0),
            (LevelFilter::Error, 1),
            (LevelFilter::Warn, 2),
            (LevelFilter::Info, 3),
            (LevelFilter::Debug, 4),
            (LevelFilter::Trace, 5),
        ] {
            assert_eq!(level_number(level), number);
            assert_eq!(parse_log_level(&number.to_string()), Some(level));
        }
    }

    #[def_test]
    fn level_name_matches_acceptable_input() {
        for (level, name) in [
            (LevelFilter::Off, "off"),
            (LevelFilter::Error, "error"),
            (LevelFilter::Warn, "warn"),
            (LevelFilter::Info, "info"),
            (LevelFilter::Debug, "debug"),
            (LevelFilter::Trace, "trace"),
        ] {
            assert_eq!(level_name(level), name);
            assert_eq!(parse_log_level(name), Some(level));
        }
    }

    #[def_test]
    fn set_log_level_takes_effect_immediately() {
        let previous = current_log_level();
        set_log_level("3");
        assert_eq!(current_log_level(), LevelFilter::Info);
        set_log_level_filter(LevelFilter::Debug);
        assert_eq!(current_log_level(), LevelFilter::Debug);
        // Invalid input must not silence the whole kernel by accident.
        set_log_level("invalid");
        assert_eq!(current_log_level(), LevelFilter::Debug);
        set_log_level_filter(previous);
    }
}

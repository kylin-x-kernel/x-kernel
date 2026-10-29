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

use kspin::{SpinNoIrq, static_lock};
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

/// Colour wrapper whose terminating SGR reset stays **inside** the line.
///
/// `color_fmt!` emits its reset after the wrapped text; when that text ends with
/// a newline the reset lands at the start of the *next* console line, where it
/// breaks anything that anchors on a line start — a `grep '^<N>'` over a serial
/// capture, or a CI status marker matched line-exact. Keeping the newline outside
/// the wrapper removes that class of surprises.
macro_rules! color_line {
    ($color_code:expr, $($arg:tt)*) => {
        format_args!("{}\n", color_fmt!($color_code, $($arg)*))
    };
}

#[repr(u8)]
enum AnsiColor {
    Red    = 31,
    Green  = 32,
    Yellow = 33,
    Cyan   = 36,
    White  = 37,
}

#[repr(u8)]
enum AnsiBrightColor {
    BrightBlack = 90,
}

// ---------------------------------------------------------------------------
// Runtime filter: a default level plus per-target overrides.
//
// One global level is not enough on real hardware: bringing the mmc stack up
// needs `debug` there, while `debug` everywhere drowns the console in IPI and
// syscall lines. A spec is a default level plus `target=level` overrides
// matched by longest path prefix, e.g.
//   `warn ksyscall=debug drv_uart=debug`
// ---------------------------------------------------------------------------

/// Maximum number of `target=level` overrides in a spec.
pub const MAX_FILTER_RULES: usize = 12;
/// Maximum length of one override's target prefix.
pub const MAX_FILTER_TARGET: usize = 24;
/// Longest level name (`error`, `debug`, `trace`, ...).
const MAX_LEVEL_NAME: usize = 5;
/// Longest canonical spec text, i.e. what [`filter_spec`] can produce for any
/// spec [`set_filter_spec`] accepts.
///
/// This must cover the whole accepted domain: a spec that can be installed but
/// not reported back would make `/proc/sys/kernel/printk` unreadable (the node
/// returns `InvalidInput` when the text does not fit) and hide the active
/// overrides from the boot banner. Layout:
///
/// ```text
/// <default> ( [" " <target> "=" <level>]{0,MAX_FILTER_RULES} )
/// ```
pub const MAX_FILTER_SPEC: usize =
    MAX_LEVEL_NAME + MAX_FILTER_RULES * (1 + MAX_FILTER_TARGET + 1 + MAX_LEVEL_NAME);

#[derive(Clone, Copy)]
struct FilterRule {
    target: [u8; MAX_FILTER_TARGET],
    target_len: u8,
    level: LevelFilter,
}

const EMPTY_RULE: FilterRule = FilterRule {
    target: [0; MAX_FILTER_TARGET],
    target_len: 0,
    level: LevelFilter::Off,
};

struct Filter {
    /// Build-time level (`LOG_LEVEL_*`): the logger's default and its **floor**.
    ///
    /// A runtime spec may ask for more — a `loglevel=debug` on a `warn` image
    /// still reveals the `debug!` sites compiled into it — but it can never take
    /// the logger below this, so quietening the console cannot starve the ring.
    floor: LevelFilter,
    /// Level used when no rule matches.
    default: LevelFilter,
    rules: [FilterRule; MAX_FILTER_RULES],
    count: usize,
}

impl Filter {
    const fn new() -> Self {
        Self {
            floor: LevelFilter::Error,
            default: LevelFilter::Warn,
            rules: [EMPTY_RULE; MAX_FILTER_RULES],
            count: 0,
        }
    }

    /// Highest level the console filter asks for: the default or any override.
    fn console_ceiling(&self) -> LevelFilter {
        self.rules[..self.count]
            .iter()
            .map(|rule| rule.level)
            .fold(self.default, |acc, level| acc.max(level))
    }

    /// What `log::max_level` must be: whatever the console filter wants, but
    /// never below the build floor.
    fn ceiling(&self) -> LevelFilter {
        self.floor.max(self.console_ceiling())
    }

    /// Level for `target`, longest matching prefix wins.
    fn level_for(&self, target: &str) -> LevelFilter {
        let mut best: Option<(usize, LevelFilter)> = None;
        for rule in &self.rules[..self.count] {
            let Ok(name) = core::str::from_utf8(&rule.target[..rule.target_len as usize]) else {
                continue;
            };
            if target.starts_with(name) && best.is_none_or(|(len, _)| name.len() >= len) {
                best = Some((name.len(), rule.level));
            }
        }
        best.map_or(self.default, |(_, level)| level)
    }
}

static_lock! {
    static FILTER: SpinNoIrq<Filter> = SpinNoIrq::new(Filter::new());
}

/// Why a filter spec was rejected. The spec is applied atomically, so a
/// rejected one leaves the previous filter in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilterSpecError;

/// Applies a filter spec: `[<level>] [<target>=<level>]...`, levels being
/// `0..=5` or `off/error/warn/info/debug/trace` and targets matched by prefix
/// (a trailing `*` is accepted and redundant). Tokens may be separated by
/// spaces or commas.
///
/// A bare level sets the default level for every target; without one the
/// **current default is kept** and only the rule set is replaced, so
/// `ksyscall=trace` narrows one target instead of silently resetting the rest
/// of the kernel to a hard-coded level.
///
/// The spec is validated before it is applied, so a bad token leaves the
/// current filter untouched.
pub fn set_filter_spec(spec: &str) -> Result<(), FilterSpecError> {
    let mut next = Filter::new();
    let mut default: Option<LevelFilter> = None;
    for token in spec.split(|c: char| c.is_ascii_whitespace() || c == ',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        match token.split_once('=') {
            Some((target, level)) => {
                let target = target.trim().trim_end_matches('*');
                let level = parse_log_level(level).ok_or(FilterSpecError)?;
                if target.is_empty() || target.len() > MAX_FILTER_TARGET {
                    return Err(FilterSpecError);
                }
                if next.count == MAX_FILTER_RULES {
                    return Err(FilterSpecError);
                }
                let mut rule = EMPTY_RULE;
                rule.target[..target.len()].copy_from_slice(target.as_bytes());
                rule.target_len = target.len() as u8;
                rule.level = level;
                next.rules[next.count] = rule;
                next.count += 1;
            }
            None => {
                default = Some(parse_log_level(token).ok_or(FilterSpecError)?);
            }
        }
    }
    if default.is_none() && next.count == 0 {
        return Err(FilterSpecError);
    }
    // Publish the rule set, the inherited default and the `log` ceiling in one
    // critical section, so the installed filter and the ceiling cannot disagree.
    let mut filter = FILTER.lock();
    filter.rules = next.rules;
    filter.count = next.count;
    filter.default = default.unwrap_or(filter.default);
    log::set_max_level(filter.ceiling());
    Ok(())
}

/// Writes the canonical spec (`warn ksyscall=debug`) into `buf` and returns its
/// length, or `None` when `buf` is too small.
pub fn filter_spec(buf: &mut [u8]) -> Option<usize> {
    let filter = FILTER.lock();
    let mut out = FilterSpecWriter { buf, len: 0 };
    if out.put_str(level_name(filter.default)).is_err() {
        return None;
    }
    for rule in &filter.rules[..filter.count] {
        let name = core::str::from_utf8(&rule.target[..rule.target_len as usize]).ok()?;
        out.put_str(" ").ok()?;
        out.put_str(name).ok()?;
        out.put_str("=").ok()?;
        out.put_str(level_name(rule.level)).ok()?;
    }
    Some(out.len)
}

struct FilterSpecWriter<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl FilterSpecWriter<'_> {
    fn put_str(&mut self, text: &str) -> fmt::Result {
        let end = self.len + text.len();
        if end > self.buf.len() {
            return Err(fmt::Error);
        }
        self.buf[self.len..end].copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

struct KernelLogger;

impl Write for KernelLogger {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        cfg_if::cfg_if! {
            if #[cfg(feature = "std")] {
                std::print!("{s}");
            } else {
                kiface_logging_api::LogSinkIf::write_str(s);
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
        // `enabled` is true for every level (the level is compiled in); the real
        // decision is here, where the per-target filter is known.
        if !self.enabled(record.metadata()) {
            return;
        }
        // An NMI cannot be masked, so a log call from NMI context would spin on
        // `FILTER` if the interrupted context on this CPU holds it. Drop the
        // record instead of taking any lock; emergency output uses the platform's
        // atomic console path. The host (`std`) build has no kiface providers
        // linked and no NMI, so it does not ask.
        #[cfg(not(feature = "std"))]
        if kiface_logging_api::LogContextIf::in_nmi() {
            return;
        }

        if !filter_enabled(record.target(), record.level()) {
            return;
        }
        let line = record.line().unwrap_or(0);
        let path = record.target();
        let compact = path == "unittest";
        let color = get_level_color(record.level());

        cfg_if::cfg_if! {
            if #[cfg(feature = "std")] {
                let _ = if compact {
                    print_fmt(color_line!(
                        AnsiColor::White,
                        "[{time}] {args}",
                        time = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.6f"),
                        args = color_fmt!(color, "{}", record.args()),
                    ))
                } else {
                    print_fmt(color_line!(
                        AnsiColor::White,
                        "[{time} {path}:{line}] {args}",
                        time = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.6f"),
                        path = path,
                        line = line,
                        args = color_fmt!(color, "{}", record.args()),
                    ))
                };
            } else {
                let cpu_id = kiface_logging_api::LogContextIf::cpu_id();
                let tid = kiface_logging_api::LogContextIf::task_id();
                let now = kiface_logging_api::LogContextIf::now();

                // Logging is best effort; reporting its format error via the logger would recurse.
                let _ = match (compact, cpu_id, tid) {
                    (true, Some(c), Some(t)) => print_fmt(color_line!(
                        AnsiColor::White,
                        "[{:>3}.{:06} {c}:{t}] {args}",
                        now.as_secs(),
                        now.subsec_micros(),
                        c = c,
                        t = t,
                        args = color_fmt!(color, "{}", record.args()),
                    )),
                    (true, Some(c), None) => print_fmt(color_line!(
                        AnsiColor::White,
                        "[{:>3}.{:06} {c}] {args}",
                        now.as_secs(),
                        now.subsec_micros(),
                        c = c,
                        args = color_fmt!(color, "{}", record.args()),
                    )),
                    (true, _, _) => print_fmt(color_line!(
                        AnsiColor::White,
                        "[{:>3}.{:06}] {args}",
                        now.as_secs(),
                        now.subsec_micros(),
                        args = color_fmt!(color, "{}", record.args()),
                    )),
                    (false, Some(c), Some(t)) => print_fmt(color_line!(
                        AnsiColor::White,
                        "[{:>3}.{:06} {c}:{t} {path}:{line}] {args}",
                        now.as_secs(),
                        now.subsec_micros(),
                        c = c,
                        t = t,
                        path = path,
                        line = line,
                        args = color_fmt!(color, "{}", record.args()),
                    )),
                    (false, Some(c), None) => print_fmt(color_line!(
                        AnsiColor::White,
                        "[{:>3}.{:06} {c} {path}:{line}] {args}",
                        now.as_secs(),
                        now.subsec_micros(),
                        c = c,
                        path = path,
                        line = line,
                        args = color_fmt!(color, "{}", record.args()),
                    )),
                    (false, _, _) => print_fmt(color_line!(
                        AnsiColor::White,
                        "[{:>3}.{:06} {path}:{line}] {args}",
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
    static_lock! {
        static LOCK: SpinNoIrq<()> = SpinNoIrq::new(());
    }

    let _guard = LOCK.lock();
    KernelLogger.write_fmt(args)
}

/// Installs the global `log` logger and fixes the build-time level.
///
/// `build_level` is the compiled-in `LOG_LEVEL_*` choice, parsed here because
/// this crate owns the level vocabulary. It is both the console default and the
/// **floor** of the `log` crate's filter: a runtime spec (a `loglevel=` on the
/// command line, a device-tree property, a `/proc/sys/kernel/log_filter` write)
/// may raise the ceiling above it and reveal the higher-level sites compiled into
/// the image, but nothing can take the logger below it — that is what keeps a
/// quiet console from starving the ring.
///
/// The cost of a raised ceiling is paid only when it is actually requested: with
/// no spec asking for more, records above the build level are dropped by the
/// `log` macros for the price of a comparison.
///
/// A configuration that selects no level (`configured_log_level()` reports
/// `off`) is mapped to `error` rather than silencing the kernel: the Kconfig
/// choice has no `off` option, so an empty selection is a broken configuration,
/// and losing only the warnings is a safer fallback than losing everything.
///
/// Call once from primary-CPU boot after the `kiface_logging_api` providers are
/// linked; `kruntime` then applies the resolved boot filter spec through
/// [`set_log_level`]. Records emitted before this call are dropped by the `log`
/// crate, which is why early boot uses [`kprintln!`] instead of the macros.
pub fn init_klogger(build_level: &str) {
    let cap = build_floor_from(build_level);
    #[expect(
        clippy::unwrap_used,
        reason = "CG-021-no-panic-on-reachable-failure: failure to install the global logger \
                  during primary boot is unrecoverable; this initialization entry has no error \
                  return or recovery caller"
    )]
    log::set_logger(&KernelLogger).unwrap();
    let mut filter = FILTER.lock();
    filter.floor = cap;
    filter.default = cap;
    log::set_max_level(filter.ceiling());
}

/// Resolves the build level into the logger's floor.
///
/// The Kconfig choice has no `off` option, so `off` (or an unparsable value) can
/// only come from a broken configuration; losing just the warnings is a safer
/// fallback than silencing the kernel.
pub fn build_floor_from(build_level: &str) -> LevelFilter {
    parse_log_level(build_level)
        .filter(|level| *level != LevelFilter::Off)
        .unwrap_or(LevelFilter::Error)
}

/// The build-time level (`LOG_LEVEL_*`): the console default and the floor of the
/// logger's filter.
///
/// A spec may ask for a higher level and get it; it cannot go below this.
pub fn build_level_floor() -> LevelFilter {
    FILTER.lock().floor
}

/// The level the `log` crate currently admits, i.e. `max(build floor, console
/// ceiling)`. Diagnostics and tests report what is really in force with it.
pub fn effective_ceiling() -> LevelFilter {
    log::max_level()
}

/// Whether a record at `level` from `target` should reach the console.
fn filter_enabled(target: &str, level: Level) -> bool {
    let filter = FILTER.lock();
    let allowed = filter.level_for(target);
    allowed >= level_filter_of(level)
}

const fn level_filter_of(level: Level) -> LevelFilter {
    match level {
        Level::Error => LevelFilter::Error,
        Level::Warn => LevelFilter::Warn,
        Level::Info => LevelFilter::Info,
        Level::Debug => LevelFilter::Debug,
        Level::Trace => LevelFilter::Trace,
    }
}

/// Returns the default level of the active filter (no per-target override
/// applied).
pub fn current_log_level() -> LevelFilter {
    FILTER.lock().default
}

/// Sets the console default level at runtime, keeping every per-target override.
///
/// This is the knob behind `/proc/sys/kernel/printk`, which only ever carried the
/// level: writing it must not silently drop the module filter that
/// `/proc/sys/kernel/log_filter` owns.
pub fn set_console_default(level: LevelFilter) {
    let mut filter = FILTER.lock();
    filter.default = level;
    log::set_max_level(filter.ceiling());
}

/// Sets the console default level at runtime and drops every per-target
/// override.
///
/// Takes effect immediately for the console; the ring keeps recording everything
/// up to the build cap, and a level above that cap cannot be produced until the
/// kernel is rebuilt.
pub fn set_log_level_filter(level: LevelFilter) {
    let mut filter = FILTER.lock();
    filter.default = level;
    filter.count = 0;
    log::set_max_level(filter.ceiling());
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

/// Applies a filter spec (see [`set_filter_spec`]); a bare level name/number
/// behaves like [`set_log_level_filter`]. Invalid input leaves the current
/// filter unchanged.
pub fn set_log_level(spec: &str) {
    let _ = set_filter_spec(spec.trim());
}

unittest::path_mod_test!(tests, "../ut-tests/lib.rs");

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel logging utilities and macros.
#![cfg_attr(not(feature = "std"), no_std)]

extern crate log;

use core::{
    fmt::{self, Write},
    str::FromStr,
    sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU64, Ordering},
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

// ---------------------------------------------------------------------------
// Kernel log ring buffer ("dmesg"): every record that passes the `log`
// ceiling is stored here, whether or not the console prints it. That is what
// makes a quiet console and a complete boot log compatible: the console filter
// decides what to *show*, this buffer keeps what happened, and userland reads
// it back through `syslog(2)` / `dmesg`.
// ---------------------------------------------------------------------------

/// Fixed size of the kernel log buffer.
pub const LOGBUF_SIZE: usize = 64 * 1024;
/// Longest single stored line; longer records are truncated.
const LOGBUF_LINE: usize = 256;
/// Number of records the index tracks.
///
/// The ring stores bodies only; each published body's location lives in a slot,
/// which is what makes a record boundary exact without putting framing bytes
/// into the text that `dmesg` prints and without scanning for newlines (a
/// message body may contain one). The ring keeps at most this many records even
/// when its byte budget would allow more, so a flood of very short records
/// retains less than 64 KiB.
const LOGBUF_RECORDS: usize = 1024;
/// How often a read restarts when the ring overwrites its window mid-copy.
const LOGBUF_READ_RETRIES: usize = 8;

/// Where one published record lives, indexed by its sequence number.
///
/// `seq` is the publication word: it holds 0 until the body is fully copied and
/// then the record's sequence number, stored with `Release`. A reader loads it
/// with `Acquire`, so seeing `seq == n` guarantees the `at`/`len` it reads are
/// the ones written for record `n`, and that the body bytes it copied are the
/// body of record `n` (a later record can overwrite those bytes, which the
/// reader detects separately). Slots are recycled: record `n` and record
/// `n + LOGBUF_RECORDS` share a slot, and a reader tells them apart by exact
/// `seq` comparison.
struct Slot {
    seq: AtomicU64,
    /// Absolute position of the body in the reservation space. Absolute, not
    /// modulo the ring, because it is what tells a reader whether the body has
    /// already been overwritten.
    at: AtomicU64,
    len: AtomicU16,
}

impl Slot {
    const fn new() -> Self {
        Self {
            seq: AtomicU64::new(0),
            at: AtomicU64::new(0),
            len: AtomicU16::new(0),
        }
    }
}

/// Bodies, as single-byte atomics.
///
/// They are atomics rather than a plain array because a writer may be copying
/// into the ring while a reader copies out of it. Each access is `Relaxed`: the
/// `Release`/`Acquire` pair on [`Slot::seq`] and the final tail check order the
/// two sides, so the bytes themselves need no stronger ordering. On the targets
/// X-Kernel builds for, a relaxed single-byte atomic compiles to an ordinary
/// `ldrb`/`strb`, so the record path does not pay a fence per byte.
static LOGBUF_BYTES: [AtomicU8; LOGBUF_SIZE] = [const { AtomicU8::new(0) }; LOGBUF_SIZE];
static LOGBUF_SLOTS: [Slot; LOGBUF_RECORDS] = [const { Slot::new() }; LOGBUF_RECORDS];
/// End of the reservation space: a record's body starts where the reservation
/// returned, and no writer ever writes below `LOGBUF_TAIL - LOGBUF_SIZE`.
static LOGBUF_TAIL: AtomicU64 = AtomicU64::new(0);
/// Sequence number of the newest record whose body is complete.
static LOGBUF_SEQ: AtomicU64 = AtomicU64::new(0);
/// Records at or below this sequence number were cleared and are not readable.
static LOGBUF_CLEARED: AtomicU64 = AtomicU64::new(0);

/// The slot that holds record `seq` (1-based).
fn slot_of(seq: u64) -> &'static Slot {
    &LOGBUF_SLOTS[((seq - 1) % LOGBUF_RECORDS as u64) as usize]
}

/// Where record `seq` is, if it is still published and still inside the ring.
///
/// Whether a record slot is readable right now.
enum SlotState {
    /// Published and still inside the ring.
    Live { at: u64, len: usize },
    /// The sequence number is out but the slot still names the previous
    /// incarnation, so the writer has not published the slot yet. The record
    /// becomes readable in a moment; a reader skips it.
    Pending,
    /// A newer incarnation has taken the slot (its sequence number is higher than
    /// the one asked for), so this record can never become readable again.
    Recycled,
    /// The ring wrapped past the body, so the record is gone for good.
    Gone,
}

/// The state of record `seq`, given the oldest absolute position still valid.
///
/// The sequence number is loaded with `Acquire` **before** the slot's `at`/`len`:
/// the writer stores those with `Relaxed` and then publishes `seq` with
/// `Release`, so only an acquire load of `seq` orders the metadata reads after
/// it. Loading the metadata first could pair a new sequence number with the
/// previous occupant's `at`/`len`. The second `seq` load catches a slot that was
/// recycled while the metadata was read.
fn slot_state(seq: u64, oldest_at: u64) -> SlotState {
    let slot = slot_of(seq);
    let published = slot.seq.load(Ordering::Acquire);
    if published != seq {
        // A slot's sequence number only grows, so a value above `seq` means a
        // newer record already took this slot: the record is gone, not pending.
        return if published > seq {
            SlotState::Recycled
        } else {
            SlotState::Pending
        };
    }
    let at = slot.at.load(Ordering::Relaxed);
    let len = slot.len.load(Ordering::Relaxed) as usize;
    if slot.seq.load(Ordering::Acquire) != seq {
        return SlotState::Recycled;
    }
    if at < oldest_at || at + len as u64 > LOGBUF_TAIL.load(Ordering::Acquire) {
        return SlotState::Gone;
    }
    SlotState::Live { at, len }
}

/// Appends `line` as one record. Never blocks and never disables interrupts.
///
/// The body is reserved with one `fetch_add` on [`LOGBUF_TAIL`], so writers on
/// different CPUs never wait for each other: each owns a disjoint byte range,
/// writes it, and only then publishes its slot. Publication order can therefore
/// differ from byte order, which is why a record's identity is its sequence
/// number and not its position.
fn logbuf_push(line: &[u8]) {
    let len = line.len().min(LOGBUF_SIZE);
    if len == 0 {
        return;
    }
    let at = LOGBUF_TAIL.fetch_add(len as u64, Ordering::Relaxed);
    let offset = (at % LOGBUF_SIZE as u64) as usize;
    let head = len.min(LOGBUF_SIZE - offset);
    for (i, byte) in line[..head].iter().enumerate() {
        LOGBUF_BYTES[offset + i].store(*byte, Ordering::Relaxed);
    }
    for (i, byte) in line[head..len].iter().enumerate() {
        LOGBUF_BYTES[i].store(*byte, Ordering::Relaxed);
    }
    let seq = LOGBUF_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    let slot = slot_of(seq);
    slot.at.store(at, Ordering::Relaxed);
    slot.len.store(len as u16, Ordering::Relaxed);
    // Release: the body and the slot fields above are visible to a reader that
    // loads this value with Acquire.
    slot.seq.store(seq, Ordering::Release);
}

/// Copies the body of one record into `out`.
fn copy_record(at: u64, len: usize, out: &mut [u8]) {
    let offset = (at % LOGBUF_SIZE as u64) as usize;
    let head = len.min(LOGBUF_SIZE - offset);
    for i in 0..head {
        out[i] = LOGBUF_BYTES[offset + i].load(Ordering::Relaxed);
    }
    for i in 0..len - head {
        out[head + i] = LOGBUF_BYTES[i].load(Ordering::Relaxed);
    }
}

/// The newest whole records that fit in `cap` bytes.
///
/// Returns `(first, newest, bytes)` — the records `first..=newest` — or `None`
/// when nothing is readable. Walks back from the newest record and stops at the
/// first one that is unpublished, cleared or already overwritten, so a window is
/// always a contiguous run of real records and never mixes two ring states.
fn newest_window(cap: usize) -> Option<(u64, u64, usize)> {
    let cleared = LOGBUF_CLEARED.load(Ordering::Acquire);
    let mut newest = LOGBUF_SEQ.load(Ordering::Acquire);
    if newest <= cleared {
        return None;
    }
    let oldest_at = LOGBUF_TAIL
        .load(Ordering::Acquire)
        .saturating_sub(LOGBUF_SIZE as u64);
    // `logbuf_push` makes the sequence number visible after the body is copied
    // but before the slot is published, so a reader can sample a newest record
    // that is still being published. Step down to the highest published record
    // instead of returning "nothing": the older records are perfectly readable.
    //
    // The step-down is bounded: only an in-flight publication (a handful of
    // records, one per writing CPU) can be skipped. If the sample is so old that
    // the ring already recycled its slot, `LOGBUF_SEQ` moved on and the window is
    // recomputed from a fresh sample rather than walking sequence numbers.
    let mut skipped = 0;
    loop {
        match slot_state(newest, oldest_at) {
            SlotState::Live { .. } => break,
            SlotState::Pending if skipped < LOGBUF_RECORDS => {
                newest -= 1;
                skipped += 1;
                if newest <= cleared {
                    return None;
                }
            }
            // Recycled, gone, or too many pending hops: the sample is stale.
            SlotState::Pending | SlotState::Recycled | SlotState::Gone => {
                let refreshed = LOGBUF_SEQ.load(Ordering::Acquire);
                if refreshed <= newest {
                    return None;
                }
                newest = refreshed;
                skipped = 0;
            }
        }
    }
    let mut first = newest + 1;
    let mut bytes = 0;
    while first > cleared + 1 && newest + 1 - first < LOGBUF_RECORDS as u64 {
        let seq = first - 1;
        match slot_state(seq, oldest_at) {
            SlotState::Live { len, .. } => {
                if bytes + len > cap {
                    break;
                }
                bytes += len;
                first = seq;
            }
            // A hole below the newest record: end the window here rather than
            // returning records that skip over it.
            SlotState::Pending | SlotState::Recycled | SlotState::Gone => break,
        }
    }
    if first > newest {
        return None;
    }
    Some((first, newest, bytes))
}

/// Bytes currently held in the kernel log buffer.
pub fn logbuf_len() -> usize {
    newest_window(usize::MAX)
        .map(|(_, _, bytes)| bytes)
        .unwrap_or(0)
}

/// Copies the newest **whole records** that fit into `out`, oldest first, and
/// returns how many bytes were written.
///
/// A thin wrapper over [`logbuf_read_newest_seq`] for callers that do not clear
/// afterwards.
pub fn logbuf_read_newest(out: &mut [u8]) -> usize {
    logbuf_read_newest_seq(out).0
}

/// Copies the newest **whole records** that fit into `out` and reports the
/// window's newest record sequence number, which a `READ_CLEAR` uses as its
/// clear watermark.
///
/// The window always starts and ends on a record boundary: when even the newest
/// record does not fit, nothing is copied and the return value is `(0, 0)` — the
/// same outcome Linux's `syslog_print_all` produces. Reading never consumes.
///
/// Neither side takes a lock. Writers reserve bodies with `fetch_add` and publish
/// a slot, so a writer can wrap the ring onto the window while the copy runs;
/// records are therefore copied newest first and each one is validated against
/// the tail after it was copied. A wrap then costs the oldest records, not the
/// newest: the read returns the surviving suffix of the window instead of an
/// empty result, so a full ring under sustained logging never reports an empty
/// log. Only a window whose newest record is already gone returns `(0, 0)`.
pub fn logbuf_read_newest_seq(out: &mut [u8]) -> (usize, u64) {
    for _ in 0..LOGBUF_READ_RETRIES {
        let Some((first, newest, total)) = newest_window(out.len()) else {
            return (0, 0);
        };
        // Walk backwards from the newest record, so the destination offsets of
        // the records that survive a concurrent wrap stay valid.
        let mut offset = total;
        let mut accepted = None;
        let mut seq = newest;
        loop {
            let tail = LOGBUF_TAIL.load(Ordering::Acquire);
            let oldest_at = tail.saturating_sub(LOGBUF_SIZE as u64);
            let SlotState::Live { at, len } = slot_state(seq, oldest_at) else {
                break;
            };
            if len > offset {
                break;
            }
            let begin = offset - len;
            copy_record(at, len, &mut out[begin..offset]);
            // The bytes are still this record's until the ring wraps past its
            // start; a newer record is overwritten strictly later, so the first
            // failure bounds the suffix that survives.
            if LOGBUF_TAIL.load(Ordering::Acquire) >= at + LOGBUF_SIZE as u64 {
                break;
            }
            offset = begin;
            accepted = Some(seq);
            if seq == first {
                break;
            }
            seq -= 1;
        }
        if accepted.is_none() {
            // Even the newest record did not survive: retry from a fresh window.
            continue;
        }
        if offset > 0 {
            out.copy_within(offset..total, 0);
        }
        // The accepted suffix always ends at the window's newest record, which is
        // therefore the clear watermark a `READ_CLEAR` should use.
        return (total - offset, newest);
    }
    (0, 0)
}

/// Drops every record up to and including `seq`, the watermark a `READ_CLEAR`
/// takes from [`logbuf_read_newest_seq`].
///
/// Records logged after that watermark — including those logged while the read
/// was copying — stay readable, which is Linux's `clear_seq` behaviour: a reader
/// that loses records to concurrent logging can read them next time instead of
/// losing them silently.
pub fn logbuf_clear_through(seq: u64) {
    LOGBUF_CLEARED.fetch_max(seq, Ordering::AcqRel);
}

/// Drops everything in the kernel log buffer.
///
/// The bodies stay in the ring; the clear watermark is what makes them
/// unreadable, so a reader that is copying a window keeps a consistent picture
/// instead of reading bytes that were zeroed underneath it.
pub fn logbuf_clear() {
    LOGBUF_CLEARED.store(LOGBUF_SEQ.load(Ordering::Acquire), Ordering::Release);
}

/// Fixed-capacity line writer: logging must not allocate, and a runaway
/// message must not lose the interesting prefix.
struct LineBuf {
    buf: [u8; LOGBUF_LINE],
    len: usize,
}

impl LineBuf {
    const fn new() -> Self {
        Self {
            buf: [0; LOGBUF_LINE],
            len: 0,
        }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// Appends the record's terminating newline.
    ///
    /// Kept separate from the formatting path so the terminator cannot be
    /// truncated away: `write_str` reserves the byte, this writes it.
    fn terminate(&mut self) {
        self.buf[self.len] = b'\n';
        self.len += 1;
    }
}

impl fmt::Write for LineBuf {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        // One byte is reserved for the record's terminating newline: the caller
        // formats the body with `write!` and then appends the terminator through
        // `LineBuf::terminate`, so a record that filled the buffer would
        // otherwise lose it and run into the next record.
        let room = self.buf.len() - 1 - self.len;
        let mut take = text.len().min(room);
        // Copy whole characters only; a split code point would put invalid
        // UTF-8 into the ring and into every `dmesg` reader.
        while take > 0 && !text.is_char_boundary(take) {
            take -= 1;
        }
        self.buf[self.len..self.len + take].copy_from_slice(&text.as_bytes()[..take]);
        self.len += take;
        Ok(())
    }
}

/// Maximum number of CPUs the recursion guard tracks.
///
/// The guard only needs to catch a CPU re-entering the logger, which happens
/// with a console sink that logs or with a fault taken inside the logger. A CPU
/// id beyond this bound simply skips the guard rather than growing state.
const MAX_GUARD_CPUS: usize = 64;

/// Per-CPU "this CPU is inside the logger" flags.
static LOGGING: [AtomicBool; MAX_GUARD_CPUS] = [const { AtomicBool::new(false) }; MAX_GUARD_CPUS];

/// Marks the current CPU as inside the logger for as long as it lives.
struct RecursionGuard {
    slot: Option<&'static AtomicBool>,
}

impl RecursionGuard {
    /// Returns `None` when this CPU is already inside the logger.
    fn enter() -> Option<Self> {
        // The host (`std`) build links no kiface provider, so it must not ask
        // one: it runs one thread per logger and needs no per-CPU guard.
        #[cfg(feature = "std")]
        let cpu = None;
        #[cfg(not(feature = "std"))]
        let cpu = kiface_logging_api::LogContextIf::cpu_id();
        let slot = cpu.and_then(|cpu| LOGGING.get(cpu.min(MAX_GUARD_CPUS - 1)));
        match slot {
            // `Acquire` on success pairs with the `Release` store in `drop`; the
            // failure case only needs the current value, so it stays `Relaxed`.
            Some(slot) => slot
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .ok()
                .map(|_| Self { slot: Some(slot) }),
            // No CPU identity yet (very early boot): skip the guard.
            None => Some(Self { slot: None }),
        }
    }
}

impl Drop for RecursionGuard {
    fn drop(&mut self) {
        if let Some(slot) = self.slot {
            // `Release` publishes everything this CPU did inside the logger to the
            // next CPU that claims the slot with an `Acquire` exchange.
            slot.store(false, Ordering::Release);
        }
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
        // The other context that must not reach the locks is a nested record on
        // the same CPU — a console sink that logs, or a fault inside the logger.
        // It would deadlock on `SpinNoIrq` instead of reporting anything.
        let Some(_guard) = RecursionGuard::enter() else {
            return;
        };

        capture_record(record);

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

/// The `[time cpu:tid target:line]` prefix, in the same shape the console
/// uses (without the per-level colour).
struct LineText<'a>(&'a Record<'a>);

impl fmt::Display for LineText<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let record = self.0;
        let path = record.target();
        let line = record.line().unwrap_or(0);
        cfg_if::cfg_if! {
            if #[cfg(feature = "std")] {
                write!(f, "[{} {}:{}] {}", chrono::Local::now().format("%H:%M:%S%.6f"), path, line, record.args())
            } else {
                let now = kiface_logging_api::LogContextIf::now();
                match (
                    kiface_logging_api::LogContextIf::cpu_id(),
                    kiface_logging_api::LogContextIf::task_id(),
                ) {
                    (Some(c), Some(t)) => write!(
                        f,
                        "[{:>3}.{:06} {c}:{t} {path}:{line}] {}",
                        now.as_secs(),
                        now.subsec_micros(),
                        record.args()
                    ),
                    (Some(c), None) => write!(
                        f,
                        "[{:>3}.{:06} {c} {path}:{line}] {}",
                        now.as_secs(),
                        now.subsec_micros(),
                        record.args()
                    ),
                    _ => write!(
                        f,
                        "[{:>3}.{:06} {path}:{line}] {}",
                        now.as_secs(),
                        now.subsec_micros(),
                        record.args()
                    ),
                }
            }
        }
    }
}

/// Writes already formatted text to the console output path.
///
/// This is the primitive behind [`kprint!`] and [`kprintln!`]: it serializes
/// console writes with an IRQ-safe lock, but bypasses the per-target filter and
/// the kernel log buffer (`kprint!` output is not visible to `dmesg`).
///
/// May be called in any execution context.
/// Appends one record to the kernel log buffer, in the shape `dmesg` prints.
///
/// Kept out of [`Log::log`] so the record path (guards, capture, filter) and the
/// console formatting each stay readable; the console half is unchanged from the
/// pre-ring logger.
fn capture_record(record: &Record) {
    // Capture first, print second: the buffer is the record of what
    // happened, the console filter only decides what is shown. The macros
    // already dropped everything above the build cap, so every record that
    // reaches this point is stored — the ring is never narrowed by the
    // console filter. The `<level>` prefix is X-Kernel's own numbering
    // (`1..=5`, error..trace), so `dmesg | grep '^<1>'` selects one level.
    let mut line = LineBuf::new();
    let level = level_filter_of(record.level());
    // The trailing newline is part of the record: the console path adds its
    // own, and without it every buffered record runs into the next one, so
    // `dmesg` prints the whole log as a single line. It is appended after
    // formatting because a record that fills the line buffer would otherwise
    // lose it to truncation.
    let _ = write!(line, "<{}>{}", level_number(level), LineText(record));
    line.terminate();
    logbuf_push(line.as_bytes());
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

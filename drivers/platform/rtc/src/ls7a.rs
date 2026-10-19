// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! LS7A RTC time-of-year readout, year-boundary consistent.
//!
//! The LS7A TOY counters pack month/day/hour/min/sec/millis into a 32-bit
//! `TOY_READ0` word and the calendar year (minus 1900) into `TOY_READ1`.
//! Because the two counters are not read atomically, a sample straddling
//! new-year can pair an old year with a new-year date. [`read_mapped`]
//! re-samples until the year is stable.

use khal::mem::VirtAddr;
use ktime_types::{NANOS_PER_MILLIS, SystemTime};

const SYS_TOY_READ0: usize = 0x2C;
const SYS_TOY_READ1: usize = 0x30;
const SYS_RTCCTRL: usize = 0x40;
const TOY_ENABLE: u32 = 1 << 11;
const OSC_ENABLE: u32 = 1 << 8;
/// Maximum number of resampling attempts when the RTC year boundary is
/// unstable. A normal new-year rollover is resolved by the very next sample
/// (the year halves match on the second attempt); exhausting this budget
/// therefore indicates a persistent counter anomaly rather than a crossing,
/// and the read is rejected instead of publishing a torn value.
const RTC_MAX_RETRIES: usize = 4;

/// Extract the bit field `range` (half-open, `[start, end)`) from `value`.
fn extract_bits(value: u32, range: core::ops::Range<u32>) -> u32 {
    (value >> range.start) & ((1 << (range.end - range.start)) - 1)
}

/// Register access surface for the LS7A RTC TOY (time-of-year) counters.
///
/// The production implementation ([`MmioRtc`]) talks to MMIO; tests
/// substitute a scripted backend to drive year-boundary tearing scenarios
/// without touching real hardware.
trait RtcToyAccess {
    /// Write the RTC control register.
    fn write_ctrl(&mut self, val: u32);
    /// Read the TOY year counter (calendar year minus 1900).
    fn read_toy_high(&mut self) -> u32;
    /// Read the TOY date/time counter (month..millis packed per the LS7A layout).
    fn read_toy_low(&mut self) -> u32;
}

/// MMIO-backed register access; the production backend.
struct MmioRtc {
    base: usize,
}

impl RtcToyAccess for MmioRtc {
    fn write_ctrl(&mut self, val: u32) {
        // SAFETY: `base` is the kernel virtual address of the 4 KiB LS7A RTC
        // aperture established by `read_mapped`'s caller via `iomap_device`.
        // `SYS_RTCCTRL` is a 32-bit register offset within that mapped region,
        // and the volatile write models device-memory semantics, not
        // synchronization.
        unsafe {
            ((self.base + SYS_RTCCTRL) as *mut u32).write_volatile(val);
        }
    }

    fn read_toy_high(&mut self) -> u32 {
        // SAFETY: `SYS_TOY_READ1` is a 32-bit register offset within the mapped
        // LS7A RTC aperture; the volatile read models device-memory semantics.
        unsafe { ((self.base + SYS_TOY_READ1) as *const u32).read_volatile() }
    }

    fn read_toy_low(&mut self) -> u32 {
        // SAFETY: `SYS_TOY_READ0` is a 32-bit register offset within the mapped
        // LS7A RTC aperture; the volatile read models device-memory semantics.
        unsafe { ((self.base + SYS_TOY_READ0) as *const u32).read_volatile() }
    }
}

/// Decode a `(year, toy_low)` sample into a `SystemTime`, mirroring the LS7A
/// TOY bit layout: month 26..32, day 21..26, hour 16..21, minute 10..16,
/// second 4..10, millisecond 0..4.
///
/// The raw bit fields are wider than the valid calendar ranges (month, day,
/// and hour can encode values such as 0 or 24+), so a faulting or misbehaving
/// counter can yield bits that form no real timestamp. Such input returns
/// `None` instead of panicking; the caller applies its own failed-sample
/// policy.
fn build_system_time(year: u32, toy_low: u32) -> Option<SystemTime> {
    use chrono::{TimeZone, Timelike, Utc};
    let date_time = Utc
        .with_ymd_and_hms(
            1900 + year as i32,
            extract_bits(toy_low, 26..32),
            extract_bits(toy_low, 21..26),
            extract_bits(toy_low, 16..21),
            extract_bits(toy_low, 10..16),
            extract_bits(toy_low, 4..10),
        )
        .single()?;
    let date_time =
        date_time.with_nanosecond(extract_bits(toy_low, 0..4) * NANOS_PER_MILLIS as u32)?;
    Some({
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: chrono has validated the timestamp \
                      and normalized its nanoseconds below one billion"
        )]
        SystemTime::from_unix_parts(date_time.timestamp(), date_time.nanosecond())
            .expect("chrono returns normalized nanoseconds")
    })
}

/// Sample `(year, toy_low)` with a year-boundary consistency check.
///
/// Reads year → `toy_low` → year; if the two year samples match, the date in
/// `toy_low` was captured within that year and the sample is non-torn. If they
/// differ, the read straddled the year rollover and pairs an old year with a
/// new-year date; because the year increments monotonically, re-sampling once
/// the rollover has completed yields a matching pair, so a normal crossing is
/// resolved on the next attempt. If the year halves still disagree after
/// [`RTC_MAX_RETRIES`] attempts the counter is persistently unstable and no
/// trustworthy sample exists: return `None` so the caller applies its own
/// failure policy instead of publishing a torn (wrong) wall-clock value.
fn read_year_consistent<R: RtcToyAccess>(regs: &mut R) -> Option<(u32, u32)> {
    for _ in 0..RTC_MAX_RETRIES {
        let year = regs.read_toy_high();
        let low = regs.read_toy_low();
        if year == regs.read_toy_high() {
            return Some((year, low));
        }
    }
    log::warn!("ls7a-rtc: year boundary unstable after {RTC_MAX_RETRIES} samples; rejecting read");
    None
}

/// Read the LS7A RTC and return a year-consistent `SystemTime`.
///
/// Enables the TOY/OSC counters, then samples the year and date with the
/// [`read_year_consistent`] protocol so a read straddling new-year cannot pair
/// an old year with a new-year date. An unstable year or register contents
/// that decode to no valid calendar timestamp return `None` so the caller can
/// apply its own failed-sample policy.
fn read_rtc_from<R: RtcToyAccess>(regs: &mut R) -> Option<SystemTime> {
    regs.write_ctrl(TOY_ENABLE | OSC_ENABLE);
    let (year, low) = read_year_consistent(regs)?;
    let sample = build_system_time(year, low);
    if sample.is_none() {
        log::warn!(
            "ls7a-rtc: TOY registers decode to no valid date (year={year}, toy=0x{low:08x})"
        );
    }
    sample
}

/// Samples a mapped LS7A RTC aperture.
///
/// Returns `None` when `vaddr` is the null placeholder (no RTC present) or
/// when no trustworthy sample could be decoded (an unstable year, or register
/// contents forming no valid timestamp).
pub(super) fn read_mapped(vaddr: VirtAddr) -> Option<SystemTime> {
    if vaddr.as_usize() == 0 {
        return None;
    }
    let mut regs = MmioRtc {
        base: vaddr.as_usize(),
    };
    read_rtc_from(&mut regs)
}

unittest::path_mod_test!(tests, "../ut-tests/ls7a.rs");

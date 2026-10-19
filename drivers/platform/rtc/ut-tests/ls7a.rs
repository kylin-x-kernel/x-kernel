// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ktime_types::SystemTime;
use unittest::{assert, assert_eq, assert_ne, def_test};

use super::*;

/// Encode a date into a TOY_LOW value, mirroring `extract_bits`.
fn encode_toy_low(month: u32, day: u32, hour: u32, min: u32, sec: u32, millis: u32) -> u32 {
    (millis & 0xF)
        | ((sec & 0x3F) << 4)
        | ((min & 0x3F) << 10)
        | ((hour & 0x1F) << 16)
        | ((day & 0x1F) << 21)
        | ((month & 0x3F) << 26)
}

/// Build the expected `SystemTime` for a `(year, month, day, h, m, s)`
/// tuple, where `year` is the TOY encoding (calendar year minus 1900).
fn expected_system_time(
    year: u32,
    month: u32,
    day: u32,
    hour: u32,
    min: u32,
    sec: u32,
) -> SystemTime {
    use chrono::TimeZone;
    let dt = chrono::Utc
        .with_ymd_and_hms(1900 + year as i32, month, day, hour, min, sec)
        .unwrap();
    SystemTime::from_unix_parts(dt.timestamp(), 0).expect("expected timestamp is valid")
}

/// Scripted RTC backend: returns values from a fixed call sequence.
/// `write_ctrl` is a no-op; `read_toy_high`/`read_toy_low` consume the
/// next scripted value. It never touches real MMIO.
struct ScriptedRtc {
    reads: [u32; 16],
    count: usize,
    idx: usize,
}

impl ScriptedRtc {
    fn new() -> Self {
        Self {
            reads: [0; 16],
            count: 0,
            idx: 0,
        }
    }

    fn push(&mut self, v: u32) {
        if self.count < self.reads.len() {
            self.reads[self.count] = v;
            self.count += 1;
        }
    }

    fn next(&mut self) -> u32 {
        let v = if self.idx < self.count {
            self.reads[self.idx]
        } else {
            self.reads[self.count.saturating_sub(1)]
        };
        self.idx += 1;
        v
    }
}

impl RtcToyAccess for ScriptedRtc {
    fn write_ctrl(&mut self, _val: u32) {}

    fn read_toy_high(&mut self) -> u32 {
        self.next()
    }

    fn read_toy_low(&mut self) -> u32 {
        self.next()
    }
}

#[def_test]
fn test_cross_year_resamples() {
    // Simulate a read straddling new-year: year reads as 2026, the date
    // rolls into 2027-01-01, then year re-reads as 2027. The first triple
    // mismatches and is re-sampled; the second is consistent.
    let new_year_low = encode_toy_low(1, 1, 0, 0, 0, 0); // 2027-01-01 00:00:00
    let mut rtc = ScriptedRtc::new();
    // first (torn) triple: 2026, 2027-01-01, 2027 -> mismatch
    rtc.push(126);
    rtc.push(new_year_low);
    rtc.push(127);
    // second (consistent) triple: 2027, 2027-01-01, 2027 -> match
    rtc.push(127);
    rtc.push(new_year_low);
    rtc.push(127);

    let result = read_rtc_from(&mut rtc).expect("valid sample decodes");
    // Must accept 2027-01-01, not the torn 2026-01-01 the old code produced.
    assert_eq!(result, expected_system_time(127, 1, 1, 0, 0, 0));
    assert_ne!(result, expected_system_time(126, 1, 1, 0, 0, 0));
}

#[def_test]
fn test_same_year_read() {
    // A stable same-year triple is accepted on the first attempt.
    let low = encode_toy_low(6, 15, 12, 30, 45, 0); // 2026-06-15 12:30:45
    let mut rtc = ScriptedRtc::new();
    rtc.push(126);
    rtc.push(low);
    rtc.push(126);

    let result = read_rtc_from(&mut rtc).expect("stable sample decodes");
    assert_eq!(result, expected_system_time(126, 6, 15, 12, 30, 45));
}

#[def_test]
fn test_persistent_mismatch_returns_none() {
    // A year that keeps "rolling" across every sample is a persistent
    // anomaly, not a normal rollover (which resolves on the next sample).
    // No trustworthy sample exists, so after RTC_MAX_RETRIES attempts the
    // read must be rejected instead of publishing a torn value such as
    // year 2026 paired with a 2027-01-01 date.
    let low = encode_toy_low(1, 1, 0, 0, 0, 0);
    let mut rtc = ScriptedRtc::new();
    for _ in 0..RTC_MAX_RETRIES {
        rtc.push(126); // year1 = 2026
        rtc.push(low);
        rtc.push(127); // year_after = 2027 -> always mismatch
    }

    assert!(read_rtc_from(&mut rtc).is_none());
}

#[def_test]
fn test_invalid_date_returns_none() {
    // A faulting counter can return bit patterns wider than the valid
    // calendar ranges. None of these form a real timestamp, so each must
    // yield `None` instead of panicking inside the driver.
    for low in [
        encode_toy_low(0, 1, 0, 0, 0, 0),  // month 0
        encode_toy_low(1, 0, 0, 0, 0, 0),  // day 0
        encode_toy_low(1, 1, 24, 0, 0, 0), // hour 24
        encode_toy_low(2, 30, 0, 0, 0, 0), // Feb 30
    ] {
        let mut rtc = ScriptedRtc::new();
        rtc.push(126);
        rtc.push(low);
        rtc.push(126);
        assert!(read_rtc_from(&mut rtc).is_none());
    }
}

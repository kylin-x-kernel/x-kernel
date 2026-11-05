// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;

use alloc::string::ToString;

use log::{Level, LevelFilter};
use unittest::{assert_eq, def_test};

use super::{
    build_floor_from, build_level_floor, current_log_level, effective_ceiling, filter_enabled,
    filter_spec, level_name, level_number,
    logbuf_clear, logbuf_clear_through, logbuf_len, logbuf_read_newest, logbuf_read_newest_seq, parse_log_level, set_filter_spec, set_log_level,
    set_log_level_filter,
};

/// The canonical spec of the active filter, so a test restores the **whole**
/// state it found — default level *and* rule set. Restoring only the default
/// level with `set_log_level_filter` drops every per-target rule, which silently
/// leaks state into the tests that run after it.
struct SavedSpec {
    buf: [u8; super::MAX_FILTER_SPEC],
    len: usize,
}

impl SavedSpec {
    fn take() -> Self {
        let mut buf = [0u8; super::MAX_FILTER_SPEC];
        let len = filter_spec(&mut buf).unwrap_or(0);
        Self { buf, len }
    }

    fn restore(&self) {
        if let Ok(spec) = core::str::from_utf8(&self.buf[..self.len]) {
            set_log_level(spec);
        }
    }
}

/// Whole-ring snapshot, for assertions that must not depend on how much other
/// kernel threads logged in the meantime.
fn ring_snapshot() -> alloc::vec::Vec<u8> {
    let mut buf = alloc::vec![0u8; super::LOGBUF_SIZE];
    let len = logbuf_read_newest(&mut buf);
    buf.truncate(len);
    buf
}

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

/// `serial`: the level filter and the `log` ceiling are process-global, so a
/// test that asserts on them must not share them with a parallel one.
#[def_test(serial)]
fn set_log_level_takes_effect_immediately() {
    let saved = SavedSpec::take();
    set_log_level("3");
    assert_eq!(current_log_level(), LevelFilter::Info);
    set_log_level_filter(LevelFilter::Debug);
    assert_eq!(current_log_level(), LevelFilter::Debug);
    // Invalid input must not silence the whole kernel by accident.
    set_log_level("invalid");
    assert_eq!(current_log_level(), LevelFilter::Debug);
    saved.restore();
}

#[def_test(serial)]
fn filter_spec_reports_the_canonical_text() {
    let saved = SavedSpec::take();
    assert_eq!(set_filter_spec("warn drv_uart=debug"), Ok(()));
    // The default stays visible to anything that reports "the level".
    assert_eq!(current_log_level(), LevelFilter::Warn);

    let mut buf = [0u8; super::MAX_FILTER_SPEC];
    let len = filter_spec(&mut buf).expect("spec fits");
    assert_eq!(core::str::from_utf8(&buf[..len]), Ok("warn drv_uart=debug"));

    assert_eq!(set_filter_spec("warn drv_uart=trace drv_uart::dw_uart=error"), Ok(()));
    let mut buf = [0u8; super::MAX_FILTER_SPEC];
    let len = filter_spec(&mut buf).expect("spec fits");
    assert_eq!(
        core::str::from_utf8(&buf[..len]),
        Ok("warn drv_uart=trace drv_uart::dw_uart=error")
    );

    saved.restore();
}

/// The console decision itself: the longest matching prefix wins, and a target
/// without a rule falls back to the default level.
#[def_test(serial)]
fn filter_rules_match_the_longest_prefix() {
    let saved = SavedSpec::take();
    assert_eq!(
        set_filter_spec("error drv_uart=debug drv_uart::dw_uart=trace"),
        Ok(())
    );

    // The most specific rule decides for the nested module.
    assert!(filter_enabled("drv_uart::dw_uart", Level::Trace));
    assert!(filter_enabled("drv_uart::dw_uart", Level::Debug));
    assert!(filter_enabled("drv_uart::dw_uart", Level::Error));

    // Its sibling inherits the shorter rule, not the nested one.
    assert!(filter_enabled("drv_uart::other", Level::Debug));
    assert!(!filter_enabled("drv_uart::other", Level::Trace));

    // Everything else uses the default (`error`).
    assert!(filter_enabled("ksyscall", Level::Error));
    assert!(!filter_enabled("ksyscall", Level::Warn));
    assert!(!filter_enabled("spi", Level::Warn));

    // Matching is textual prefix, not path-segment: a longer module path that
    // merely starts with the rule's target is covered by it.
    assert!(filter_enabled("drv_uart_extra", Level::Debug));
    assert!(!filter_enabled("drv_uart_extra", Level::Trace));

    saved.restore();
}

/// A spec without a bare level keeps the default in force and replaces only the
/// rule set: `ksyscall=trace` must not silently reset the rest of the kernel to
/// a hard-coded `warn`.
#[def_test(serial)]
fn filter_spec_without_a_level_keeps_the_current_default() {
    let saved = SavedSpec::take();
    set_log_level_filter(LevelFilter::Debug);

    assert_eq!(set_filter_spec("ksyscall=trace"), Ok(()));
    assert_eq!(current_log_level(), LevelFilter::Debug);
    assert!(filter_enabled("drv_uart", Level::Debug));
    assert!(filter_enabled("ksyscall", Level::Trace));

    let mut buf = [0u8; super::MAX_FILTER_SPEC];
    let len = filter_spec(&mut buf).expect("spec fits");
    assert_eq!(
        core::str::from_utf8(&buf[..len]),
        Ok("debug ksyscall=trace")
    );

    // A bare level still replaces the default, and the rule set is replaced
    // wholesale rather than merged.
    assert_eq!(set_filter_spec("warn drv_uart=info"), Ok(()));
    assert_eq!(current_log_level(), LevelFilter::Warn);
    assert!(!filter_enabled("ksyscall", Level::Trace));

    saved.restore();
}

#[def_test(serial)]
fn filter_spec_rejects_bad_input_without_half_applying() {
    let saved = SavedSpec::take();
    assert_eq!(set_filter_spec("info subsys_uart=debug"), Ok(()));
    // A bad token anywhere must leave the previous spec intact.
    assert_eq!(set_filter_spec("debug drv_uart=nonsense").is_err(), true);
    assert_eq!(set_filter_spec("debug =4").is_err(), true);
    assert_eq!(set_filter_spec("debug too_long_target_name_aaaaaaaaaaaaaaaa=4").is_err(), true);
    assert_eq!(set_filter_spec("   ").is_err(), true);
    assert_eq!(current_log_level(), LevelFilter::Info);
    let mut buf = [0u8; super::MAX_FILTER_SPEC];
    let len = filter_spec(&mut buf).expect("spec fits");
    assert_eq!(
        core::str::from_utf8(&buf[..len]),
        Ok("info subsys_uart=debug")
    );
    saved.restore();
}

/// A record the console filter drops still lands in the ring: that is the
/// "quiet console, complete log" contract `dmesg` depends on, and it holds for
/// every build cap because capture is no longer derived from the console filter.
///
/// `serial`: the ring and the filter are process-global resources. The assertions
/// below are scoped to this test's own unique markers because other kernel
/// threads keep logging concurrently.
#[def_test(serial)]
fn logbuf_captures_records_the_console_filter_hides() {
    let saved = SavedSpec::take();
    // Console filter `off`: even `error!` is not printed. `error` is at or below
    // every build cap, so the ring still records it in any configuration.
    assert_eq!(set_filter_spec("off"), Ok(()));
    // Drop whatever earlier contexts buffered: the window below must be this
    // test's records.
    logbuf_clear();

    super::error!("klogger logbuf captured marker");

    // Search the whole ring: a fixed window could be filled by other kernel
    // threads between the record and the read.
    let snapshot = ring_snapshot();
    let text = core::str::from_utf8(&snapshot).expect("log text is utf-8");
    let line = text
        .lines()
        .find(|line| line.contains("klogger logbuf captured marker"))
        .expect("the record the console dropped is missing from the ring");
    // Each line carries X-Kernel's own level number, which is what makes
    // `dmesg | grep '^<1>'` a level filter.
    assert!(
        line.starts_with("<1>"),
        "the captured record lost its level: {text}"
    );
    // A whole-ring snapshot ends on a record boundary.
    assert!(text.ends_with('\n'), "record without newline: {text:?}");

    // Clearing drops this test's records; unrelated records may follow.
    logbuf_clear();
    let snapshot = ring_snapshot();
    let text = core::str::from_utf8(&snapshot).expect("log text is utf-8");
    assert!(
        !text.contains("klogger logbuf"),
        "clear left stale records: {text}"
    );

    saved.restore();
}

/// A read returns the newest whole records that fit, oldest first, starting at a
/// record boundary — Linux's `syslog_print_all` shape — and never consumes.
///
/// `serial` because the ring is process-global; the level used is `error`, which
/// is at or below every build cap, and the records are emitted while the console
/// filter is `off` so nothing goes through the UART.
#[def_test(serial)]
fn logbuf_read_returns_the_newest_whole_records() {
    let saved = SavedSpec::take();
    assert_eq!(set_filter_spec("off"), Ok(()));
    logbuf_clear();

    super::error!("klogger order marker one");
    super::error!("klogger order marker two");

    // Whole-ring window: independent of how much other contexts logged.
    let snapshot = ring_snapshot();
    let text = core::str::from_utf8(&snapshot).expect("log text is utf-8");
    let first = text.find("klogger order marker one").expect("first record");
    let second = text.find("klogger order marker two").expect("second record");
    assert!(first < second, "records are not in log order: {text}");
    assert!(text.ends_with('\n'), "snapshot does not end a record: {text:?}");

    // Reading does not consume; the markers are still there. Their *byte count*
    // is not compared with a later read: other threads keep logging into this
    // process-global ring, so only the content is stable.

    // A destination smaller than the ring gets the *newest* whole records, and
    // never a fragment: either at least one whole record, starting at a record
    // boundary, or nothing at all.
    let mut small = [0u8; 96];
    let small_len = logbuf_read_newest(&mut small);
    assert!(small_len <= small.len());
    let tail = core::str::from_utf8(&small[..small_len]).expect("log text is utf-8");
    assert!(
        small_len == 0 || tail.starts_with('<'),
        "the window does not start at a record boundary: {tail:?}"
    );

    // A destination smaller than the newest record returns nothing rather than
    // half a record.
    let mut tiny = [0u8; 4];
    assert_eq!(logbuf_read_newest(&mut tiny), 0);

    logbuf_clear();
    saved.restore();
}

/// The read window never starts inside a record, even when the record body
/// itself contains a newline (a message may carry one, and `/dev/log` forwards a
/// user datagram verbatim), so a line-scanner would cut in the wrong place.
#[def_test(serial)]
fn logbuf_window_starts_at_a_record_boundary_despite_embedded_newlines() {
    let saved = SavedSpec::take();
    assert_eq!(set_filter_spec("off"), Ok(()));
    logbuf_clear();

    super::error!("first line of a multi-line record");
    super::error!("second line\nthird line of a multi-line record");

    // Every destination size either yields whole records or nothing. (No
    // comparison against a snapshot taken earlier: other kernel threads keep
    // logging, so the ring's tail moves between the two reads; the property under
    // test is the window *shape*.)
    let _ = ring_snapshot();
    for size in [1usize, 8, 16, 32, 64, 96, 128, 4096] {
        let mut buf = alloc::vec![0u8; size];
        let len = logbuf_read_newest(&mut buf);
        assert!(len <= size);
        if len == 0 {
            continue;
        }
        let text = core::str::from_utf8(&buf[..len]).expect("log text is utf-8");
        assert!(
            text.starts_with('<'),
            "a {size}-byte window started mid-record: {text:?}"
        );
        // A window is whole records: it starts with a record's level tag and
        // ends with the terminator of the last record it includes.
        assert!(
            text.ends_with('\n'),
            "a {size}-byte window ends inside a record: {text:?}"
        );
    }

    logbuf_clear();
    saved.restore();
}

/// A ring that wrapped keeps serving whole records: a record the wrap cut into is
/// dropped rather than returned in part.
#[def_test(serial)]
fn logbuf_serves_whole_records_after_a_wrap() {
    let saved = SavedSpec::take();
    assert_eq!(set_filter_spec("off"), Ok(()));
    logbuf_clear();

    // Wrap the ring several times; records are ~80 bytes, so this is well past
    // 64 KiB.
    for index in 0..1200 {
        super::error!("klogger wrap marker {index}");
    }

    let snapshot = ring_snapshot();
    let text = core::str::from_utf8(&snapshot).expect("log text is utf-8");
    assert!(!text.is_empty());
    assert!(
        text.starts_with('<'),
        "the window after a wrap starts mid-record: {text:?}"
    );
    assert!(text.ends_with('\n'), "the snapshot ends mid-record");

    logbuf_clear();
    saved.restore();
}

/// A `READ_CLEAR` drops exactly the records a read returned and leaves later
/// ones buffered, so records logged while the reader was copying are not lost.
#[def_test(serial)]
fn logbuf_clear_through_keeps_records_above_the_watermark() {
    let saved = SavedSpec::take();
    assert_eq!(set_filter_spec("off"), Ok(()));
    logbuf_clear();

    super::error!("klogger watermark before");
    let mut buf = alloc::vec![0u8; super::LOGBUF_SIZE];
    let (len, watermark) = logbuf_read_newest_seq(&mut buf);
    assert!(len > 0);
    let text = core::str::from_utf8(&buf[..len]).expect("log text is utf-8");
    assert!(text.contains("klogger watermark before"));
    assert!(watermark > 0, "a read must report its clear watermark");

    super::error!("klogger watermark after");
    logbuf_clear_through(watermark);

    let snapshot = ring_snapshot();
    let text = core::str::from_utf8(&snapshot).expect("log text is utf-8");
    assert!(
        !text.contains("klogger watermark before"),
        "the cleared record came back: {text}"
    );
    assert!(
        text.contains("klogger watermark after"),
        "a record logged after the read was cleared: {text}"
    );

    logbuf_clear();
    saved.restore();
}

/// A clear is a watermark, not an erasure: the bodies stay in the ring, so a
/// record pushed after a clear must not be mixed with stale bytes of a longer
/// record that occupied the same place.
#[def_test(serial)]
fn logbuf_clear_hides_bodies_without_erasing_them() {
    let saved = SavedSpec::take();
    assert_eq!(set_filter_spec("off"), Ok(()));
    logbuf_clear();

    super::error!("klogger pre-clear record with a long body that fills the slot");
    logbuf_clear();
    super::error!("klogger post-clear x");

    let snapshot = ring_snapshot();
    let text = core::str::from_utf8(&snapshot).expect("log text is utf-8");
    assert!(
        !text.contains("pre-clear record"),
        "a cleared record came back: {text}"
    );
    let line = text
        .lines()
        .find(|line| line.contains("klogger post-clear x"))
        .expect("the record pushed after the clear is missing");
    assert!(
        line.ends_with("post-clear x"),
        "a shorter record kept bytes of a longer one: {line:?}"
    );

    logbuf_clear();
    saved.restore();
}

/// The reported length tracks the ring: it grows with the records, saturates at
/// the ring size instead of overrunning it, and returns to 0 on a clear.
///
/// The length is a snapshot, and other kernel threads keep logging regardless of
/// the console filter, so this asserts the bound rather than an exact equality
/// with a second read.
#[def_test(serial)]
fn logbuf_length_tracks_the_ring_and_saturates() {
    let saved = SavedSpec::take();
    assert_eq!(set_filter_spec("off"), Ok(()));
    logbuf_clear();

    super::error!("klogger length marker one");
    let after_one = logbuf_len();
    assert!(after_one > 0, "a captured record reported no bytes");

    for index in 0..1500 {
        super::error!("klogger length marker fill {index}");
        let len = logbuf_len();
        assert!(len <= super::LOGBUF_SIZE, "the ring overran its budget: {len}");
    }
    assert!(
        logbuf_len() > super::LOGBUF_SIZE - 1024,
        "a wrapped ring reports far less than it holds"
    );

    // A clear is a watermark, so a record another thread publishes right after it
    // is visible again; "the marker is gone" is the property, not "length is 0".
    super::error!("klogger length marker after-clear");
    logbuf_clear();
    let snapshot = ring_snapshot();
    let text = core::str::from_utf8(&snapshot).expect("log text is utf-8");
    assert!(
        !text.contains("klogger length marker fill"),
        "the clear left records behind: {}",
        &text[..text.len().min(200)]
    );
    saved.restore();
}

/// The build level is the logger's floor, so it must never be `off`; a
/// configuration that selects nothing falls back to `error` rather than silence.
#[def_test]
fn build_level_is_a_usable_floor() {
    assert!(
        build_level_floor() != log::LevelFilter::Off,
        "an unset LOG_LEVEL_* must fall back to a level, not silence the kernel"
    );
    // The fallback itself, which this image's configuration never exercises.
    assert_eq!(build_floor_from("off"), log::LevelFilter::Error);
    assert_eq!(build_floor_from("nonsense"), log::LevelFilter::Error);
    assert_eq!(build_floor_from(""), log::LevelFilter::Error);
    assert_eq!(build_floor_from("debug"), log::LevelFilter::Debug);
    assert_eq!(build_floor_from("3"), log::LevelFilter::Info);
}

/// The console filter may raise the ceiling above the build floor (that is how
/// `make run LOG=debug` reveals the `debug!` sites of a `warn` image) and may
/// lower it, but the floor always wins so `quiet` cannot starve the ring.
#[def_test(serial)]
fn console_filter_moves_the_ceiling_but_not_below_the_build_floor() {
    let saved = SavedSpec::take();
    let floor = build_level_floor();

    // Raising: a level above the floor is admitted.
    assert_eq!(set_filter_spec("trace"), Ok(()));
    assert_eq!(effective_ceiling(), log::LevelFilter::Trace);

    // Lowering: the floor keeps the logger where it was.
    assert_eq!(set_filter_spec("off"), Ok(()));
    assert_eq!(
        effective_ceiling(),
        floor,
        "quietening the console must not take the logger below the build level"
    );

    // A rule above the floor raises the ceiling for the whole logger (all
    // levels up to it are formatted and captured; the console filter still
    // decides what is printed).
    assert_eq!(set_filter_spec("off ksyscall=trace"), Ok(()));
    assert_eq!(effective_ceiling(), log::LevelFilter::Trace);

    saved.restore();
}

/// A record longer than the line buffer keeps its terminating newline and never
/// ends in the middle of a code point, so `dmesg` still sees one line per
/// record.
#[def_test(serial)]
fn logbuf_terminates_truncated_records() {
    use alloc::string::String;

    let saved = SavedSpec::take();
    // `off` keeps the console quiet; `error` is at or below every build cap, so
    // the ring records these in any configuration.
    assert_eq!(set_filter_spec("off"), Ok(()));
    logbuf_clear();

    // Four times the 256-byte line buffer, so both records are truncated.
    let long = String::from("0123456789abcdef").repeat(16);
    super::error!("marker-one {long}");
    super::error!("marker-two {long}");
    // Truncation must not split a three-byte code point.
    super::error!("{}", String::from("\u{5b57}").repeat(120));

    let snapshot = ring_snapshot();
    let text = core::str::from_utf8(&snapshot).expect("log text is utf-8");
    let marker_lines: alloc::vec::Vec<&str> = text
        .lines()
        .filter(|line| line.contains("marker-one") || line.contains("marker-two"))
        .collect();
    assert_eq!(marker_lines.len(), 2, "records ran together: {text}");
    for line in marker_lines {
        assert!(line.len() < 256, "record over the line budget: {line:?}");
    }
    assert!(text.ends_with('\n'), "record without terminator: {text:?}");

    saved.restore();
}

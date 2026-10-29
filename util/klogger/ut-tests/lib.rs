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
    parse_log_level, set_filter_spec, set_log_level,
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


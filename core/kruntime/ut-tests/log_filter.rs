// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Boot log-filter resolution: the command line wins over the build level, and a
//! rejected token does not hide a later valid one.

use klogger::{
    current_log_level, filter_spec, level_name, parse_log_level, set_filter_spec, MAX_FILTER_SPEC,
};
use unittest::{assert, assert_eq, def_test};

/// Canonical text of the active filter, as `/proc/sys/kernel/log_filter` reports it.
fn active_spec() -> alloc::string::String {
    let mut buf = [0u8; MAX_FILTER_SPEC];
    let len = filter_spec(&mut buf).expect("the active spec fits");
    alloc::string::String::from_utf8_lossy(&buf[..len]).into_owned()
}

/// Restores the filter that was active when the test started, rules included.
///
/// Returns whether the saved spec still applies; the caller asserts it, because
/// the test macros in this framework expand to `return TestResult::Failed` and
/// therefore only work inside a `#[def_test]` body.
fn restore(spec: &str) -> bool {
    set_filter_spec(spec).is_ok()
}

/// An explicit `loglevel=` replaces the build level, including a level the build
/// could not have produced on its own.
#[def_test(serial)]
fn command_line_level_overrides_the_build_level() {
    let saved = active_spec();
    let build_level = super::apply_build_log_level();

    let resolved = super::resolve_log_level(Some("console=ttyS0 loglevel=debug"));
    assert_eq!(resolved, "debug");
    assert_eq!(
        current_log_level(),
        parse_log_level("debug").expect("debug is valid")
    );

    // `quiet`/`debug` are the `loglevel=2`/`loglevel=4` shorthands.
    let resolved = super::resolve_log_level(Some("quiet"));
    assert_eq!(resolved, "2");
    assert_eq!(
        current_log_level(),
        parse_log_level("warn").expect("warn is valid")
    );
    let resolved = super::resolve_log_level(Some("debug"));
    assert_eq!(resolved, "4");

    // Without a request the build level is what stays in force.
    let resolved = super::resolve_log_level(None);
    assert_eq!(resolved, build_level);

    assert!(restore(&saved), "the saved spec failed to re-apply");
}

/// A rejected `loglevel=` is skipped and the scan continues, so a later valid one
/// on the same command line still applies.
#[def_test(serial)]
fn an_invalid_level_does_not_hide_a_later_one() {
    let saved = active_spec();

    let resolved = super::resolve_log_level(Some("loglevel=verbos loglevel=info"));
    assert_eq!(resolved, "info");
    assert_eq!(
        current_log_level(),
        parse_log_level("info").expect("info is valid")
    );

    // A command line with only invalid values falls back to the build level.
    let build_level = super::apply_build_log_level();
    let resolved = super::resolve_log_level(Some("loglevel=6 loglevel="));
    assert_eq!(resolved, build_level);

    assert!(restore(&saved), "the saved spec failed to re-apply");
}

/// The spec form is accepted too, so a per-target override can come from the
/// command line in its comma form.
#[def_test(serial)]
fn command_line_accepts_a_full_spec() {
    let saved = active_spec();

    let resolved = super::resolve_log_level(Some("loglevel=warn,ksyscall=debug"));
    assert_eq!(resolved, "warn,ksyscall=debug");
    assert_eq!(active_spec(), "warn ksyscall=debug");
    assert_eq!(
        current_log_level(),
        parse_log_level("warn").expect("warn is valid")
    );
    assert_eq!(level_name(current_log_level()), "warn");

    assert!(restore(&saved), "the saved spec failed to re-apply");
}

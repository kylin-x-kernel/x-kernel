// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::normalized_eq;

/// Identical ASCII strings must compare equal.
#[test]
fn identical_strings_equal() {
    assert!(normalized_eq(b"hello", b"hello"));
}

/// Case is folded to lowercase.
#[test]
fn case_folding() {
    assert!(normalized_eq(b"Hello", b"hello"));
    assert!(normalized_eq(b"HELLO WORLD", b"hello world"));
}

/// Leading spaces are stripped.
#[test]
fn leading_spaces_stripped() {
    assert!(normalized_eq(b"  hello", b"hello"));
}

/// Trailing spaces are stripped.
///
/// Regression test: `NormalizedIter` must not emit a trailing space for
/// input that ends with a space sequence.
#[test]
fn trailing_spaces_stripped() {
    assert!(normalized_eq(b"hello  ", b"hello"));
    assert!(normalized_eq(b"hello ", b"hello"));
}

/// Multiple consecutive internal spaces are collapsed to a single space.
///
/// Regression test for the double-space bug: `pending_space` must not
/// cause two spaces to be emitted for a single space in the input.
#[test]
fn internal_spaces_collapsed() {
    assert!(normalized_eq(b"hello  world", b"hello world"));
    assert!(normalized_eq(b"hello   world", b"hello world"));
}

/// Combined: leading + trailing + internal spaces, case folding.
#[test]
fn combined_normalization() {
    assert!(normalized_eq(b"  Hello   World  ", b"hello world"));
}

/// Empty string and all-spaces string must both yield zero bytes.
#[test]
fn empty_and_whitespace_only() {
    assert!(normalized_eq(b"", b""));
    assert!(normalized_eq(b"   ", b""));
    assert!(normalized_eq(b"   ", b"   "));
}

/// Different strings must NOT compare equal after normalization.
#[test]
fn different_strings_not_equal() {
    assert!(!normalized_eq(b"hello", b"world"));
    assert!(!normalized_eq(b"ab", b"abc"));
}

/// `NormalizedIter`: input ending with an internal space sequence followed by
/// trailing spaces must emit the space and then stop (no double space, no
/// trailing space).
#[test]
fn internal_then_trailing_space_no_trailing_emit() {
    assert!(
        normalized_eq(b"ab  ", b"ab"),
        "trailing spaces must not be emitted"
    );
    assert!(
        normalized_eq(b"ab  cd  ", b"ab cd"),
        "internal double-space collapses; trailing spaces stripped"
    );
}

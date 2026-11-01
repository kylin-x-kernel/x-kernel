// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn plain_decimals_parse() {
    assert_eq!(to_number("0"), Some(0));
    assert_eq!(to_number("7"), Some(7));
    assert_eq!(to_number("12345"), Some(12345));
    // Values from 4294967280 up are rejected exactly as `name_to_int()` does,
    // even though they would still fit in a `u32`.
    assert_eq!(to_number("4294967279"), Some(4294967279));
    assert_eq!(to_number("4294967280"), None);
}

#[def_test]
fn lax_spellings_are_rejected() {
    // `str::parse` would accept all of these; `name_to_int()` accepts none.
    for rejected in [
        "", "+0", "+7", "-1", "007", "01", " 7", "7 ", "0x10", "1a", "1_0", "١٢",
    ] {
        assert_eq!(to_number(rejected), None, "accepted {rejected:?}");
    }
}

#[def_test]
fn values_that_do_not_fit_are_rejected() {
    assert_eq!(to_number("4294967296"), None);
    assert_eq!(to_number("99999999999999999999"), None);
}

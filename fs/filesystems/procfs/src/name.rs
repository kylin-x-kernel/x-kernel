// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Numeric names in `/proc`, parsed the way Linux parses them.
//!
//! Several `/proc` directories take a bare number as an entry name -- the task
//! id under `/proc/<pid>`, the descriptor under `/proc/<pid>/fd` and
//! `/proc/<pid>/fdinfo`. Linux compares those names against the numbers it
//! enumerates, using `name_to_int()`, which accepts *only* a plain decimal:
//! a leading `+`, a leading zero on a multi-digit name, a sign or any
//! non-digit is not a number there. `str::parse` is laxer than that (it takes
//! `+0` and `007`), and a laxer lookup would let `/proc/self/fdinfo/+0` open a
//! file that `ls /proc/self/fdinfo` never lists.

/// Parses a `/proc` entry name as a non-negative decimal number.
///
/// Returns `None` for anything `name_to_int()` would reject: an empty name, a
/// leading zero on a name of more than one digit, a sign, a non-digit, or a
/// value that does not fit in `u32`.
pub(crate) fn to_number(name: &str) -> Option<u32> {
    let bytes = name.as_bytes();
    // "0" is a number; "00", "01" and "0x0" are not.
    if bytes.is_empty() || (bytes.len() > 1 && bytes[0] == b'0') {
        return None;
    }
    // Same guard Linux uses (`fs/proc/util.c`): values from
    // `(u32::MAX - 9) / 10` up cannot take another digit there and are
    // rejected, which keeps this byte-for-byte equal to `name_to_int()`.
    let overflow_guard = (u32::MAX - 9) / 10;
    let mut value: u32 = 0;
    for &byte in bytes {
        let digit = byte.checked_sub(b'0')?;
        if digit > 9 || value >= overflow_guard {
            return None;
        }
        value = value * 10 + u32::from(digit);
    }
    Some(value)
}

unittest::path_mod_test!(name_tests, "../ut-tests/name.rs");

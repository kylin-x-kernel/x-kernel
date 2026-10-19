// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, assert_ne, def_test};

use super::*;

#[def_test]
fn test_jitter_available_when_feature_compiled() {
    assert!(is_available());
}

#[def_test]
fn test_jitter_read_zero_len() {
    assert!(read(0).is_none());
}

#[def_test]
fn test_jitter_collect_nonzero() {
    if is_available() {
        let mut buf = [0u8; 32];
        let written = collect_into(&mut buf);
        assert_ne!(written, 0);
        assert_ne!(buf, [0u8; 32]);
    }
}

#[def_test]
fn test_jitter_read_returns_requested_len() {
    if is_available() {
        let data = read(48).expect("jitter should produce samples");
        assert_eq!(data.len(), 48);
        assert!(data.iter().any(|&b| b != 0));
    } else {
        assert!(read(16).is_none());
    }
}

#[def_test]
fn test_jitter_two_reads_differ() {
    if is_available() {
        let a = read(32).expect("first jitter read");
        let b = read(32).expect("second jitter read");
        assert_ne!(a.as_slice(), b.as_slice());
    }
}

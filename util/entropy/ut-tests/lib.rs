// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, assert_ne, def_test};

use super::*;

#[def_test]
fn test_fill_random_without_init() {
    let mut buf = [0u8; 32];
    fill_random(&mut buf);
    assert_ne!(buf, [0u8; 32]);
}

#[def_test]
fn test_fill_random_empty_is_noop() {
    let mut buf = [];
    fill_random(&mut buf);
    assert_eq!(buf.len(), 0);
}

#[def_test]
fn test_fill_random_various_lengths() {
    init();
    for len in [1usize, 7, 32, 64, 256, 1024] {
        let mut buf = alloc::vec![0u8; len];
        fill_random(&mut buf);
        assert_eq!(buf.len(), len);
        // A single uniform random byte is zero with probability 1/256, so
        // only assert non-degenerate output for multi-byte fills.
        if len > 1 {
            assert!(buf.iter().any(|&b| b != 0), "all-zero output for len {len}");
        }
    }
}

#[def_test]
fn test_add_entropy_empty_is_noop() {
    // Empty input must return immediately without mixing or panicking.
    add_entropy(&[]);
    let mut buf = [0u8; 16];
    fill_random(&mut buf);
    assert_ne!(buf, [0u8; 16]);
}

#[def_test]
fn test_add_entropy_changes_output() {
    init();
    let mut before = [0u8; 32];
    let mut after = [0u8; 32];
    fill_random(&mut before);
    add_entropy(b"extra-entropy-input");
    fill_random(&mut after);
    assert_ne!(before, after);
}

#[def_test]
fn test_add_entropy_does_not_mark_ready() {
    // Capture readiness before an untrusted mix. Userspace writes must not
    // be what establishes CRNG readiness (Linux write_pool semantics).
    let ready_before = is_ready();
    add_entropy(b"attacker-controlled-known-bytes");
    if !ready_before {
        assert!(!is_ready());
    }
    // Mixing still affects subsequent output when the pool is already live.
    let mut before = [0u8; 32];
    let mut after = [0u8; 32];
    fill_random(&mut before);
    add_entropy(b"more-untrusted-bytes");
    fill_random(&mut after);
    assert_ne!(before, after);
}

#[def_test]
fn test_wait_until_ready_returns_when_ready() {
    init();
    try_seed_from_hardware();
    // Default qemu defconfig enables jitter and/or HW sources.
    assert!(is_ready());
    wait_until_ready();
}

#[def_test]
fn test_two_reads_differ() {
    init();
    let mut buf1 = [0u8; 32];
    let mut buf2 = [0u8; 32];
    fill_random(&mut buf1);
    fill_random(&mut buf2);
    assert_ne!(buf1, buf2);
}

#[def_test]
fn test_init_is_idempotent() {
    init();
    let ready_before = is_ready();
    let hw_before = is_hardware_ready();
    init();
    init();
    assert_eq!(is_ready(), ready_before);
    assert_eq!(is_hardware_ready(), hw_before);
}

#[def_test]
fn test_try_seed_from_hardware_is_safe() {
    init();
    try_seed_from_hardware();
    try_seed_from_hardware();
    let mut buf = [0u8; 16];
    fill_random(&mut buf);
    assert_ne!(buf, [0u8; 16]);
}

#[def_test]
fn test_format_source_list() {
    assert_eq!(format_source_list(&[]), "");
    assert_eq!(format_source_list(&["cpu-rng"]), "cpu-rng");
    assert_eq!(
        format_source_list(&["cpu-rng", "jitter"]),
        "cpu-rng, jitter"
    );
}

#[def_test]
fn test_hardware_presence_matches_sources() {
    init();
    // Source eligibility is the discovery side of hardware readiness.
    let present = source::any_reseed_source_available();
    if is_hardware_ready() {
        assert!(present);
    }
}

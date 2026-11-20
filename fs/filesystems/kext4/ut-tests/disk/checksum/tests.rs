// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::{bitmap_checksum, crc32c};

#[def_test]
fn linux_crc32c_golden_vector() {
    assert_eq!(crc32c(u32::MAX, b"123456789"), 0x1cf9_6d7c);
}

#[def_test]
fn bitmap_checksum_matches_linux_seeded_bitmap_crc() {
    let seed = 0x1122_3344;
    let bitmap = [0b1010_0101, 0x5a, 0xff, 0x00];

    assert_eq!(bitmap_checksum(&bitmap, seed), crc32c(seed, &bitmap));
}

/// Bit-at-a-time reference implementation, kept independent of the `crc`
/// crate used by the production path.
fn reference_crc32c(mut crc: u32, bytes: &[u8]) -> u32 {
    const POLYNOMIAL: u32 = 0x82f6_3b78;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (POLYNOMIAL & mask);
        }
    }
    crc
}

#[def_test]
fn crc32c_matches_bitwise_reference_across_lengths_and_seeds() {
    let mut data = [0u8; 1031];
    let mut state = 0x9e37_79b9u32;
    for byte in &mut data {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *byte = (state >> 24) as u8;
    }
    for len in [0usize, 1, 2, 3, 7, 8, 9, 16, 31, 255, 256, 511, 1023, 1031] {
        for seed in [0u32, 1, u32::MAX, 0x1122_3344] {
            assert_eq!(
                crc32c(seed, &data[..len]),
                reference_crc32c(seed, &data[..len]),
                "length {len} seed {seed:#x}"
            );
        }
    }
}

#[def_test]
fn crc32c_chained_updates_match_one_shot() {
    let data = b"the quick brown fox jumps over the lazy dog";
    for split in 0..=data.len() {
        let seed = 0x1122_3344;
        let chained = crc32c(crc32c(seed, &data[..split]), &data[split..]);
        assert_eq!(chained, crc32c(seed, data), "split {split}");
    }
}

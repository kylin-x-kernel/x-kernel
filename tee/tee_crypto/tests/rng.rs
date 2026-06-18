// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use rand_core::Rng;
use tee_crypto::rng::DeterministicRng;

#[test]
fn test_zero_seed_deterministic() {
    let mut rng1 = DeterministicRng::seed_from_u64(0);
    let mut rng2 = DeterministicRng::seed_from_u64(0);
    let mut buf1 = [0u8; 32];
    let mut buf2 = [0u8; 32];
    Rng::fill_bytes(&mut rng1, &mut buf1);
    Rng::fill_bytes(&mut rng2, &mut buf2);
    assert_eq!(buf1, buf2);
}

#[test]
fn test_seed_from_u64_deterministic() {
    let mut rng1 = DeterministicRng::seed_from_u64(42);
    let mut rng2 = DeterministicRng::seed_from_u64(42);
    let mut buf1 = [0u8; 16];
    let mut buf2 = [0u8; 16];
    Rng::fill_bytes(&mut rng1, &mut buf1);
    Rng::fill_bytes(&mut rng2, &mut buf2);
    assert_eq!(buf1, buf2);
}

#[test]
fn test_seed_from_bytes_deterministic() {
    let seed = [0xABu8; 32];
    let mut rng1 = DeterministicRng::seed_from_bytes(&seed);
    let mut rng2 = DeterministicRng::seed_from_bytes(&seed);
    let mut buf1 = [0u8; 64];
    let mut buf2 = [0u8; 64];
    Rng::fill_bytes(&mut rng1, &mut buf1);
    Rng::fill_bytes(&mut rng2, &mut buf2);
    assert_eq!(buf1, buf2);
}

#[test]
fn test_different_seeds_produce_different_output() {
    let mut rng1 = DeterministicRng::seed_from_u64(1);
    let mut rng2 = DeterministicRng::seed_from_u64(2);
    let mut buf1 = [0u8; 32];
    let mut buf2 = [0u8; 32];
    Rng::fill_bytes(&mut rng1, &mut buf1);
    Rng::fill_bytes(&mut rng2, &mut buf2);
    assert_ne!(buf1, buf2);
}

#[test]
fn test_rng_core_next_u32() {
    let mut rng = DeterministicRng::seed_from_u64(99);
    let a = rng.next_u32();
    let b = rng.next_u32();
    // Deterministic: same seed always yields same sequence
    let mut rng2 = DeterministicRng::seed_from_u64(99);
    assert_eq!(a, rng2.next_u32());
    assert_eq!(b, rng2.next_u32());
}

#[test]
fn test_rng_core_next_u64() {
    let mut rng = DeterministicRng::seed_from_u64(100);
    let a = rng.next_u64();
    let b = rng.next_u64();
    let mut rng2 = DeterministicRng::seed_from_u64(100);
    assert_eq!(a, rng2.next_u64());
    assert_eq!(b, rng2.next_u64());
}

#[test]
fn test_rand_core_crypto_rng_trait() {
    let mut rng = DeterministicRng::seed_from_u64(7);
    let mut buf = [0u8; 16];
    Rng::fill_bytes(&mut rng, &mut buf);
    // Should not be all zeros (astronomically unlikely)
    assert_ne!(buf, [0u8; 16]);
}

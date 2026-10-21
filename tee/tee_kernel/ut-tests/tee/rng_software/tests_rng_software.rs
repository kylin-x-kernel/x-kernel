// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::assert_ne;

use super::*;

#[unittest::def_test]
fn test_get_rand() {
    let mut buf1 = [0u8; 10];
    let mut buf2 = [0u8; 10];
    tee_software_get_rand(&mut buf1);
    tee_software_get_rand(&mut buf2);
    assert_ne!(buf1, buf2);
}

#[unittest::def_test]
fn test_tee_software_rng() {
    let mut rng = TeeSoftwareRng::new();
    let mut buf = [0u8; 10];
    rng.rng.fill_bytes(&mut buf);
    assert_ne!(buf, [0u8; 10]);
}

#[unittest::def_test]
fn test_tee_software_rng_crypto_rng() {
    let mut rng = TeeSoftwareRng::new();
    let mut buf = [0u8; 10];
    Rng::fill_bytes(&mut rng, &mut buf);
    assert_ne!(buf, [0u8; 10]);
}

#[unittest::def_test]
fn test_tee_software_rng_instances_use_distinct_seeds() {
    let mut rng1 = TeeSoftwareRng::new();
    let mut rng2 = TeeSoftwareRng::new();
    let mut buf1 = [0u8; 32];
    let mut buf2 = [0u8; 32];
    rng1.rng.fill_bytes(&mut buf1);
    rng2.rng.fill_bytes(&mut buf2);
    assert_ne!(buf1, buf2);
}

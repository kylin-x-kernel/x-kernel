// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use xdice::mbedtls_sm2dsa::*;

#[test]
fn test_sm2_sign_verify_roundtrip() {
    let seed = [0x42u8; 32];
    let mut public_key = [0u8; 64];
    let mut private_key = [0u8; 32];

    sm2_keypair_from_seed(&mut public_key, &mut private_key, &seed)
        .expect("keypair generation failed");

    let message = b"hello sm2 sign verify test";
    let signature = sm2_sign(message, &private_key).expect("sign failed");
    assert_eq!(signature.len(), 64);

    sm2_verify(message, &signature, &public_key).expect("verify failed");
}

#[test]
fn test_dice_hash() {
    let input = b"test hash data";
    let mut output = [0u8; 32];
    dice_hash(&mut 0, input, &mut output).expect("hash failed");
    assert!(output.iter().any(|&b| b != 0));
}

#[test]
fn test_dice_kdf() {
    let ikm = [0x01u8; 32];
    let salt = [0x02u8; 16];
    let info = b"kdf info";
    let mut output = [0u8; 64];
    dice_kdf(&ikm, Some(&salt), Some(info), &mut output).expect("kdf failed");
    assert!(output.iter().any(|&b| b != 0));
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq};

use super::*;
use crate::tee::utils::slice_fmt;

#[unittest::def_test]
fn test_crypto_cipher_encrypt() {
    let algo = TEE_ALG_AES_ECB_NOPAD;
    let key = [
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
        0x0F, 0x10,
    ];
    let plain = [
        0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E,
        0x1F, 0x20,
    ];
    let mut cipher = [0u8; 16];
    let result = crypto_cipher_ecb_nopad(
        algo,
        TEE_OperationMode::TEE_MODE_ENCRYPT,
        &key,
        &plain,
        &mut cipher,
    );
    tee_debug!(
        "test_crypto_cipher_encrypt: key: {:?}, plain: {:?}, cipher: {:?}",
        slice_fmt(&key),
        slice_fmt(&plain),
        slice_fmt(&cipher)
    );
    assert!(result.is_ok());
    assert_eq!(
        "D721A0F194231822F398706DD1FFF2B7",
        hex::encode_upper(cipher)
    );

    let mut decrypted = [0u8; 16];
    let result = crypto_cipher_ecb_nopad(
        algo,
        TEE_OperationMode::TEE_MODE_DECRYPT,
        &key,
        &cipher,
        &mut decrypted,
    );
    assert!(result.is_ok());
    assert_eq!(plain, decrypted);

    let algo = TEE_ALG_SM4_ECB_NOPAD;
    let result = crypto_cipher_ecb_nopad(
        algo,
        TEE_OperationMode::TEE_MODE_ENCRYPT,
        &key,
        &plain,
        &mut cipher,
    );
    tee_debug!(
        "test_crypto_cipher_encrypt: key: {:?}, plain: {:?}, cipher: {:?}",
        slice_fmt(&key),
        slice_fmt(&plain),
        slice_fmt(&cipher)
    );
    assert!(result.is_ok());
    assert_eq!(
        "4329A6241E39AD7A9A404A814A7EDD32",
        hex::encode_upper(cipher)
    );

    let result = crypto_cipher_ecb_nopad(
        algo,
        TEE_OperationMode::TEE_MODE_DECRYPT,
        &key,
        &cipher,
        &mut decrypted,
    );
    assert!(result.is_ok());
    assert_eq!(plain, decrypted);
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::assert;

use super::*;

#[unittest::def_test]
fn test_crypto_ecc_keypair_ops_generate() {
    let mut keypair = EccKeypair {
        curve: TEE_ECC_CURVE_SM2,
        ..Default::default()
    };
    let key_size = 256;
    let result = EccKeypairOpsCtx::<Sm2DsaKeyPair>::new(&mut keypair).generate(key_size);
    info!("Generated ECC key: {:X?}", result);
    assert!(result.is_ok());

    let d = keypair.d.to_bytes().expect("d to_bytes");
    let x = keypair.x.to_bytes().expect("x to_bytes");
    let y = keypair.y.to_bytes().expect("y to_bytes");
    assert!(!d.is_empty());
    assert!(!x.is_empty());
    assert!(!y.is_empty());
}

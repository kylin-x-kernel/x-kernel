// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::assert;

use super::*;

#[unittest::def_test]
fn ecc_dispatch_preserves_generation_and_errors() {
    use tee_raw_sys::{
        TEE_ERROR_NOT_IMPLEMENTED, TEE_TYPE_ECDH_KEYPAIR, TEE_TYPE_ECDSA_KEYPAIR,
        TEE_TYPE_SM2_DSA_KEYPAIR, TEE_TYPE_SM2_KEP_KEYPAIR, TEE_TYPE_SM2_PKE_KEYPAIR,
    };

    use crate::tee::crypto::crypto::crypto_acipher_gen_ecc_key;

    for object_type in [
        TEE_TYPE_ECDSA_KEYPAIR,
        TEE_TYPE_ECDH_KEYPAIR,
        TEE_TYPE_SM2_PKE_KEYPAIR,
        TEE_TYPE_SM2_DSA_KEYPAIR,
        TEE_TYPE_SM2_KEP_KEYPAIR,
    ] {
        let mut keypair = EccKeypair {
            curve: TEE_ECC_CURVE_SM2,
            ..Default::default()
        };
        assert_eq!(
            crypto_acipher_gen_ecc_key(&mut keypair, 255, object_type),
            Err(TEE_ERROR_BAD_PARAMETERS)
        );
        crypto_acipher_gen_ecc_key(&mut keypair, 256, object_type).unwrap();
        assert!(!keypair.d.to_bytes().unwrap().is_empty());
        assert!(!keypair.x.to_bytes().unwrap().is_empty());
        assert!(!keypair.y.to_bytes().unwrap().is_empty());
    }
    let mut keypair = EccKeypair::default();
    assert_eq!(
        crypto_acipher_gen_ecc_key(&mut keypair, 256, u32::MAX),
        Err(TEE_ERROR_NOT_IMPLEMENTED)
    );
}

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

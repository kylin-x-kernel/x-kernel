// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

mod common;

use tee_crypto::{
    bytes::{BigEndianBytes, PublicBytes, SecretBytes},
    tee_ops::ecc::{EccCurve, ecc_keygen_bytes, ecc_public_from_private_bytes},
};

#[test]
fn test_semantic_bytes_borrow_as_slices() {
    let public = PublicBytes::new(vec![1, 2, 3]);
    let integer = BigEndianBytes::new(vec![4, 5, 6]);
    let secret = SecretBytes::new(vec![7, 8, 9]);

    assert_eq!(public.as_bytes(), &[1, 2, 3]);
    assert_eq!(integer.as_bytes(), &[4, 5, 6]);
    assert_eq!(secret.expose_secret(), &[7, 8, 9]);
}

#[test]
fn test_secret_bytes_debug_redacts_contents() {
    let secret = SecretBytes::new(vec![0xaa, 0xbb, 0xcc]);
    let debug = format!("{secret:?}");

    assert!(debug.contains("SecretBytes"));
    assert!(debug.contains('3'));
    assert!(!debug.contains("170"));
    assert!(!debug.contains("187"));
    assert!(!debug.contains("204"));
}

#[test]
fn test_ecc_typed_key_material_uses_semantic_bytes() {
    let mut rng = common::seeded_rng(67);
    let keypair = ecc_keygen_bytes(EccCurve::P256, &mut rng).unwrap();
    let public =
        ecc_public_from_private_bytes(EccCurve::P256, keypair.private_key.expose_secret()).unwrap();

    assert_eq!(keypair.private_key.expose_secret().len(), 32);
    assert_eq!(keypair.public_x.as_bytes().len(), 32);
    assert_eq!(keypair.public_y.as_bytes().len(), 32);
    assert_eq!(public.public_x.as_bytes(), keypair.public_x.as_bytes());
    assert_eq!(public.public_y.as_bytes(), keypair.public_y.as_bytes());
}

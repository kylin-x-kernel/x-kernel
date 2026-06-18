// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

mod common;

use tee_crypto::{
    asymmetric::{
        Decryptor, Encryptor, KeyAgreement, Keypair, PublicKeyComponents, Signer, Verifier,
    },
    hash::{DigestBytes, HashAlgorithm},
    sm2::*,
};

#[test]
fn test_sm2_dsa_keygen_sign_verify_roundtrip() {
    let mut rng = common::seeded_rng(42);
    let keypair = Sm2DsaKeypair::generate(&mut rng, 0).expect("keygen");
    let msg = b"hello SM2 DSA";

    let sig = keypair.sign(msg, &mut rng).expect("sign");
    keypair.verify(msg, &sig).expect("verify");
}

#[test]
fn test_sm2_dsa_verify_wrong_msg_fails() {
    let mut rng = common::seeded_rng(55);
    let keypair = Sm2DsaKeypair::generate(&mut rng, 0).expect("keygen");
    let msg = b"correct SM2 message";
    let sig = keypair.sign(msg, &mut rng).expect("sign");
    assert!(keypair.verify(b"wrong SM2 message", &sig).is_err());
}

#[test]
fn test_sm2_dsa_public_key_components() {
    let mut rng = common::seeded_rng(66);
    let keypair = Sm2DsaKeypair::generate(&mut rng, 0).expect("keygen");
    let comps = keypair.to_public_components().expect("components");
    match comps {
        PublicKeyComponents::Sm2(point) => {
            assert_eq!(point.x().len(), 32);
            assert_eq!(point.y().len(), 32);
        }
        _ => panic!("expected SM2 components"),
    }
}

/// regression_4006 case 433 (GMT 0003.5 A.2): xtest hashes `ptx = Z || msg` with SM3
/// and passes the result as the verify digest.
#[test]
fn test_gmt003_a2_verify_vector() {
    let public_x: [u8; 32] = [
        0x09, 0xf9, 0xdf, 0x31, 0x1e, 0x54, 0x21, 0xa1, 0x50, 0xdd, 0x7d, 0x16, 0x1e, 0x4b, 0xc5,
        0xc6, 0x72, 0x17, 0x9f, 0xad, 0x18, 0x33, 0xfc, 0x07, 0x6b, 0xb0, 0x8f, 0xf3, 0x56, 0xf3,
        0x50, 0x20,
    ];
    let public_y: [u8; 32] = [
        0xcc, 0xea, 0x49, 0x0c, 0xe2, 0x67, 0x75, 0xa5, 0x2d, 0xc6, 0xea, 0x71, 0x8c, 0xc1, 0xaa,
        0x60, 0x0a, 0xed, 0x05, 0xfb, 0xf3, 0x5e, 0x08, 0x4a, 0x66, 0x32, 0xf6, 0x07, 0x2d, 0xa9,
        0xad, 0x13,
    ];
    let sig_bytes: [u8; 64] = [
        0xf5, 0xa0, 0x3b, 0x06, 0x48, 0xd2, 0xc4, 0x63, 0x0e, 0xea, 0xc5, 0x13, 0xe1, 0xbb, 0x81,
        0xa1, 0x59, 0x44, 0xda, 0x38, 0x27, 0xd5, 0xb7, 0x41, 0x43, 0xac, 0x7e, 0xac, 0xee, 0xe7,
        0x20, 0xb3, 0xb1, 0xb6, 0xaa, 0x29, 0xdf, 0x21, 0x2f, 0xd8, 0x76, 0x31, 0x82, 0xbc, 0x0d,
        0x42, 0x1c, 0xa1, 0xbb, 0x90, 0x38, 0xfd, 0x1f, 0x7f, 0x42, 0xd4, 0x84, 0x0b, 0x69, 0xc4,
        0x85, 0xbb, 0xc1, 0xaa,
    ];
    let z: [u8; 32] = [
        0xb2, 0xe1, 0x4c, 0x5c, 0x79, 0xc6, 0xdf, 0x5b, 0x85, 0xf4, 0xfe, 0x7e, 0xd8, 0xdb, 0x7a,
        0x26, 0x2b, 0x9d, 0xa7, 0xe0, 0x7c, 0xcb, 0x0e, 0xa9, 0xf4, 0x74, 0x7b, 0x8c, 0xcd, 0xa8,
        0xa4, 0xf3,
    ];
    let msg = b"message digest";
    let mut ptx = Vec::with_capacity(46);
    ptx.extend_from_slice(&z);
    ptx.extend_from_slice(msg);

    use digest::Digest;
    let e = sm3::Sm3::digest(&ptx);

    let signature = tee_crypto::material::SignatureBytes::new(
        sig_bytes.to_vec(),
        tee_crypto::material::SignatureAlgorithm::Sm2Dsa,
        tee_crypto::material::SignatureEncoding::Raw,
    );
    sm2_dsa_verify(&public_x, &public_y, &e, &signature).expect("GMT A.2 verify");
}

#[test]
fn test_sm2_dsa_raw_digest_sign_verify_roundtrip() {
    let mut rng = common::seeded_rng(77);
    let keypair = Sm2DsaKeypair::generate(&mut rng, 0).expect("keygen");
    let secret_key = keypair.as_inner().to_bytes();
    let public = keypair.to_public_components().expect("components");
    let PublicKeyComponents::Sm2(point) = public else {
        panic!("expected SM2 components");
    };
    let digest = DigestBytes::new([0x5au8; 32].to_vec(), HashAlgorithm::Sm3);

    let sig = sm2_dsa_sign(&secret_key, digest.as_bytes(), &mut rng).expect("sign");
    sm2_dsa_verify(point.x(), point.y(), digest.as_bytes(), &sig).expect("verify");
}

#[test]
fn test_sm2_dsa_raw_digest_rejects_wrong_digest() {
    let mut rng = common::seeded_rng(88);
    let keypair = Sm2DsaKeypair::generate(&mut rng, 0).expect("keygen");
    let secret_key = keypair.as_inner().to_bytes();
    let public = keypair.to_public_components().expect("components");
    let PublicKeyComponents::Sm2(point) = public else {
        panic!("expected SM2 components");
    };
    let digest = DigestBytes::new([0x33u8; 32].to_vec(), HashAlgorithm::Sm3);
    let wrong_digest = DigestBytes::new([0x44u8; 32].to_vec(), HashAlgorithm::Sm3);

    let sig = sm2_dsa_sign(&secret_key, digest.as_bytes(), &mut rng).expect("sign");
    assert!(sm2_dsa_verify(point.x(), point.y(), wrong_digest.as_bytes(), &sig).is_err());
}

#[test]
fn test_sm2_pke_encrypt_decrypt_roundtrip() {
    let mut rng = common::seeded_rng(77);
    let keypair = Sm2PkeKeypair::generate(&mut rng, 0).expect("keygen");

    let msg = b"hello SM2 PKE";
    let ct = keypair.encrypt(msg, &mut rng).expect("encrypt");
    let pt = keypair.decrypt(&ct).expect("decrypt");
    assert_eq!(pt.expose_secret(), msg);
}

#[test]
fn test_sm2_pke_public_key_components() {
    let mut rng = common::seeded_rng(88);
    let keypair = Sm2PkeKeypair::generate(&mut rng, 0).expect("keygen");
    let comps = keypair.to_public_components().expect("components");
    match comps {
        PublicKeyComponents::Sm2(point) => {
            assert_eq!(point.x().len(), 32);
            assert_eq!(point.y().len(), 32);
        }
        _ => panic!("expected SM2 components"),
    }
}

#[test]
fn test_sm2_kep_public_key_components() {
    let mut rng = common::seeded_rng(99);
    let keypair = Sm2KepKeypair::generate(&mut rng, 0).expect("keygen");
    let comps = keypair.to_public_components().expect("components");
    match comps {
        PublicKeyComponents::Sm2(point) => {
            assert_eq!(point.x().len(), 32);
            assert_eq!(point.y().len(), 32);
        }
        _ => panic!("expected SM2 components"),
    }
}

#[test]
fn test_sm2_kep_shared_secret_symmetry() {
    let mut rng = common::seeded_rng(100);
    let alice = Sm2KepKeypair::generate(&mut rng, 0).expect("alice keygen");
    let bob = Sm2KepKeypair::generate(&mut rng, 0).expect("bob keygen");

    let alice_pub = alice.to_public_components().expect("alice pub");
    let bob_pub = bob.to_public_components().expect("bob pub");

    let secret_a = alice.shared_secret(&bob_pub).expect("alice shared");
    let secret_b = bob.shared_secret(&alice_pub).expect("bob shared");

    assert_eq!(secret_a, secret_b);
    assert!(!secret_a.expose_secret().is_empty());
}

#[test]
fn test_sm2_kep_different_peers_differ() {
    let mut rng = common::seeded_rng(101);
    let alice = Sm2KepKeypair::generate(&mut rng, 0).expect("alice");
    let bob = Sm2KepKeypair::generate(&mut rng, 0).expect("bob");
    let carol = Sm2KepKeypair::generate(&mut rng, 0).expect("carol");

    let bob_pub = bob.to_public_components().expect("bob pub");
    let carol_pub = carol.to_public_components().expect("carol pub");

    let secret_ab = alice.shared_secret(&bob_pub).expect("ab shared");
    let secret_ac = alice.shared_secret(&carol_pub).expect("ac shared");

    assert_ne!(secret_ab, secret_ac);
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

mod common;

use tee_crypto::{
    CryptoError,
    asymmetric::{EccCurve, KeyAgreement, Keypair, PublicKeyComponents, Signer, Verifier},
    ecc::*,
};

#[test]
fn test_p256_keygen_sign_verify_roundtrip() {
    let mut rng = common::seeded_rng(42);
    let keypair = EccP256Keypair::generate(&mut rng, 0).expect("keygen");
    let msg = b"hello P-256";

    let sig = keypair.sign(msg, &mut rng).expect("sign");
    keypair.verify(msg, &sig).expect("verify");
}

#[test]
fn test_p384_keygen_sign_verify_roundtrip() {
    let mut rng = common::seeded_rng(43);
    let keypair = EccP384Keypair::generate(&mut rng, 0).expect("keygen");
    let msg = b"hello P-384";

    let sig = keypair.sign(msg, &mut rng).expect("sign");
    keypair.verify(msg, &sig).expect("verify");
}

#[test]
fn test_p521_keygen_sign_verify_roundtrip() {
    let mut rng = common::seeded_rng(44);
    let keypair = EccP521Keypair::generate(&mut rng, 0).expect("keygen");
    let msg = b"hello P-521";

    let sig = keypair.sign(msg, &mut rng).expect("sign");
    keypair.verify(msg, &sig).expect("verify");
}

#[test]
fn test_p256_verify_wrong_msg_fails() {
    let mut rng = common::seeded_rng(55);
    let keypair = EccP256Keypair::generate(&mut rng, 0).expect("keygen");
    let msg = b"correct";
    let sig = keypair.sign(msg, &mut rng).expect("sign");
    assert_eq!(
        keypair.verify(b"wrong", &sig).unwrap_err(),
        CryptoError::VerificationFailed
    );
}

#[test]
fn test_p256_public_key_components() {
    let mut rng = common::seeded_rng(66);
    let keypair = EccP256Keypair::generate(&mut rng, 0).expect("keygen");
    let comps = keypair.to_public_components().expect("components");
    match comps {
        PublicKeyComponents::Ecc(point) => {
            assert_eq!(point.curve(), EccCurve::P256);
            assert_eq!(point.x().len(), 32);
            assert_eq!(point.y().len(), 32);
        }
        _ => panic!("expected ECC components"),
    }
}

#[test]
fn test_p256_ecdh_key_agreement() {
    let mut rng = common::seeded_rng(77);
    let alice = EccP256Keypair::generate(&mut rng, 0).expect("alice keygen");
    let bob = EccP256Keypair::generate(&mut rng, 0).expect("bob keygen");

    let alice_components = alice.to_public_components().expect("alice pub");
    let bob_components = bob.to_public_components().expect("bob pub");

    let secret_a = alice.shared_secret(&bob_components).expect("alice shared");
    let secret_b = bob.shared_secret(&alice_components).expect("bob shared");

    assert_eq!(secret_a, secret_b);
}

#[test]
fn test_p384_ecdh_key_agreement() {
    let mut rng = common::seeded_rng(88);
    let alice = EccP384Keypair::generate(&mut rng, 0).expect("alice keygen");
    let bob = EccP384Keypair::generate(&mut rng, 0).expect("bob keygen");

    let alice_components = alice.to_public_components().expect("alice pub");
    let bob_components = bob.to_public_components().expect("bob pub");

    let secret_a = alice.shared_secret(&bob_components).expect("alice shared");
    let secret_b = bob.shared_secret(&alice_components).expect("bob shared");

    assert_eq!(secret_a, secret_b);
}

#[test]
fn test_p521_ecdh_key_agreement() {
    let mut rng = common::seeded_rng(89);
    let alice = EccP521Keypair::generate(&mut rng, 0).expect("alice keygen");
    let bob = EccP521Keypair::generate(&mut rng, 0).expect("bob keygen");

    let alice_components = alice.to_public_components().expect("alice pub");
    let bob_components = bob.to_public_components().expect("bob pub");

    let secret_a = alice.shared_secret(&bob_components).expect("alice shared");
    let secret_b = bob.shared_secret(&alice_components).expect("bob shared");

    assert_eq!(secret_a, secret_b);
}

#[test]
fn test_ecdh_rejects_mismatched_curve_components() {
    let mut rng = common::seeded_rng(90);
    let p256 = EccP256Keypair::generate(&mut rng, 0).expect("p256 keygen");
    let p384 = EccP384Keypair::generate(&mut rng, 0).expect("p384 keygen");
    let p384_components = p384.to_public_components().expect("p384 pub");

    assert_eq!(
        p256.shared_secret(&p384_components).unwrap_err(),
        CryptoError::InvalidInput
    );
}

mod ops_api {
    extern crate alloc;

    use tee_crypto::{CryptoError, hash::HashAlgorithm, tee_ops::ecc::*};

    #[test]
    fn test_p192_keygen() {
        let mut rng = super::common::seeded_rng(41);
        let (sk, px, py) = ecc_keygen(EccCurve::P192, &mut rng).unwrap();
        assert_eq!(sk.len(), 24);
        assert_eq!(px.len(), 24);
        assert_eq!(py.len(), 24);
    }

    #[test]
    fn test_p256_keygen_sign_verify() {
        let mut rng = super::common::seeded_rng(42);
        let (sk, px, py) = ecc_keygen(EccCurve::P256, &mut rng).unwrap();
        let msg = b"hello P-256";
        let hash = super::common::sha256_digest(msg);
        let sig = ecc_sign(EccCurve::P256, EccHashAlgo::Sha256, &sk, &hash, &mut rng).unwrap();
        ecc_verify(EccCurve::P256, EccHashAlgo::Sha256, &px, &py, &hash, &sig).unwrap();
    }

    #[test]
    fn test_p384_keygen_sign_verify() {
        let mut rng = super::common::seeded_rng(43);
        let (sk, px, py) = ecc_keygen(EccCurve::P384, &mut rng).unwrap();
        let msg = b"hello P-384";
        let hash = super::common::sha256_digest(msg);
        let sig = ecc_sign(EccCurve::P384, EccHashAlgo::Sha256, &sk, &hash, &mut rng).unwrap();
        ecc_verify(EccCurve::P384, EccHashAlgo::Sha256, &px, &py, &hash, &sig).unwrap();
    }

    #[test]
    fn test_p521_keygen_sign_verify() {
        let mut rng = super::common::seeded_rng(44);
        let (sk, px, py) = ecc_keygen(EccCurve::P521, &mut rng).unwrap();
        let msg = b"hello P-521";
        let hash = super::common::sha256_digest(msg);
        let sig = ecc_sign(EccCurve::P521, EccHashAlgo::Sha256, &sk, &hash, &mut rng).unwrap();
        ecc_verify(EccCurve::P521, EccHashAlgo::Sha256, &px, &py, &hash, &sig).unwrap();
    }

    #[test]
    fn test_ecc_wrong_signature_fails() {
        let mut rng = super::common::seeded_rng(55);
        let (sk, px, py) = ecc_keygen(EccCurve::P256, &mut rng).unwrap();
        let hash = super::common::sha256_digest(b"correct");
        let sig = ecc_sign(EccCurve::P256, EccHashAlgo::Sha256, &sk, &hash, &mut rng).unwrap();
        let wrong_hash = super::common::sha256_digest(b"wrong");
        assert_eq!(
            ecc_verify(
                EccCurve::P256,
                EccHashAlgo::Sha256,
                &px,
                &py,
                &wrong_hash,
                &sig,
            )
            .unwrap_err(),
            CryptoError::VerificationFailed
        );
    }

    #[test]
    fn test_ecc_public_from_private() {
        let mut rng = super::common::seeded_rng(66);
        let (sk, px_orig, py_orig) = ecc_keygen(EccCurve::P256, &mut rng).unwrap();
        let (px, py) = ecc_public_from_private(EccCurve::P256, &sk).unwrap();
        assert_eq!(px, px_orig);
        assert_eq!(py, py_orig);
    }

    #[test]
    fn test_ecc_typed_raw_key_components() {
        let mut rng = super::common::seeded_rng(67);
        let keypair = ecc_keygen_bytes(EccCurve::P256, &mut rng).unwrap();
        let public =
            ecc_public_from_private_bytes(EccCurve::P256, keypair.private_key.expose_secret())
                .unwrap();
        assert_eq!(public.public_x.as_bytes(), keypair.public_x.as_bytes());
        assert_eq!(public.public_y.as_bytes(), keypair.public_y.as_bytes());
    }

    #[test]
    fn test_ecc_hash_algo_metadata_bridge() {
        assert_eq!(EccHashAlgo::Sha256.hash_algorithm(), HashAlgorithm::Sha256);
        assert_eq!(
            EccHashAlgo::try_from(HashAlgorithm::Sm3).unwrap(),
            EccHashAlgo::Sm3
        );
        assert_eq!(
            EccHashAlgo::try_from(HashAlgorithm::Md5).unwrap_err(),
            CryptoError::UnsupportedAlgorithm
        );
    }

    #[test]
    fn test_p256_ecdh_shared_secret() {
        let mut rng = super::common::seeded_rng(77);
        let (sk_a, px_a, py_a) = ecc_keygen(EccCurve::P256, &mut rng).unwrap();
        let (sk_b, px_b, py_b) = ecc_keygen(EccCurve::P256, &mut rng).unwrap();
        let secret_a = ecc_shared_secret(EccCurve::P256, &sk_a, &px_b, &py_b).unwrap();
        let secret_b = ecc_shared_secret(EccCurve::P256, &sk_b, &px_a, &py_a).unwrap();
        assert_eq!(secret_a, secret_b);
    }

    #[test]
    fn test_sm2_keygen_and_public() {
        let mut rng = super::common::seeded_rng(88);
        let (sk, px, py) = ecc_keygen(EccCurve::Sm2, &mut rng).unwrap();
        let (px2, py2) = ecc_public_from_private(EccCurve::Sm2, &sk).unwrap();
        assert_eq!(px, px2);
        assert_eq!(py, py2);
    }

    #[test]
    fn test_sm2_ecdh_shared_secret() {
        let mut rng = super::common::seeded_rng(99);
        let (sk_a, px_a, py_a) = ecc_keygen(EccCurve::Sm2, &mut rng).unwrap();
        let (sk_b, px_b, py_b) = ecc_keygen(EccCurve::Sm2, &mut rng).unwrap();
        let secret_a = ecc_shared_secret(EccCurve::Sm2, &sk_a, &px_b, &py_b).unwrap();
        let secret_b = ecc_shared_secret(EccCurve::Sm2, &sk_b, &px_a, &py_a).unwrap();
        assert_eq!(secret_a, secret_b);
    }

    #[test]
    fn test_p192_nist1862_testvector1_sign_verify() {
        const PTX: [u8; 128] = [
            0x66, 0xe9, 0x8a, 0x16, 0x58, 0x54, 0xcd, 0x07, 0x98, 0x9b, 0x1e, 0xe0, 0xec, 0x3f,
            0x8d, 0xbe, 0x0e, 0xe3, 0xc2, 0xfb, 0x00, 0x51, 0xef, 0x53, 0xa0, 0xbe, 0x03, 0x45,
            0x7c, 0x4f, 0x21, 0xbc, 0xe7, 0xdc, 0x50, 0xef, 0x4d, 0xf3, 0x74, 0x86, 0xc3, 0x20,
            0x7d, 0xfe, 0xe2, 0x6b, 0xde, 0x4e, 0xd6, 0x23, 0x40, 0xcb, 0xb2, 0xda, 0x78, 0x49,
            0x06, 0xb1, 0xb7, 0x83, 0xb4, 0xd6, 0x01, 0xbd, 0xff, 0x4a, 0xe1, 0xa7, 0xe5, 0xe8,
            0x5a, 0x85, 0xaf, 0xa3, 0x20, 0x8d, 0xc6, 0x0f, 0x09, 0x90, 0xc8, 0x23, 0xbe, 0xdd,
            0xdb, 0x3d, 0xb6, 0x63, 0x42, 0x66, 0x65, 0x15, 0x2e, 0xd7, 0xb0, 0x93, 0xd6, 0xbd,
            0xa5, 0x06, 0xc9, 0x3a, 0x69, 0x4b, 0x83, 0xac, 0x71, 0x55, 0x3f, 0x31, 0xf5, 0xcc,
            0x0d, 0x6b, 0xa2, 0xfa, 0x24, 0x80, 0x90, 0xe8, 0x79, 0x65, 0x73, 0xc4, 0x91, 0x5d,
            0x15, 0x86,
        ];
        const SK: [u8; 24] = [
            0x00, 0x17, 0x89, 0x99, 0x49, 0xd0, 0x2b, 0x55, 0xf9, 0x55, 0x68, 0x46, 0x41, 0x1c,
            0xc9, 0xde, 0x51, 0x2c, 0x6f, 0x16, 0xec, 0xde, 0xb1, 0xc4,
        ];
        const PX: [u8; 24] = [
            0x14, 0xf6, 0x97, 0x38, 0x59, 0x96, 0x89, 0xf5, 0x70, 0x6a, 0xb7, 0x13, 0x43, 0xbe,
            0xcc, 0x88, 0x6e, 0xf1, 0x56, 0x9a, 0x2d, 0x11, 0x37, 0xfe,
        ];
        const PY: [u8; 24] = [
            0x0c, 0xf5, 0xa4, 0x33, 0x90, 0x9e, 0x33, 0x21, 0x7f, 0xb4, 0xdf, 0x6b, 0x95, 0x93,
            0xf7, 0x1d, 0x43, 0xfb, 0x1c, 0x2a, 0x56, 0x53, 0xb7, 0x63,
        ];

        use tee_crypto::hash::{Digest, DigestBytes, HashAlgorithm, Sha1};

        let mut hasher = Sha1::new();
        hasher.update(&PTX);
        let digest = hasher.finalize();
        assert_eq!(digest.algorithm(), HashAlgorithm::Sha1);
        let mut rng = super::common::seeded_rng(1);
        let sig =
            ecc_sign(EccCurve::P192, EccHashAlgo::Sha1, &SK, &digest, &mut rng).expect("sign");
        ecc_verify(EccCurve::P192, EccHashAlgo::Sha1, &PX, &PY, &digest, &sig).expect("verify");
    }

    #[test]
    fn test_p224_nist1862_testvector16_verify_sha1_digest() {
        const PTX: [u8; 128] = [
            0x66, 0xe9, 0x8a, 0x16, 0x58, 0x54, 0xcd, 0x07, 0x98, 0x9b, 0x1e, 0xe0, 0xec, 0x3f,
            0x8d, 0xbe, 0x0e, 0xe3, 0xc2, 0xfb, 0x00, 0x51, 0xef, 0x53, 0xa0, 0xbe, 0x03, 0x45,
            0x7c, 0x4f, 0x21, 0xbc, 0xe7, 0xdc, 0x50, 0xef, 0x4d, 0xf3, 0x74, 0x86, 0xc3, 0x20,
            0x7d, 0xfe, 0xe2, 0x6b, 0xde, 0x4e, 0xd6, 0x23, 0x40, 0xcb, 0xb2, 0xda, 0x78, 0x49,
            0x06, 0xb1, 0xb7, 0x83, 0xb4, 0xd6, 0x01, 0xbd, 0xff, 0x4a, 0xe1, 0xa7, 0xe5, 0xe8,
            0x5a, 0x85, 0xaf, 0xa3, 0x20, 0x8d, 0xc6, 0x0f, 0x09, 0x90, 0xc8, 0x23, 0xbe, 0xdd,
            0xdb, 0x3d, 0xb6, 0x63, 0x42, 0x66, 0x65, 0x15, 0x2e, 0xd7, 0xb0, 0x93, 0xd6, 0xbd,
            0xa5, 0x06, 0xc9, 0x3a, 0x69, 0x4b, 0x83, 0xac, 0x71, 0x55, 0x3f, 0x31, 0xf5, 0xcc,
            0x0d, 0x6b, 0xa2, 0xfa, 0x24, 0x80, 0x90, 0xe8, 0x79, 0x65, 0x73, 0xc4, 0x91, 0x5d,
            0x15, 0x86,
        ];
        const SIG: [u8; 56] = [
            0x96, 0x60, 0xbf, 0xff, 0xc1, 0x73, 0x43, 0x1d, 0x29, 0xf8, 0x3f, 0xa2, 0xaf, 0x0b,
            0xa5, 0x81, 0x79, 0x1b, 0xe3, 0xf4, 0x36, 0x25, 0x31, 0x6f, 0x39, 0x5d, 0x27, 0xff,
            0x9e, 0x8c, 0x3b, 0x82, 0xbc, 0xa2, 0xa4, 0x46, 0x7c, 0x96, 0x94, 0xc6, 0x66, 0xdf,
            0xf7, 0xf0, 0xe7, 0x9d, 0x27, 0x9b, 0xd6, 0x4e, 0xb8, 0x3b, 0xce, 0x2e, 0x30, 0x18,
        ];
        const PX: [u8; 28] = [
            0x56, 0xfb, 0x65, 0x38, 0xf1, 0x72, 0x3d, 0x2b, 0xef, 0x3c, 0x76, 0x41, 0x34, 0x32,
            0x0b, 0x44, 0xba, 0x61, 0x5f, 0x66, 0x3d, 0xb8, 0x04, 0xe5, 0x40, 0x50, 0xb9, 0x5a,
        ];
        const PY: [u8; 28] = [
            0x95, 0x14, 0xa4, 0x42, 0xeb, 0x66, 0xdb, 0xf2, 0xb4, 0x50, 0x74, 0x6f, 0x66, 0xd5,
            0x41, 0x01, 0x87, 0x7a, 0x50, 0xd4, 0xbc, 0x29, 0x10, 0xc6, 0x1d, 0x00, 0x5a, 0xdd,
        ];

        use tee_crypto::hash::{Digest, HashAlgorithm, Sha1};

        let mut hasher = Sha1::new();
        hasher.update(&PTX);
        let sha1_digest = hasher.finalize();
        assert_eq!(sha1_digest.as_bytes().len(), 20);

        // xtest uses SHA1 digest for all ECDSA curves; operation algo is ECDSA_SHA224.
        let digest = tee_crypto::hash::DigestBytes::new(
            sha1_digest.as_bytes().to_vec(),
            HashAlgorithm::Sha224,
        );
        let signature = tee_crypto::material::SignatureBytes::new(
            SIG.to_vec(),
            tee_crypto::material::SignatureAlgorithm::Ecdsa(EccCurve::P224),
            tee_crypto::material::SignatureEncoding::Raw,
        );
        ecc_verify(
            EccCurve::P224,
            EccHashAlgo::Sha224,
            &PX,
            &PY,
            &digest,
            &signature,
        )
        .expect("verify");
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Stage 0.5: stitch tests for `pkix::x509_verify` helpers.

mod common;

extern crate alloc;

use alloc::vec::Vec;

use der::Decode;
use elliptic_curve::{Generate, pkcs8::EncodePublicKey};
use p256::ecdsa::{SigningKey as P256SigningKey, signature::Signer};
use rsa::{
    RsaPrivateKey,
    pkcs1v15::SigningKey as RsaSigningKey,
    signature::{Keypair as RsaKeypair, RandomizedSigner, SignatureEncoding},
};
use sha2::Sha256;
use spki::SubjectPublicKeyInfoRef;
use tee_crypto::{
    asymmetric::{Keypair, PublicKeyComponents},
    pkix::{verify_ecdsa_p256_sha256, verify_rsa_pkcs1v15_sha256, verify_sm2_sign_sm3},
    sm2::{Sm2DsaKeypair, sm2_sign_message, sm2_verify_message_sec1},
};

#[test]
fn stitch_ecdsa_p256_sha256_message_der() {
    let mut rng = common::seeded_rng(301);
    let sk = P256SigningKey::try_generate_from_rng(&mut rng).expect("keygen");
    let msg = b"fake-TBSCertificate-DER-bytes";
    let sig: p256::ecdsa::DerSignature = sk.sign(msg);
    let pk = p256::PublicKey::from_affine(*sk.verifying_key().as_affine()).expect("pk");
    let spki_der = pk.to_public_key_der().expect("spki der");
    let spki = SubjectPublicKeyInfoRef::from_der(spki_der.as_bytes()).expect("spki parse");
    verify_ecdsa_p256_sha256(spki, msg, sig.as_bytes()).expect("x509_verify ecdsa p256");
}

#[test]
fn stitch_rsa_pkcs1v15_sha256_message_der() {
    let mut rng = common::seeded_rng(302);
    let private = RsaPrivateKey::new(&mut rng, 2048).expect("rsa keygen");
    let signing_key = RsaSigningKey::<Sha256>::new(private);
    let msg = b"fake-TBSCertificate-RSA";
    let sig = signing_key.sign_with_rng(&mut rng, msg).to_vec();
    let spki_der = signing_key
        .verifying_key()
        .to_public_key_der()
        .expect("spki");
    let spki = SubjectPublicKeyInfoRef::from_der(spki_der.as_bytes()).expect("spki parse");
    verify_rsa_pkcs1v15_sha256(spki, msg, &sig).expect("x509_verify rsa sha256");
}

#[test]
fn stitch_sm2_message_sec1_and_spki_helper() {
    let mut rng = common::seeded_rng(303);
    let keypair = Sm2DsaKeypair::generate(&mut rng, 0).expect("keygen");
    let comps = keypair.to_public_components().expect("pub");
    let (x, y) = match comps {
        PublicKeyComponents::Sm2(p) => (p.x().to_vec(), p.y().to_vec()),
        _ => panic!("sm2"),
    };
    let mut sec1 = vec![0x04u8];
    sec1.extend_from_slice(&x);
    sec1.extend_from_slice(&y);
    let sk = keypair.as_inner().to_bytes();
    let msg = b"fake-TBSCertificate-SM2";
    let sig = sm2_sign_message(&sk, msg, &mut rng).expect("sign");
    sm2_verify_message_sec1(&sec1, msg, &sig).expect("sm2 module verify");
    let spki_der = build_sm2_spki_der(&sec1);
    let spki = SubjectPublicKeyInfoRef::from_der(&spki_der).expect("spki");
    verify_sm2_sign_sm3(spki, msg, &sig).expect("x509_verify sm2");
}

fn build_sm2_spki_der(sec1: &[u8]) -> Vec<u8> {
    use der::Encode;
    use spki::SubjectPublicKeyInfo;
    let alg_oid = der::asn1::ObjectIdentifier::new_unwrap("1.2.156.10197.1.301");
    let spki = SubjectPublicKeyInfo::<der::Any, der::asn1::BitString> {
        algorithm: spki::AlgorithmIdentifier {
            oid: alg_oid,
            parameters: None,
        },
        subject_public_key: der::asn1::BitString::from_bytes(sec1).expect("bits"),
    };
    spki.to_der().expect("der")
}

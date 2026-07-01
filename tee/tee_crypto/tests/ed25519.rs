// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;

mod common;

use tee_crypto::{
    CryptoError,
    ed25519::{
        ED25519_CTX_MAX_LENGTH, ED25519_SIGNATURE_SIZE_BYTES, Ed25519Variant,
        ed25519_generate_keypair, ed25519_sign, ed25519_sign_variant, ed25519_verify,
        ed25519_verify_variant,
    },
};

fn hex_decode(s: &str) -> alloc::vec::Vec<u8> {
    hex::decode(s).expect("valid test hex")
}

fn key32(bytes: &[u8]) -> [u8; 32] {
    bytes.try_into().expect("32-byte key")
}

#[test]
fn test_ed25519_keygen_sign_verify_roundtrip() {
    let mut rng = common::seeded_rng(42);
    let (seed, public_key) = ed25519_generate_keypair(&mut rng).expect("keygen");
    let message = b"hello Ed25519";

    let sig = ed25519_sign(&seed, message).expect("sign");
    assert_eq!(sig.as_bytes().len(), ED25519_SIGNATURE_SIZE_BYTES);
    ed25519_verify(&public_key, message, sig.as_bytes()).expect("verify");
}

#[test]
fn test_ed25519_verify_wrong_message_fails() {
    let mut rng = common::seeded_rng(55);
    let (seed, public_key) = ed25519_generate_keypair(&mut rng).expect("keygen");
    let sig = ed25519_sign(&seed, b"correct").expect("sign");
    assert_eq!(
        ed25519_verify(&public_key, b"wrong", sig.as_bytes()).unwrap_err(),
        CryptoError::VerificationFailed
    );
}

/// CBOR SigStructure verify vector from `tee_svc_cryp2::test_cryp_ed25519_verify_sig_structure_vector`.
#[test]
fn test_ed25519_verify_sig_structure_vector() {
    let public_key = key32(&hex_decode(
        "720e968320f6d324d29423d546524c7acbb549c12a49e059dbc508c56099f82e",
    ));
    let signature = hex_decode(
        "8399c94482427c9831776073c2e5c3b73f4f8a659601606f3c56aab6b27c68543948cb578a1c7b17f178ac203546d69f6174443a885448e8371659788162a400",
    );
    let message = hex_decode(
        "846a5369676e61747572653143a1012740590188a901782830323836306133303533633262626339666336396262383235663134653134333136393965323930027828376266386562323866393934323830373062363862346263356534323163373166313765306166313a0047445058400bde5e136fceb2c6fdeda53ec8faac595e88f46c5b5a93fe03a3d17f76f5753ce94d5748d2f8709ac546585da5aa1cc13bdba7b013e00fe6b4d1646eb05423bf3a0047445354a23a00011171634156423a000111721a1a0001733a004744525840a6793f45095c98d12b197fd255c2401c885d042df34c25779febefbbd179e7f3fce6b0393526a57e7637a3a7884d78e7047c152d64fcc01928dbc57a00e25f6d3a004744545840000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003a0047445641023a00474457582da50101032704810220062158202ce5042362885a95d0897c033359516fd8e98cd2f5b8e3052dd0a1cd540ddb283a004744584120",
    );

    assert_eq!(signature.len(), ED25519_SIGNATURE_SIZE_BYTES);
    assert_eq!(message.len(), 412);

    ed25519_verify(&public_key, &message, &signature).expect("verify sig structure vector");
}

#[test]
fn test_ed25519_verify_sig_structure_vector_tampered_signature_fails() {
    let public_key = key32(&hex_decode(
        "720e968320f6d324d29423d546524c7acbb549c12a49e059dbc508c56099f82e",
    ));
    let mut signature = hex_decode(
        "8399c94482427c9831776073c2e5c3b73f4f8a659601606f3c56aab6b27c68543948cb578a1c7b17f178ac203546d69f6174443a885448e8371659788162a400",
    );
    let message = hex_decode(
        "846a5369676e61747572653143a1012740590188a901782830323836306133303533633262626339666336396262383235663134653134333136393965323930027828376266386562323866393934323830373062363862346263356534323163373166313765306166313a0047445058400bde5e136fceb2c6fdeda53ec8faac595e88f46c5b5a93fe03a3d17f76f5753ce94d5748d2f8709ac546585da5aa1cc13bdba7b013e00fe6b4d1646eb05423bf3a0047445354a23a00011171634156423a000111721a1a0001733a004744525840a6793f45095c98d12b197fd255c2401c885d042df34c25779febefbbd179e7f3fce6b0393526a57e7637a3a7884d78e7047c152d64fcc01928dbc57a00e25f6d3a004744545840000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003a0047445641023a00474457582da50101032704810220062158202ce5042362885a95d0897c033359516fd8e98cd2f5b8e3052dd0a1cd540ddb283a004744584120",
    );

    signature[ED25519_SIGNATURE_SIZE_BYTES - 1] ^= 0xff;
    assert_eq!(
        ed25519_verify(&public_key, &message, &signature).unwrap_err(),
        CryptoError::VerificationFailed
    );
}

/// RFC 8032 section 7.2 Ed25519ctx test vector.
#[test]
fn test_ed25519ctx_rfc8032_7_2_sign_verify() {
    const PRIVATE: [u8; 32] = [
        0x03, 0x05, 0x33, 0x4e, 0x38, 0x1a, 0xf7, 0x8f, 0x14, 0x1c, 0xb6, 0x66, 0xf6, 0x19, 0x9f,
        0x57, 0xbc, 0x34, 0x95, 0x33, 0x5a, 0x25, 0x6a, 0x95, 0xbd, 0x2a, 0x55, 0xbf, 0x54, 0x66,
        0x63, 0xf6,
    ];
    const PUBLIC: [u8; 32] = [
        0xdf, 0xc9, 0x42, 0x5e, 0x4f, 0x96, 0x8f, 0x7f, 0x0c, 0x29, 0xf0, 0x25, 0x9c, 0xf5, 0xf9,
        0xae, 0xd6, 0x85, 0x1c, 0x2b, 0xb4, 0xad, 0x8b, 0xfb, 0x86, 0x0c, 0xfe, 0xe0, 0xab, 0x24,
        0x82, 0x92,
    ];
    const MESSAGE: [u8; 16] = [
        0xf7, 0x26, 0x93, 0x6d, 0x19, 0xc8, 0x00, 0x49, 0x4e, 0x3f, 0xda, 0xff, 0x20, 0xb2, 0x76,
        0xa8,
    ];
    const CONTEXT: [u8; 3] = [0x66, 0x6f, 0x6f];
    const SIGNATURE: [u8; 64] = [
        0x55, 0xa4, 0xcc, 0x2f, 0x70, 0xa5, 0x4e, 0x04, 0x28, 0x8c, 0x5f, 0x4c, 0xd1, 0xe4, 0x5a,
        0x7b, 0xb5, 0x20, 0xb3, 0x62, 0x92, 0x91, 0x18, 0x76, 0xca, 0xda, 0x73, 0x23, 0x19, 0x8d,
        0xd8, 0x7a, 0x8b, 0x36, 0x95, 0x0b, 0x95, 0x13, 0x00, 0x22, 0x90, 0x7a, 0x7f, 0xb7, 0xc4,
        0xe9, 0xb2, 0xd5, 0xf6, 0xcc, 0xa6, 0x85, 0xa5, 0x87, 0xb4, 0xb2, 0x1f, 0x4b, 0x88, 0x8e,
        0x4e, 0x7e, 0xdb, 0x0d,
    ];

    let variant = Ed25519Variant {
        prehash: false,
        context: Some(CONTEXT.to_vec()),
    };
    let sig = ed25519_sign_variant(&PRIVATE, &MESSAGE, &variant).expect("sign");
    assert_eq!(sig.as_bytes(), SIGNATURE);
    ed25519_verify_variant(&PUBLIC, &MESSAGE, &SIGNATURE, &variant).expect("verify");
}

#[test]
fn test_ed25519ctx_sign_verify_roundtrip() {
    let mut rng = common::seeded_rng(77);
    let (seed, public_key) = ed25519_generate_keypair(&mut rng).expect("keygen");
    let message = b"Ed25519ctx roundtrip";
    let variant = Ed25519Variant {
        prehash: false,
        context: Some(b"tee-crypto-test".to_vec()),
    };

    let sig = ed25519_sign_variant(&seed, message, &variant).expect("sign");
    ed25519_verify_variant(&public_key, message, sig.as_bytes(), &variant).expect("verify");
}

#[test]
fn test_ed25519_invalid_signature_length_fails() {
    let mut rng = common::seeded_rng(88);
    let (_, public_key) = ed25519_generate_keypair(&mut rng).expect("keygen");
    assert_eq!(
        ed25519_verify(&public_key, b"msg", &[0u8; 63]).unwrap_err(),
        CryptoError::InvalidLength
    );
}

#[test]
fn test_ed25519_variant_context_too_long_fails() {
    let variant = Ed25519Variant {
        prehash: false,
        context: Some(alloc::vec![0u8; ED25519_CTX_MAX_LENGTH + 1]),
    };
    assert_eq!(variant.validate().unwrap_err(), CryptoError::InvalidLength);
}

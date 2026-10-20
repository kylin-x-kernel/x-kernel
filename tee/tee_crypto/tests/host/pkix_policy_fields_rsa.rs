// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use der::Decode;

use super::*;

const PC_NOW: u64 = 1_780_272_000;

// sha256WithRSAEncryption: 1.2.840.113549.1.1.11  (RFC 5912 §2)
const RSA_SHA256_OID: der::asn1::ObjectIdentifier =
    der::asn1::ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.11");
// ecdsa-with-SHA256: 1.2.840.10045.4.3.2  (RFC 5912 §6)
const ECDSA_SHA256_OID: der::asn1::ObjectIdentifier =
    der::asn1::ObjectIdentifier::new_unwrap("1.2.840.10045.4.3.2");
// id-kp-serverAuth: 1.3.6.1.5.5.7.3.1
const ID_KP_SERVER_AUTH: der::asn1::ObjectIdentifier =
    der::asn1::ObjectIdentifier::new_unwrap("1.3.6.1.5.5.7.3.1");

fn load(bytes: &[u8]) -> Certificate {
    Certificate::from_der(bytes).expect("valid DER fixture")
}

// -----------------------------------------------------------------------
// min_rsa_key_bits helper unit tests (PKIX-ken.1.11)
// -----------------------------------------------------------------------

/// Direct unit test of `rsa_public_key_bits` helper.
/// Oracle: openssl x509 -inform DER -in leaf-rsa2048.der -text -noout | grep 'Public-Key'
/// → Public-Key: (2048 bit)
#[test]
fn rsa_key_bits_correct_for_2048_key() {
    let cert = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/leaf-rsa2048-365d-san-eku.der"
    ));
    let result = rsa_public_key_bits(&cert.tbs_certificate().subject_public_key_info());
    assert_eq!(
        result,
        Some(2048),
        "RSA-2048 key must return Some(2048) from rsa_public_key_bits"
    );
}

/// Direct unit test of `rsa_public_key_bits` helper.
/// Oracle: openssl x509 -inform DER -in leaf-rsa1024.der -text -noout | grep 'Public-Key'
/// → Public-Key: (1024 bit)
#[test]
fn rsa_key_bits_correct_for_1024_key() {
    let cert = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/leaf-rsa1024-365d-san-eku.der"
    ));
    let result = rsa_public_key_bits(&cert.tbs_certificate().subject_public_key_info());
    assert_eq!(
        result,
        Some(1024),
        "RSA-1024 key must return Some(1024) from rsa_public_key_bits"
    );
}

/// Direct unit test of `rsa_public_key_bits` helper.
/// P-256 key is not RSA; must return None.
#[test]
fn rsa_key_bits_none_for_ec_key() {
    let cert = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/leaf-p256-365d-san-eku.der"
    ));
    let result = rsa_public_key_bits(&cert.tbs_certificate().subject_public_key_info());
    assert_eq!(
        result, None,
        "EC key must return None from rsa_public_key_bits (not RSA)"
    );
}

// -----------------------------------------------------------------------
// min_rsa_key_bits validate_path tests (PKIX-ken.1.11)
// -----------------------------------------------------------------------

/// Oracle: leaf-rsa2048-365d-san-eku.der has RSA-2048 leaf.
/// 2048 >= 2048 → passes.
#[test]
fn min_rsa_key_bits_passes_when_key_meets_limit() {
    let root = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/root-rsa2048.der"
    ));
    let int_cert = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/int-rsa2048.der"
    ));
    let leaf = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/leaf-rsa2048-365d-san-eku.der"
    ));
    let mut policy = ValidationPolicy::new(PC_NOW);
    policy.min_rsa_key_bits = Some(2048);
    let anchors = [TrustAnchor::from_cert(root)];
    validate_path(
        &[leaf, int_cert],
        &anchors,
        &policy,
        &RsaPkcs1v15Sha256Verifier,
    )
    .expect("RSA-2048 leaf with min=2048 should pass");
}

/// Oracle: leaf-rsa1024-365d-san-eku.der has RSA-1024 leaf.
/// 1024 < 2048 → `KeyTooSmall` { index: 0 }.
#[test]
fn min_rsa_key_bits_fails_when_key_too_small() {
    let root = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/root-rsa2048.der"
    ));
    let int_cert = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/int-rsa2048.der"
    ));
    let leaf = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/leaf-rsa1024-365d-san-eku.der"
    ));
    let mut policy = ValidationPolicy::new(PC_NOW);
    policy.min_rsa_key_bits = Some(2048);
    let anchors = [TrustAnchor::from_cert(root)];
    assert!(
        matches!(
            validate_path(
                &[leaf, int_cert],
                &anchors,
                &policy,
                &RsaPkcs1v15Sha256Verifier
            ),
            Err(Error::KeyTooSmall { index: 0 })
        ),
        "RSA-1024 leaf with min=2048 must return KeyTooSmall {{ index: 0 }}"
    );
}

/// Oracle: None = unconstrained; RSA-1024 leaf passes with no key size restriction.
#[test]
fn min_rsa_key_bits_none_is_unconstrained() {
    let root = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/root-rsa2048.der"
    ));
    let int_cert = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/int-rsa2048.der"
    ));
    let leaf = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/leaf-rsa1024-365d-san-eku.der"
    ));
    let mut policy = ValidationPolicy::new(PC_NOW);
    policy.min_rsa_key_bits = None; // default
    let anchors = [TrustAnchor::from_cert(root)];
    validate_path(
        &[leaf, int_cert],
        &anchors,
        &policy,
        &RsaPkcs1v15Sha256Verifier,
    )
    .expect("None min_rsa_key_bits must accept RSA-1024 leaf");
}

/// EC key must not be affected by `min_rsa_key_bits` regardless of the value.
/// Oracle: P-256 key is not RSA; `rsa_public_key_bits` returns None → check skipped.
#[test]
fn min_rsa_key_bits_ec_key_passes_unconditionally() {
    let root = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/root-p256.der"
    ));
    let int_cert = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/int-p256.der"
    ));
    let leaf = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/leaf-p256-365d-san-eku.der"
    ));
    let mut policy = ValidationPolicy::new(PC_NOW);
    // Extremely high floor — would reject any RSA key, but P-256 is not RSA.
    policy.min_rsa_key_bits = Some(16384);
    let anchors = [TrustAnchor::from_cert(root)];
    validate_path(&[leaf, int_cert], &anchors, &policy, &EcdsaP256Verifier)
        .expect("EC key must not be affected by min_rsa_key_bits");
}

// -----------------------------------------------------------------------
// allowed_signature_algs: RSA chain test (PKIX-ken.1.10)
// -----------------------------------------------------------------------

/// Oracle: RSA chain uses sha256WithRSAEncryption; ECDSA-only allowlist must reject it.
#[test]
fn alg_allowlist_fails_on_rsa_chain_when_only_ecdsa_allowed() {
    let root = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/root-rsa2048.der"
    ));
    let int_cert = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/int-rsa2048.der"
    ));
    let leaf = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/leaf-rsa2048-365d-san-eku.der"
    ));
    let mut policy = ValidationPolicy::new(PC_NOW);
    // Only ECDSA allowed; RSA chain must fail.
    policy.allowed_signature_algs = Some(vec![ECDSA_SHA256_OID]);
    let anchors = [TrustAnchor::from_cert(root)];
    assert!(
        matches!(
            validate_path(
                &[leaf, int_cert],
                &anchors,
                &policy,
                &RsaPkcs1v15Sha256Verifier
            ),
            Err(Error::AlgorithmNotAllowed { .. })
        ),
        "RSA chain with ECDSA-only allowlist must return AlgorithmNotAllowed"
    );
}

/// Oracle: RSA chain with RSA in allowlist must pass.
#[test]
fn alg_allowlist_passes_for_rsa_chain() {
    let root = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/root-rsa2048.der"
    ));
    let int_cert = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/int-rsa2048.der"
    ));
    let leaf = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/leaf-rsa2048-365d-san-eku.der"
    ));
    let mut policy = ValidationPolicy::new(PC_NOW);
    policy.allowed_signature_algs = Some(vec![RSA_SHA256_OID]);
    let anchors = [TrustAnchor::from_cert(root)];
    validate_path(
        &[leaf, int_cert],
        &anchors,
        &policy,
        &RsaPkcs1v15Sha256Verifier,
    )
    .expect("RSA chain with RSA-SHA256 in allowlist should pass");
}

/// EKU tests for RSA chain are structurally identical to P-256; spot-check one.
///
/// Oracle: leaf-rsa2048-365d-san-eku.der has EKU=serverAuth.
#[test]
fn required_eku_passes_for_rsa_chain() {
    let root = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/root-rsa2048.der"
    ));
    let int_cert = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/int-rsa2048.der"
    ));
    let leaf = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/leaf-rsa2048-365d-san-eku.der"
    ));
    let mut policy = ValidationPolicy::new(PC_NOW);
    policy.required_leaf_eku = Some(vec![ID_KP_SERVER_AUTH]);
    let anchors = [TrustAnchor::from_cert(root)];
    validate_path(
        &[leaf, int_cert],
        &anchors,
        &policy,
        &RsaPkcs1v15Sha256Verifier,
    )
    .expect("RSA leaf with serverAuth EKU must pass required_leaf_eku=[serverAuth]");
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use der::Decode;

use super::*;

// GRY_NOW is also the test time for these fixtures (2026-06-01T00:00:00Z).
const PC_NOW: u64 = 1_780_272_000;

// OID constants — values from const_oid spec, NOT derived from the code under test.
// ecdsa-with-SHA256: 1.2.840.10045.4.3.2  (RFC 5912 §6)
const ECDSA_SHA256_OID: der::asn1::ObjectIdentifier =
    der::asn1::ObjectIdentifier::new_unwrap("1.2.840.10045.4.3.2");
// sha256WithRSAEncryption: 1.2.840.113549.1.1.11  (RFC 5912 §2)
const RSA_SHA256_OID: der::asn1::ObjectIdentifier =
    der::asn1::ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.11");
// id-kp-serverAuth: 1.3.6.1.5.5.7.3.1  (RFC 5280 §4.2.1.12)
const ID_KP_SERVER_AUTH: der::asn1::ObjectIdentifier =
    der::asn1::ObjectIdentifier::new_unwrap("1.3.6.1.5.5.7.3.1");
// id-kp-emailProtection: 1.3.6.1.5.5.7.3.4  (RFC 5280 §4.2.1.12)
const ID_KP_EMAIL_PROTECTION: der::asn1::ObjectIdentifier =
    der::asn1::ObjectIdentifier::new_unwrap("1.3.6.1.5.5.7.3.4");

fn load(bytes: &[u8]) -> Certificate {
    Certificate::from_der(bytes).expect("valid DER fixture")
}

// -----------------------------------------------------------------------
// max_validity_secs (PKIX-ken.1.9)
// -----------------------------------------------------------------------

/// Oracle: all certs in the chain have validity ≤ 3652 days (10-year root/int,
/// 365-day leaf). A cap of 4000 days allows all of them through.
#[test]
fn max_validity_passes_when_cert_within_limit() {
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
    // 4000-day cap: root/int have ~3652 days, leaf has 365 days — all within limit.
    policy.max_validity_secs = Some(4_000 * 86_400);
    let anchors = [TrustAnchor::from_cert(root)];
    validate_path(&[leaf, int_cert], &anchors, &policy, &EcdsaP256Verifier)
        .expect("all certs within 4000-day cap should validate");
}

/// Oracle: root-p256.der and int-p256.der each have ~3652-day validity
/// (NOT_BEFORE=2026-01-01, NOT_AFTER=2036-01-01 from gen.py).
/// A cap of 400 days forces `ValidityPeriodExceedsMax` on the root (checked first
/// by `chain_walk` which iterates from high index to low).
///
/// Note: the check applies to every cert in the chain, not just the leaf.
/// The root cert (highest index) is checked first and produces the error.
#[test]
fn max_validity_fails_when_cert_exceeds_limit() {
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
    // 400-day cap: root/int have 3652-day validity → ValidityPeriodExceedsMax.
    // Wildcard index because the root (highest-index cert) is checked first.
    policy.max_validity_secs = Some(400 * 86_400);
    let anchors = [TrustAnchor::from_cert(root)];
    assert!(
        matches!(
            validate_path(&[leaf, int_cert], &anchors, &policy, &EcdsaP256Verifier),
            Err(Error::ValidityPeriodExceedsMax { .. })
        ),
        "certs with 3652-day validity over 400-day cap must return ValidityPeriodExceedsMax"
    );
}

/// Isolates the leaf-only failure: use a 1-cert self-issued chain where
/// the cert acts as both leaf and anchor. The 400-day cert fails a 398-day cap.
///
/// Oracle: leaf-p256-400d-san-eku.der has notAfter-notBefore = 400 days = 34,560,000 s.
/// 400 days > 398 days → `ValidityPeriodExceedsMax` { index: 0 }.
#[test]
fn max_validity_fails_at_leaf_index_zero() {
    // Use a single self-signed cert as both chain[0] and anchor so there is only
    // one cert in the chain, making index 0 the only possible failure point.
    // leaf-p256-400d-san-eku.der is NOT self-signed, so we use a known self-signed
    // cert from the existing fixture set (ec-p256-sha256.der) which has a long
    // validity, then set max to 1 day to force failure at index 0.
    let cert = Certificate::from_der(include_bytes!(
        "../../tests/fixtures/pkix/ec-p256-sha256.der"
    ))
    .expect("parse ec-p256-sha256.der");
    let anchors = [TrustAnchor::from_cert(cert.clone())];
    let mut policy = ValidationPolicy::new(1_780_272_000); // PC_NOW: 2026-06-01
    // 1-day cap: the cert has multi-year validity → fails at index 0.
    policy.max_validity_secs = Some(86_400);
    assert!(
        matches!(
            validate_path(&[cert], &anchors, &policy, &EcdsaP256Verifier),
            Err(Error::ValidityPeriodExceedsMax { index: 0 })
        ),
        "1-cert chain: long-validity cert with 1-day cap must return ValidityPeriodExceedsMax {{ \
         index: 0 }}"
    );
}

/// Oracle: None = unconstrained, any validity length is accepted.
#[test]
fn max_validity_none_is_unconstrained() {
    let root = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/root-p256.der"
    ));
    let int_cert = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/int-p256.der"
    ));
    let leaf = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/leaf-p256-400d-san-eku.der"
    ));
    let mut policy = ValidationPolicy::new(PC_NOW);
    policy.max_validity_secs = None; // default, but explicit for documentation
    let anchors = [TrustAnchor::from_cert(root)];
    validate_path(&[leaf, int_cert], &anchors, &policy, &EcdsaP256Verifier)
        .expect("None cap must accept any validity length");
}

// -----------------------------------------------------------------------
// allowed_signature_algs (PKIX-ken.1.10)
// -----------------------------------------------------------------------

/// Oracle: P-256 chain uses ecdsa-with-SHA256; allowlist contains that OID.
#[test]
fn alg_allowlist_passes_when_oid_in_list() {
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
    policy.allowed_signature_algs = Some(vec![ECDSA_SHA256_OID]);
    let anchors = [TrustAnchor::from_cert(root)];
    validate_path(&[leaf, int_cert], &anchors, &policy, &EcdsaP256Verifier)
        .expect("ECDSA-SHA256 chain with ECDSA-SHA256 allowlist should pass");
}

/// Oracle: P-256 chain uses ecdsa-sha256; allowlist contains only RSA-sha256.
/// `chain_walk` walks highest index first: leaf=[0], int=[1], root=[2].
/// For a 3-cert chain, the root-adjacent cert is at index 2 in the slice.
/// `chain_walk` iterates i from (chain.len()-1) down to 0, so i=2 (root) is checked
/// first and fails with `AlgorithmNotAllowed` { index: 2 }.
#[test]
fn alg_allowlist_fails_when_oid_not_in_list() {
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
    // Only RSA allowed, but chain uses ECDSA.
    policy.allowed_signature_algs = Some(vec![RSA_SHA256_OID]);
    let anchors = [TrustAnchor::from_cert(root)];
    assert!(
        matches!(
            validate_path(&[leaf, int_cert], &anchors, &policy, &EcdsaP256Verifier),
            Err(Error::AlgorithmNotAllowed { .. })
        ),
        "ECDSA chain with RSA-only allowlist must return AlgorithmNotAllowed"
    );
}

/// Oracle: None = unconstrained, any algorithm is accepted.
#[test]
fn alg_allowlist_none_is_unconstrained() {
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
    policy.allowed_signature_algs = None; // default
    let anchors = [TrustAnchor::from_cert(root)];
    validate_path(&[leaf, int_cert], &anchors, &policy, &EcdsaP256Verifier)
        .expect("None allowlist must accept any algorithm");
}

// -----------------------------------------------------------------------
// require_subject_alt_name (PKIX-ken.1.12)
// -----------------------------------------------------------------------

/// Oracle: leaf-p256-365d-san-eku.der has SAN=DNS:test.example.com.
#[test]
fn require_san_passes_when_san_present() {
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
    policy.require_subject_alt_name = true;
    let anchors = [TrustAnchor::from_cert(root)];
    validate_path(&[leaf, int_cert], &anchors, &policy, &EcdsaP256Verifier)
        .expect("leaf with SAN must pass require_subject_alt_name=true");
}

/// Oracle: leaf-p256-365d-no-san.der has no SAN extension.
#[test]
fn require_san_fails_when_san_absent() {
    let root = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/root-p256.der"
    ));
    let int_cert = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/int-p256.der"
    ));
    let leaf = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/leaf-p256-365d-no-san.der"
    ));
    let mut policy = ValidationPolicy::new(PC_NOW);
    policy.require_subject_alt_name = true;
    let anchors = [TrustAnchor::from_cert(root)];
    assert!(
        matches!(
            validate_path(&[leaf, int_cert], &anchors, &policy, &EcdsaP256Verifier),
            Err(Error::MissingSan)
        ),
        "leaf without SAN must return MissingSan when require_subject_alt_name=true"
    );
}

/// Oracle: false = default = no SAN requirement; missing SAN is not an error.
#[test]
fn require_san_false_does_not_fail_on_missing_san() {
    let root = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/root-p256.der"
    ));
    let int_cert = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/int-p256.der"
    ));
    let leaf = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/leaf-p256-365d-no-san.der"
    ));
    let mut policy = ValidationPolicy::new(PC_NOW);
    policy.require_subject_alt_name = false; // default, explicit for documentation
    let anchors = [TrustAnchor::from_cert(root)];
    validate_path(&[leaf, int_cert], &anchors, &policy, &EcdsaP256Verifier)
        .expect("require_subject_alt_name=false must not fail on missing SAN");
}

/// Regression guard for the i == 0 guard in `chain_walk`.
///
/// int-p256.der has no SAN extension. With `require_subject_alt_name=true`,
/// the check MUST NOT fail on the intermediate (i == 1). Only the leaf
/// (i == 0) is checked.
///
/// Oracle: openssl x509 -inform DER -in int-p256.der -text -noout | grep -i alt
/// → empty output; int-p256.der has no SAN. Confirmed during fixture generation.
#[test]
fn require_san_only_checks_leaf_not_intermediates() {
    let root = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/root-p256.der"
    ));
    let int_cert = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/int-p256.der"
    ));
    // The leaf HAS a SAN; the intermediate does NOT.
    let leaf = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/leaf-p256-365d-san-eku.der"
    ));
    let mut policy = ValidationPolicy::new(PC_NOW);
    policy.require_subject_alt_name = true;
    let anchors = [TrustAnchor::from_cert(root)];
    // Must pass: the SAN-less intermediate is not checked, only the leaf.
    validate_path(&[leaf, int_cert], &anchors, &policy, &EcdsaP256Verifier)
        .expect("i==0 guard must ensure only the leaf is checked for SAN presence");
}

// -----------------------------------------------------------------------
// required_leaf_eku (PKIX-ken.1.13)
// -----------------------------------------------------------------------

/// Oracle: leaf-p256-365d-san-eku.der has EKU=serverAuth (1.3.6.1.5.5.7.3.1).
#[test]
fn required_eku_passes_when_all_oids_present() {
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
    policy.required_leaf_eku = Some(vec![ID_KP_SERVER_AUTH]);
    let anchors = [TrustAnchor::from_cert(root)];
    validate_path(&[leaf, int_cert], &anchors, &policy, &EcdsaP256Verifier)
        .expect("leaf with serverAuth EKU must pass required_leaf_eku=[serverAuth]");
}

/// Oracle: leaf-p256-365d-no-eku.der has no EKU extension.
/// `required_leaf_eku=Some`([serverAuth]) with absent EKU → `MissingEku`.
#[test]
fn required_eku_fails_when_eku_extension_absent() {
    let root = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/root-p256.der"
    ));
    let int_cert = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/int-p256.der"
    ));
    let leaf = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/leaf-p256-365d-no-eku.der"
    ));
    let mut policy = ValidationPolicy::new(PC_NOW);
    policy.required_leaf_eku = Some(vec![ID_KP_SERVER_AUTH]);
    let anchors = [TrustAnchor::from_cert(root)];
    assert!(
        matches!(
            validate_path(&[leaf, int_cert], &anchors, &policy, &EcdsaP256Verifier),
            Err(Error::MissingEku)
        ),
        "leaf without EKU extension must return MissingEku when an EKU OID is required"
    );
}

/// Oracle: leaf-p256-365d-wrong-eku.der has EKU=emailProtection only, not serverAuth.
#[test]
fn required_eku_fails_when_required_oid_not_in_list() {
    let root = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/root-p256.der"
    ));
    let int_cert = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/int-p256.der"
    ));
    let leaf = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/leaf-p256-365d-wrong-eku.der"
    ));
    let mut policy = ValidationPolicy::new(PC_NOW);
    // Requires serverAuth; leaf only has emailProtection.
    policy.required_leaf_eku = Some(vec![ID_KP_SERVER_AUTH]);
    let anchors = [TrustAnchor::from_cert(root)];
    assert!(
        matches!(
            validate_path(&[leaf, int_cert], &anchors, &policy, &EcdsaP256Verifier),
            Err(Error::MissingEku)
        ),
        "leaf with wrong EKU must return MissingEku when required OID is absent"
    );
}

/// Oracle: None = no EKU requirement; missing EKU is not an error.
#[test]
fn required_eku_none_is_unconstrained() {
    let root = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/root-p256.der"
    ));
    let int_cert = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/int-p256.der"
    ));
    let leaf = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/leaf-p256-365d-no-eku.der"
    ));
    let mut policy = ValidationPolicy::new(PC_NOW);
    policy.required_leaf_eku = None; // default
    let anchors = [TrustAnchor::from_cert(root)];
    validate_path(&[leaf, int_cert], &anchors, &policy, &EcdsaP256Verifier)
        .expect("None required_leaf_eku must accept leaf with no EKU");
}

/// Oracle: Some([]) = require zero OIDs → trivially passes regardless of EKU content.
#[test]
fn required_eku_empty_vec_is_unconstrained() {
    let root = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/root-p256.der"
    ));
    let int_cert = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/int-p256.der"
    ));
    let leaf = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/leaf-p256-365d-no-eku.der"
    ));
    let mut policy = ValidationPolicy::new(PC_NOW);
    // Empty vec: Some([]) requires zero OIDs → always passes.
    policy.required_leaf_eku = Some(vec![]);
    let anchors = [TrustAnchor::from_cert(root)];
    validate_path(&[leaf, int_cert], &anchors, &policy, &EcdsaP256Verifier)
        .expect("Some([]) required_leaf_eku (empty) must accept any EKU configuration");
}

/// Verify that emailProtection in `required_leaf_eku` does NOT match serverAuth in the cert.
/// This guards against a hypothetical relaxed OID comparison bug.
#[test]
fn required_eku_emailprotection_does_not_match_serverauth() {
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
    // Require emailProtection; leaf only has serverAuth.
    policy.required_leaf_eku = Some(vec![ID_KP_EMAIL_PROTECTION]);
    let anchors = [TrustAnchor::from_cert(root)];
    assert!(
        matches!(
            validate_path(&[leaf, int_cert], &anchors, &policy, &EcdsaP256Verifier),
            Err(Error::MissingEku)
        ),
        "OID comparison must be exact; emailProtection must not match serverAuth"
    );
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use der::Decode;

use super::*;

// GRY_NOW = 2026-06-01T00:00:00Z (matches the existing P-256 fixture timestamps).
const PC_NOW: u64 = 1_780_272_000;

// 2.16.840.1.101.3.2.1.48.1 — a NIST test policy OID. The chosen value is
// arbitrary; only its absence from any committed fixture matters.
const TEST_POLICY_OID: der::asn1::ObjectIdentifier =
    der::asn1::ObjectIdentifier::new_unwrap("2.16.840.1.101.3.2.1.48.1");
const OID_ORGANIZATION_NAME: der::asn1::ObjectIdentifier =
    der::asn1::ObjectIdentifier::new_unwrap("2.5.4.10");

fn load(bytes: &[u8]) -> Certificate {
    Certificate::from_der(bytes).expect("valid DER fixture")
}

fn make_chain() -> (Certificate, Certificate, Certificate) {
    let root = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/root-p256.der"
    ));
    let int_cert = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/int-p256.der"
    ));
    let leaf = load(include_bytes!(
        "../../tests/fixtures/pkix/policy-checks/leaf-p256-365d-san-eku.der"
    ));
    (root, int_cert, leaf)
}

/// Oracle: `leaf-p256-365d-san-eku.der` has no CertificatePolicies
/// extension. Requiring any specific policy OID must fail.
#[test]
fn required_policy_oid_fails_when_extension_absent() {
    let (root, int_cert, leaf) = make_chain();
    let mut policy = ValidationPolicy::new(PC_NOW);
    policy.required_leaf_policy_oids = Some(vec![TEST_POLICY_OID]);
    let anchors = [TrustAnchor::from_cert(root)];
    let err = validate_path(&[leaf, int_cert], &anchors, &policy, &EcdsaP256Verifier)
        .expect_err("leaf without CertificatePolicies must fail when an OID is required");
    match err {
        Error::MissingLeafPolicyOid { required } => {
            assert_eq!(required, TEST_POLICY_OID, "reported OID must echo input");
        }
        other => panic!("expected MissingLeafPolicyOid, got {other:?}"),
    }
}

/// Oracle: `None` requirement is unconstrained even when the leaf has no
/// `CertificatePolicies` extension. Backward-compatible default.
#[test]
fn required_policy_oid_none_is_unconstrained() {
    let (root, int_cert, leaf) = make_chain();
    let mut policy = ValidationPolicy::new(PC_NOW);
    policy.required_leaf_policy_oids = None;
    let anchors = [TrustAnchor::from_cert(root)];
    validate_path(&[leaf, int_cert], &anchors, &policy, &EcdsaP256Verifier)
        .expect("None required_leaf_policy_oids must not constrain validation");
}

/// Oracle: `Some(vec![])` requires zero OIDs → trivially passes regardless
/// of CertificatePolicies content (mirrors `required_leaf_eku = Some(vec![])`).
#[test]
fn required_policy_oid_empty_vec_is_unconstrained() {
    let (root, int_cert, leaf) = make_chain();
    let mut policy = ValidationPolicy::new(PC_NOW);
    policy.required_leaf_policy_oids = Some(vec![]);
    let anchors = [TrustAnchor::from_cert(root)];
    validate_path(&[leaf, int_cert], &anchors, &policy, &EcdsaP256Verifier)
        .expect("Some([]) required_leaf_policy_oids must accept any CertificatePolicies");
}

/// Oracle: `leaf-p256-365d-san-eku.der` has Subject DN
/// `CN=PKIX-policy-checks-leaf-p256-365d-san-eku` (no organizationName).
/// Requiring `organizationName` must fail with `SubjectDnAttrRuleUnmet`.
#[test]
fn required_dn_attr_field_fails_when_attribute_absent() {
    let (root, int_cert, leaf) = make_chain();
    let mut policy = ValidationPolicy::new(PC_NOW);
    policy.required_leaf_subject_dn_attrs = Some(DnAttrRule::Field(OID_ORGANIZATION_NAME));
    let anchors = [TrustAnchor::from_cert(root)];
    assert!(
        matches!(
            validate_path(&[leaf, int_cert], &anchors, &policy, &EcdsaP256Verifier),
            Err(Error::SubjectDnAttrRuleUnmet)
        ),
        "leaf without organizationName must return SubjectDnAttrRuleUnmet"
    );
}

/// Oracle: `leaf-p256-365d-san-eku.der` has only `CN=...`.
/// `Field(CN)` must succeed (the attribute is present).
#[test]
fn required_dn_attr_field_passes_when_attribute_present() {
    let (root, int_cert, leaf) = make_chain();
    let mut policy = ValidationPolicy::new(PC_NOW);
    // CN = 2.5.4.3
    let oid_cn: der::asn1::ObjectIdentifier = der::asn1::ObjectIdentifier::new_unwrap("2.5.4.3");
    policy.required_leaf_subject_dn_attrs = Some(DnAttrRule::Field(oid_cn));
    let anchors = [TrustAnchor::from_cert(root)];
    validate_path(&[leaf, int_cert], &anchors, &policy, &EcdsaP256Verifier)
        .expect("leaf with CN must pass Field(CN) requirement");
}

/// Oracle: AllOf(vec![]) is vacuously true; chain must validate without
/// any DN constraints actually being checked.
#[test]
fn required_dn_attr_allof_empty_accepts_any_leaf() {
    let (root, int_cert, leaf) = make_chain();
    let mut policy = ValidationPolicy::new(PC_NOW);
    policy.required_leaf_subject_dn_attrs = Some(DnAttrRule::AllOf(Vec::new()));
    let anchors = [TrustAnchor::from_cert(root)];
    validate_path(&[leaf, int_cert], &anchors, &policy, &EcdsaP256Verifier)
        .expect("AllOf(vec![]) is vacuously true; validation must succeed");
}

/// Oracle: AnyOf(vec![]) is vacuously false; any leaf fails.
#[test]
fn required_dn_attr_anyof_empty_rejects_any_leaf() {
    let (root, int_cert, leaf) = make_chain();
    let mut policy = ValidationPolicy::new(PC_NOW);
    policy.required_leaf_subject_dn_attrs = Some(DnAttrRule::AnyOf(Vec::new()));
    let anchors = [TrustAnchor::from_cert(root)];
    assert!(
        matches!(
            validate_path(&[leaf, int_cert], &anchors, &policy, &EcdsaP256Verifier),
            Err(Error::SubjectDnAttrRuleUnmet)
        ),
        "AnyOf(vec![]) is vacuously false; validation must fail"
    );
}

/// Oracle: `None` is the default and must not constrain validation.
#[test]
fn required_dn_attr_none_is_unconstrained() {
    let (root, int_cert, leaf) = make_chain();
    let mut policy = ValidationPolicy::new(PC_NOW);
    policy.required_leaf_subject_dn_attrs = None;
    let anchors = [TrustAnchor::from_cert(root)];
    validate_path(&[leaf, int_cert], &anchors, &policy, &EcdsaP256Verifier)
        .expect("None required_leaf_subject_dn_attrs must not constrain validation");
}

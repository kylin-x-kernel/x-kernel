// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::str::FromStr;

use super::*;

// RFC 4519 / X.520 DN attribute OIDs (values from spec, not derived from code under test).
const OID_CN: der::asn1::ObjectIdentifier = der::asn1::ObjectIdentifier::new_unwrap("2.5.4.3");
const OID_SURNAME: der::asn1::ObjectIdentifier = der::asn1::ObjectIdentifier::new_unwrap("2.5.4.4");
const OID_ORGANIZATION_NAME: der::asn1::ObjectIdentifier =
    der::asn1::ObjectIdentifier::new_unwrap("2.5.4.10");
const OID_GIVEN_NAME: der::asn1::ObjectIdentifier =
    der::asn1::ObjectIdentifier::new_unwrap("2.5.4.42");
const OID_PSEUDONYM: der::asn1::ObjectIdentifier =
    der::asn1::ObjectIdentifier::new_unwrap("2.5.4.65");

fn dn(s: &str) -> x509_cert::name::Name {
    x509_cert::name::Name::from_str(s).expect("valid RFC 4514 DN string")
}

fn empty_dn() -> x509_cert::name::Name {
    x509_cert::name::Name::default()
}

// ---- dn_contains_oid -----------------------------------------------------

#[test]
fn dn_contains_oid_finds_present_attribute() {
    let subject = dn("CN=test,O=Test Org");
    assert!(dn_contains_oid(&subject, &OID_ORGANIZATION_NAME));
    assert!(dn_contains_oid(&subject, &OID_CN));
}

#[test]
fn dn_contains_oid_returns_false_for_absent_attribute() {
    let subject = dn("CN=test");
    assert!(!dn_contains_oid(&subject, &OID_ORGANIZATION_NAME));
    assert!(!dn_contains_oid(&subject, &OID_SURNAME));
}

#[test]
fn dn_contains_oid_empty_subject_matches_nothing() {
    let subject = empty_dn();
    assert!(!dn_contains_oid(&subject, &OID_CN));
}

// ---- evaluate_dn_attr_rule: Field ---------------------------------------

#[test]
fn rule_field_matches_when_attribute_present() {
    let subject = dn("CN=test,O=Test Org");
    let rule = DnAttrRule::Field(OID_ORGANIZATION_NAME);
    assert!(evaluate_dn_attr_rule(&subject, &rule));
}

#[test]
fn rule_field_rejects_when_attribute_absent() {
    let subject = dn("CN=test");
    let rule = DnAttrRule::Field(OID_ORGANIZATION_NAME);
    assert!(!evaluate_dn_attr_rule(&subject, &rule));
}

// ---- evaluate_dn_attr_rule: AllOf composition ---------------------------

#[test]
fn rule_allof_matches_when_every_branch_satisfied() {
    let subject = dn("CN=test,givenName=Alice,surname=Smith");
    let rule = DnAttrRule::AllOf(vec![
        DnAttrRule::Field(OID_GIVEN_NAME),
        DnAttrRule::Field(OID_SURNAME),
    ]);
    assert!(evaluate_dn_attr_rule(&subject, &rule));
}

#[test]
fn rule_allof_rejects_when_any_branch_missing() {
    let subject = dn("CN=test,givenName=Alice");
    let rule = DnAttrRule::AllOf(vec![
        DnAttrRule::Field(OID_GIVEN_NAME),
        DnAttrRule::Field(OID_SURNAME),
    ]);
    assert!(!evaluate_dn_attr_rule(&subject, &rule));
}

// ---- evaluate_dn_attr_rule: AnyOf composition ---------------------------

#[test]
fn rule_anyof_matches_when_at_least_one_branch_satisfied() {
    let subject = dn("CN=test,pseudonym=BlueFox");
    let rule = DnAttrRule::AnyOf(vec![
        DnAttrRule::Field(OID_PSEUDONYM),
        DnAttrRule::AllOf(vec![
            DnAttrRule::Field(OID_GIVEN_NAME),
            DnAttrRule::Field(OID_SURNAME),
        ]),
    ]);
    assert!(evaluate_dn_attr_rule(&subject, &rule));
}

#[test]
fn rule_anyof_rejects_when_no_branch_satisfied() {
    let subject = dn("CN=test");
    let rule = DnAttrRule::AnyOf(vec![
        DnAttrRule::Field(OID_PSEUDONYM),
        DnAttrRule::AllOf(vec![
            DnAttrRule::Field(OID_GIVEN_NAME),
            DnAttrRule::Field(OID_SURNAME),
        ]),
    ]);
    assert!(!evaluate_dn_attr_rule(&subject, &rule));
}

/// Sponsor-validated-style rule (pseudonym OR (givenName AND surname))
/// also matches when both branches of the inner AllOf are present.
#[test]
fn rule_anyof_matches_when_inner_allof_branch_satisfied() {
    let subject = dn("CN=test,givenName=Alice,surname=Smith");
    let rule = DnAttrRule::AnyOf(vec![
        DnAttrRule::Field(OID_PSEUDONYM),
        DnAttrRule::AllOf(vec![
            DnAttrRule::Field(OID_GIVEN_NAME),
            DnAttrRule::Field(OID_SURNAME),
        ]),
    ]);
    assert!(evaluate_dn_attr_rule(&subject, &rule));
}

// ---- evaluate_dn_attr_rule: vacuity -------------------------------------

/// RFC-conventional vacuity: AllOf with no branches is trivially true
/// (the universal quantifier over an empty set is true).
#[test]
fn rule_allof_empty_accepts_everything() {
    let empty = empty_dn();
    let cn_only = dn("CN=test");
    let rule = DnAttrRule::AllOf(Vec::new());
    assert!(evaluate_dn_attr_rule(&empty, &rule));
    assert!(evaluate_dn_attr_rule(&cn_only, &rule));
}

/// RFC-conventional vacuity: AnyOf with no branches is trivially false
/// (the existential quantifier over an empty set is false).
#[test]
fn rule_anyof_empty_rejects_everything() {
    let empty = empty_dn();
    let cn_only = dn("CN=test");
    let rule = DnAttrRule::AnyOf(Vec::new());
    assert!(!evaluate_dn_attr_rule(&empty, &rule));
    assert!(!evaluate_dn_attr_rule(&cn_only, &rule));
}

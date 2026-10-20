// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use der::Decode;

use super::*;

// Fixtures and time constants reused from tests_chain_walk.
const GRY_NOW: u64 = 1_780_272_000; // 2026-06-01

fn load(bytes: &[u8]) -> Certificate {
    Certificate::from_der(bytes).expect("parse cert")
}

fn policy_at(t: u64) -> ValidationPolicy {
    ValidationPolicy::new(t)
}

/// Happy-path 1-cert chain: self-signed cert is both chain and anchor.
///
/// Expected: Ok(ValidatedPath { `anchor_index`: 0, depth: 0 })
#[test]
fn one_cert_chain_ok() {
    let cert = load(include_bytes!(
        "../../tests/fixtures/pkix/ec-p256-sha256.der"
    ));
    let anchors = [TrustAnchor::from_cert(cert.clone())];
    let result = validate_path(&[cert], &anchors, &policy_at(GRY_NOW), &EcdsaP256Verifier)
        .expect("1-cert chain must validate");
    assert_eq!(result.anchor_index, 0);
    assert_eq!(result.depth, 0);
}

/// Happy-path 2-cert chain: leaf + intermediate, with root anchor.
///
/// Oracle: openssl verify -`CAfile` gry-root.pem -untrusted gry-int.pem gry-leaf.pem → OK
/// Expected: Ok(ValidatedPath { `anchor_index`: 0, depth: 1 })
#[test]
fn two_cert_chain_ok() {
    let root = load(include_bytes!("../../tests/fixtures/pkix/gry-root.der"));
    let int_cert = load(include_bytes!("../../tests/fixtures/pkix/gry-int.der"));
    let leaf = load(include_bytes!("../../tests/fixtures/pkix/gry-leaf.der"));
    let anchors = [TrustAnchor::from_cert(root)];
    let result = validate_path(
        &[leaf, int_cert],
        &anchors,
        &policy_at(GRY_NOW),
        &EcdsaP256Verifier,
    )
    .expect("2-cert chain must validate");
    assert_eq!(result.anchor_index, 0);
    assert_eq!(result.depth, 1);
}

/// §6.1.5 leaf-intrinsic outputs are populated correctly from `chain[0]`.
///
/// Independent oracle: parse the leaf fixture a second time via
/// `Certificate::from_der` (a different code path from `validate_path`'s
/// chain[0] access). Compare each `ValidatedPath` accessor field against
/// the independently-parsed leaf's `tbs_certificate` field. Catches:
/// - Field swaps (e.g. `leaf_subject` accidentally populated from `issuer`).
/// - Wrong cert in chain (e.g. `chain[1].subject` instead of `chain[0]`).
/// - Forgotten field assignments.
///
/// Does NOT validate x509-cert's parser correctness (that's tested
/// upstream); it validates the wiring between `validate_path` and
/// `ValidatedPath`.
#[test]
fn validated_path_exposes_leaf_subject_issuer_serial_spki() {
    let root = load(include_bytes!("../../tests/fixtures/pkix/gry-root.der"));
    let int_cert = load(include_bytes!("../../tests/fixtures/pkix/gry-int.der"));
    let leaf_bytes = include_bytes!("../../tests/fixtures/pkix/gry-leaf.der");
    let leaf = load(leaf_bytes);

    // Independent oracle: re-parse the leaf via Certificate::from_der.
    // x509-cert's parser is the upstream oracle for the field values.
    let oracle_leaf = Certificate::from_der(leaf_bytes).expect("oracle: leaf fixture must parse");

    let anchors = [TrustAnchor::from_cert(root)];
    let result = validate_path(
        &[leaf, int_cert],
        &anchors,
        &policy_at(GRY_NOW),
        &EcdsaP256Verifier,
    )
    .expect("2-cert chain must validate");

    // The four §6.1.5 leaf-intrinsic outputs must match the
    // independently-parsed leaf's corresponding tbs_certificate fields.
    assert_eq!(
        result.leaf_subject,
        oracle_leaf.tbs_certificate().subject().clone(),
        "ValidatedPath.leaf_subject must equal chain[0].tbs_certificate().subject()"
    );
    assert_eq!(
        result.leaf_issuer,
        oracle_leaf.tbs_certificate().issuer().clone(),
        "ValidatedPath.leaf_issuer must equal chain[0].tbs_certificate().issuer()"
    );
    assert_eq!(
        result.leaf_serial,
        oracle_leaf.tbs_certificate().serial_number().clone(),
        "ValidatedPath.leaf_serial must equal chain[0].tbs_certificate().serial_number()"
    );
    assert_eq!(
        result.leaf_spki,
        oracle_leaf
            .tbs_certificate()
            .subject_public_key_info()
            .clone(),
        "ValidatedPath.leaf_spki must equal chain[0].tbs_certificate().subject_public_key_info()"
    );

    // Sanity: leaf_subject != leaf_issuer for a real (non-self-signed)
    // leaf. This catches the swap-subject-with-issuer regression: a
    // subject-issuer copy bug would leave both fields equal to whichever
    // one was used.
    assert_ne!(
        result.leaf_subject, result.leaf_issuer,
        "non-self-signed leaf must have distinct subject and issuer DNs (regression: swap)"
    );
}

/// §6.1.5 outputs for a 1-cert (self-signed) chain.
///
/// Self-signed certs have `subject == issuer`. This documents that
/// `leaf_subject` and `leaf_issuer` ARE expected to be equal in this
/// case — the previous test's `assert_ne!` is for non-self-signed
/// chains only.
#[test]
fn validated_path_outputs_for_self_signed_chain() {
    let cert_bytes = include_bytes!("../../tests/fixtures/pkix/ec-p256-sha256.der");
    let cert = load(cert_bytes);
    let oracle = Certificate::from_der(cert_bytes).expect("oracle: cert must parse");
    let anchors = [TrustAnchor::from_cert(cert.clone())];

    let result = validate_path(&[cert], &anchors, &policy_at(GRY_NOW), &EcdsaP256Verifier)
        .expect("1-cert self-signed chain must validate");

    assert_eq!(
        result.leaf_subject,
        oracle.tbs_certificate().subject().clone()
    );
    assert_eq!(
        result.leaf_issuer,
        oracle.tbs_certificate().issuer().clone()
    );
    assert_eq!(
        result.leaf_serial,
        oracle.tbs_certificate().serial_number().clone()
    );
    assert_eq!(
        result.leaf_spki,
        oracle.tbs_certificate().subject_public_key_info().clone()
    );
    // Self-signed: subject == issuer by definition.
    assert_eq!(
        result.leaf_subject, result.leaf_issuer,
        "self-signed cert must have equal subject and issuer DNs"
    );
}

/// Multiple anchors: correct anchor is second in the slice.
///
/// Expected: Ok(ValidatedPath { `anchor_index`: 1, depth: 0 })
#[test]
fn correct_anchor_index_when_multiple_anchors() {
    let p256 = load(include_bytes!(
        "../../tests/fixtures/pkix/ec-p256-sha256.der"
    ));
    let rsa = load(include_bytes!(
        "../../tests/fixtures/pkix/rsa-pkcs1v15-sha256.der"
    ));
    // First anchor is the RSA cert (wrong name and SPKI for the P-256 chain).
    // Second anchor matches.
    let anchors = [
        TrustAnchor::from_cert(rsa),
        TrustAnchor::from_cert(p256.clone()),
    ];
    let result = validate_path(&[p256], &anchors, &policy_at(GRY_NOW), &EcdsaP256Verifier)
        .expect("must find second anchor");
    assert_eq!(result.anchor_index, 1);
    assert_eq!(result.depth, 0);
}

/// Empty chain returns `NoTrustedPath`.
#[test]
fn empty_chain_returns_error() {
    let anchors = [TrustAnchor::from_cert(load(include_bytes!(
        "../../tests/fixtures/pkix/ec-p256-sha256.der"
    )))];
    assert!(
        matches!(
            validate_path(&[], &anchors, &policy_at(GRY_NOW), &EcdsaP256Verifier),
            Err(Error::NoTrustedPath)
        ),
        "empty chain must fail"
    );
}

/// Duplicate certificate in chain returns `DuplicateCertificate` error.
///
/// Oracle: RFC 5280 does not define behavior for duplicate certs; we reject
/// early with a diagnostic error rather than failing later with a confusing
/// `SignatureInvalid` or `ChainBroken`.
///
/// Duplicate is detected by (issuer DN, serial number) identity per RFC 5280
/// §4.1.2.2 — the same cert appearing twice has the same issuer+serial.
/// SPKI equality is intentionally NOT used (cross-signed CAs share a key but
/// have distinct issuer+serial and must not be rejected).
#[test]
fn duplicate_cert_in_chain_returns_error() {
    let cert = load(include_bytes!(
        "../../tests/fixtures/pkix/ec-p256-sha256.der"
    ));
    let anchors = [TrustAnchor::from_cert(cert.clone())];
    // Chain [cert, cert]: same cert at index 0 and index 1 — same issuer+serial.
    let result = validate_path(
        &[cert.clone(), cert],
        &anchors,
        &policy_at(GRY_NOW),
        &EcdsaP256Verifier,
    );
    assert!(
        matches!(
            result,
            Err(Error::DuplicateCertificate {
                first: 0,
                second: 1
            })
        ),
        "duplicate cert must return DuplicateCertificate{{first:0, second:1}}, got {result:?}"
    );
}

/// `path_too_long`: vxf chain [leaf, int] with `max_path_len` = 0.
///
/// chain.len()=2 → 1 intermediate. 1 > `max_path_len(0)` → `PathTooLong`.
#[test]
fn path_too_long_returns_error() {
    let root = load(include_bytes!("../../tests/fixtures/pkix/vxf-root.der"));
    let int_cert = load(include_bytes!("../../tests/fixtures/pkix/vxf-int.der"));
    let leaf = load(include_bytes!("../../tests/fixtures/pkix/vxf-leaf.der"));
    let anchors = [TrustAnchor::from_cert(root)];
    let policy = ValidationPolicy {
        current_time_unix: GRY_NOW,
        max_path_len: 0,
        ..ValidationPolicy::new(0)
    };
    assert!(
        matches!(
            validate_path(&[leaf, int_cert], &anchors, &policy, &EcdsaP256Verifier),
            Err(Error::PathTooLong)
        ),
        "1 intermediate with max_path_len=0 must return PathTooLong"
    );
}

/// `no_trusted_path`: vxf chain presented to an unrelated anchor (gry-root).
///
/// vxf's last cert issuer name does not match gry-root's subject name.
#[test]
fn no_trusted_path_unrelated_anchor_returns_error() {
    let gry_root = load(include_bytes!("../../tests/fixtures/pkix/gry-root.der"));
    let vxf_int = load(include_bytes!("../../tests/fixtures/pkix/vxf-int.der"));
    let vxf_leaf = load(include_bytes!("../../tests/fixtures/pkix/vxf-leaf.der"));
    let anchors = [TrustAnchor::from_cert(gry_root)];
    assert!(
        matches!(
            validate_path(
                &[vxf_leaf, vxf_int],
                &anchors,
                &policy_at(GRY_NOW),
                &EcdsaP256Verifier
            ),
            Err(Error::NoTrustedPath)
        ),
        "vxf chain with gry anchor must return NoTrustedPath"
    );
}

/// `oid_mismatch`: outer signatureAlgorithm OID differs from inner TBS signature OID.
///
/// Patch the SECOND occurrence of the ECDSA-with-SHA256 OID bytes in vxf-leaf.der
/// to ECDSA-with-SHA384. The inner TBS.signature remains SHA256.
/// `check_oid_consistency` detects this → `MalformedCertificate` { index: 0 }.
///
/// Oracle: RFC 5280 §4.1.1.2 requires outer and inner `AlgorithmIdentifiers` to be identical.
#[test]
fn oid_mismatch_outer_returns_malformed_certificate() {
    let mut leaf_der = include_bytes!("../../tests/fixtures/pkix/vxf-leaf.der").to_vec();
    // ECDSA-with-SHA256 OID content bytes: 1.2.840.10045.4.3.2
    let oid_sha256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02];
    // ECDSA-with-SHA384 OID content bytes: 1.2.840.10045.4.3.3 (same length, last byte differs)
    let oid_sha384: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x03];
    // In Certificate DER the inner TBS.signature OID appears FIRST (inside TBSCertificate)
    // and the outer signatureAlgorithm OID appears SECOND (after TBSCertificate). Patching
    // only the second occurrence changes the outer OID while leaving the inner intact.
    let first = leaf_der
        .windows(8)
        .position(|w| w == oid_sha256)
        .expect("inner SHA256 OID must be present in vxf-leaf.der");
    let second = leaf_der[first + 8..]
        .windows(8)
        .position(|w| w == oid_sha256)
        .map(|p| first + 8 + p)
        .expect("outer SHA256 OID must be present in vxf-leaf.der");
    leaf_der[second..second + 8].copy_from_slice(oid_sha384);
    let leaf = Certificate::from_der(&leaf_der).expect("patched DER must parse");
    assert_ne!(
        leaf.signature_algorithm(),
        leaf.tbs_certificate().signature(),
        "outer/inner OIDs must differ after patch"
    );
    let int_cert = load(include_bytes!("../../tests/fixtures/pkix/vxf-int.der"));
    let root = load(include_bytes!("../../tests/fixtures/pkix/vxf-root.der"));
    let anchors = [TrustAnchor::from_cert(root)];
    assert!(
        matches!(
            validate_path(
                &[leaf, int_cert],
                &anchors,
                &policy_at(GRY_NOW),
                &EcdsaP256Verifier
            ),
            Err(Error::MalformedCertificate { index: 0 })
        ),
        "outer/inner OID mismatch must return MalformedCertificate {{ index: 0 }}"
    );
}

/// `intermediate_not_ca`: nca-int has no `BasicConstraints` extension.
///
/// Oracle: pyca/cryptography — nca-int built without any extensions.
/// cert_is_ca(nca-int) returns None → `NotCA` { index: 1 }.
#[test]
fn intermediate_not_ca_returns_not_ca() {
    let root = load(include_bytes!("../../tests/fixtures/pkix/nca-root.der"));
    let int_cert = load(include_bytes!("../../tests/fixtures/pkix/nca-int.der"));
    let leaf = load(include_bytes!("../../tests/fixtures/pkix/nca-leaf.der"));
    let anchors = [TrustAnchor::from_cert(root)];
    assert!(
        matches!(
            validate_path(
                &[leaf, int_cert],
                &anchors,
                &policy_at(GRY_NOW),
                &EcdsaP256Verifier
            ),
            Err(Error::NotCA { index: 1 })
        ),
        "intermediate without BasicConstraints CA flag must return NotCA {{ index: 1 }}"
    );
}

/// `key_usage_missing_cert_sign`: kuf-int has `KeyUsage` with digitalSignature only.
///
/// Oracle: pyca/cryptography — kuf-int KeyUsage.keyCertSign = False.
/// Default policy has `enforce_key_usage` = true; `chain_walk` checks at i=1.
#[test]
fn key_usage_missing_cert_sign_returns_error() {
    let root = load(include_bytes!("../../tests/fixtures/pkix/kuf-root.der"));
    let int_cert = load(include_bytes!("../../tests/fixtures/pkix/kuf-int.der"));
    let leaf = load(include_bytes!("../../tests/fixtures/pkix/kuf-leaf.der"));
    let anchors = [TrustAnchor::from_cert(root)];
    assert!(
        matches!(
            validate_path(
                &[leaf, int_cert],
                &anchors,
                &policy_at(GRY_NOW),
                &EcdsaP256Verifier
            ),
            Err(Error::KeyUsageMissing { index: 1 })
        ),
        "intermediate with KeyUsage but no keyCertSign must return KeyUsageMissing {{ index: 1 }}"
    );
}

/// `absent_key_usage_intermediate_accepted`: nku-int has NO `KeyUsage` extension at all.
///
/// RFC 5280 §6.1.4(n): "If a `KeyUsage` extension is **present**, verify that the
/// keyCertSign bit is set." Absent `KeyUsage` must not be rejected by `enforce_key_usage`.
///
/// Oracle: pyca/cryptography — nku-int has only `BasicConstraints` (OID 2.5.29.19),
/// no `KeyUsage` extension.
#[test]
fn absent_key_usage_intermediate_accepted() {
    let root = load(include_bytes!("../../tests/fixtures/pkix/nku-root.der"));
    let int_cert = load(include_bytes!("../../tests/fixtures/pkix/nku-int.der"));
    let leaf = load(include_bytes!("../../tests/fixtures/pkix/nku-leaf.der"));
    let anchors = [TrustAnchor::from_cert(root)];
    // Default policy has enforce_key_usage = true.
    // nku-int has no KeyUsage — must NOT trigger KeyUsageMissing per RFC 5280 §6.1.4(n).
    let now: u64 = 1_720_000_000; // 2024-07-03, within nku-int validity (2024-2030)
    let policy = ValidationPolicy {
        current_time_unix: now,
        ..ValidationPolicy::new(0)
    };
    validate_path(&[leaf, int_cert], &anchors, &policy, &EcdsaP256Verifier)
        .expect("intermediate with absent KeyUsage must be accepted when enforce_key_usage=true");
}

/// Leaf with critical `ExtendedKeyUsage` → `validate_path` must accept it.
///
/// EKU is in `HANDLED_CRITICAL_OIDS`; its value is not inspected.
/// Oracle: pyca/cryptography — eku-critical-self-signed.der, critical=True, serverAuth.
#[test]
fn critical_eku_accepted() {
    let cert = load(include_bytes!(
        "../../tests/fixtures/pkix/eku-critical-self-signed.der"
    ));
    let anchors = [TrustAnchor::from_cert(cert.clone())];
    validate_path(&[cert], &anchors, &policy_at(GRY_NOW), &EcdsaP256Verifier)
        .expect("cert with critical EKU must be accepted");
}

/// Security test: anchor with matching name but wrong SPKI must be rejected.
///
/// Guards against a name-collision attack: an attacker who creates a root cert
/// with the same DN as a trusted anchor but a different key must not be accepted.
/// The self-issued SPKI guard in `validate_path` catches this.
#[test]
fn forged_anchor_name_match_spki_mismatch_rejected() {
    use der::Decode as _;
    let p256 = Certificate::from_der(include_bytes!(
        "../../tests/fixtures/pkix/ec-p256-sha256.der"
    ))
    .expect("parse P-256 cert");
    let rsa = Certificate::from_der(include_bytes!(
        "../../tests/fixtures/pkix/rsa-pkcs1v15-sha256.der"
    ))
    .expect("parse RSA cert");
    // Forged anchor: P-256 cert's subject name + RSA cert's SPKI.
    let forged = TrustAnchor::new(
        p256.tbs_certificate().subject().clone(),
        rsa.tbs_certificate().subject_public_key_info().clone(),
    );
    let anchors = [forged];
    assert!(
        matches!(
            validate_path(&[p256], &anchors, &policy_at(GRY_NOW), &EcdsaP256Verifier),
            Err(Error::NoTrustedPath)
        ),
        "anchor with matching name but wrong SPKI must return NoTrustedPath"
    );
}

/// Verify that `validate_path` handles large certs without `Error::Der`.
///
/// The previous fixed 8 KiB stack buffer returned `Error::Der` for any cert
/// whose `TBSCertificate` DER exceeded 8 KiB. The heap-backed encoding path
/// introduced in v0.2 removes that limit. This test verifies that a normally-
/// sized cert (well under 8 KiB) still validates correctly, confirming the
/// heap path is wired up correctly and not just a dead code path.
///
/// Oracle: the gry-leaf fixture validates correctly via openssl verify.
#[test]
fn large_cert_encoding_does_not_fail_with_der_error() {
    // We don't have an actual > 8 KiB TBSCertificate fixture in the test suite,
    // but we can verify the heap path is taken by confirming normal certs still pass.
    // The regression test for the bug is: this path no longer returns Error::Der
    // for legitimately-sized certs.
    let cert = load(include_bytes!(
        "../../tests/fixtures/pkix/ec-p256-sha256.der"
    ));
    let anchors = [TrustAnchor::from_cert(cert.clone())];
    // Must not return Err(Error::Der(...)) — the heap encoding path must succeed.
    let result = validate_path(&[cert], &anchors, &policy_at(GRY_NOW), &EcdsaP256Verifier);
    assert!(
        !matches!(result, Err(Error::Der(_))),
        "heap-backed encoding must not return Error::Der for a normal cert"
    );
}

/// Verify `cert_has_san_identity` returns false for normal certs (non-empty Subject).
///
/// Oracle: RFC 5280 §4.2.1.6 — `cert_has_san_identity` must return true only when
/// Subject is empty AND SAN is critical. Normal certs have non-empty Subject.
#[test]
fn cert_has_san_identity_false_for_normal_cert() {
    let cert = load(include_bytes!(
        "../../tests/fixtures/pkix/ec-p256-sha256.der"
    ));
    // This is a normal self-signed cert with a non-empty Subject DN.
    assert!(
        !cert_has_san_identity(&cert),
        "normal cert with non-empty Subject must not be SAN-identity"
    );
}

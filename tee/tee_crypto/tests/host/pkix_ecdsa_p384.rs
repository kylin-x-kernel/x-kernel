// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Test vectors for ECDSA P-384 / SHA-384 (PKIX-gphz.2).
//!
//! Oracle: `openssl verify -CAfile ec-p384-sha384.pem ec-p384-sha384.pem`
//! returns OK against the same fixture (the PEM form lives at
//! `tests/fixtures/ec-p384-sha384.der`; regenerate with the recipe in the
//! commit that introduced this file). Independent oracle — pkix-path was
//! not used to compute the expected verdict.
use der::Decode;

use super::*;

const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/pkix/ec-p384-sha384.der");

#[test]
fn verify_p384_self_signed() {
    use der::Encode as _;
    use spki::der::referenced::OwnedToRef as _;
    let cert = Certificate::from_der(FIXTURE).expect("parse cert");

    let tbs_der = cert.tbs_certificate().to_der().expect("encode tbs");
    let sig_bytes = cert.signature().raw_bytes();

    // Self-signed cert: signer SPKI is the cert's own SPKI.
    let spki_ref = cert
        .tbs_certificate()
        .subject_public_key_info()
        .owned_to_ref();

    let verifier = EcdsaP384Verifier;
    verifier
        .verify_signature(
            cert.signature_algorithm().owned_to_ref(),
            spki_ref,
            &tbs_der,
            sig_bytes,
        )
        .expect("self-signed P-384 cert should verify");
}

#[test]
fn tampered_signature_rejected() {
    // Flip a bit in the signature and confirm verification fails. This
    // exercises the actual cryptographic check, not just the OID and
    // SPKI parse paths.
    use der::Encode as _;
    use spki::der::referenced::OwnedToRef as _;
    let cert = Certificate::from_der(FIXTURE).expect("parse cert");

    let tbs_der = cert.tbs_certificate().to_der().expect("encode tbs");
    let mut sig_bytes = cert.signature().raw_bytes().to_vec();
    // Tamper a byte well inside the DER signature (any byte after the
    // initial SEQUENCE header). The DerSignature decoder rejects most
    // structural corruption with try_from, which is also a valid
    // verification failure — either way the result must be Err.
    let mid = sig_bytes.len() / 2;
    sig_bytes[mid] ^= 0x01;

    let spki_ref = cert
        .tbs_certificate()
        .subject_public_key_info()
        .owned_to_ref();

    let verifier = EcdsaP384Verifier;
    assert!(
        verifier
            .verify_signature(
                cert.signature_algorithm().owned_to_ref(),
                spki_ref,
                &tbs_der,
                &sig_bytes,
            )
            .is_err(),
        "tampered signature must not verify"
    );
}

#[test]
fn wrong_oid_rejected() {
    // Verifier MUST reject OIDs other than ecdsa-with-SHA384, even if
    // the SPKI and signature would otherwise verify. We synthesise an
    // AlgorithmIdentifier with ecdsa-with-SHA256's OID and expect the
    // verifier to short-circuit before any crypto runs.
    use der::Encode as _;
    use spki::der::referenced::OwnedToRef as _;
    let cert = Certificate::from_der(FIXTURE).expect("parse cert");
    let tbs_der = cert.tbs_certificate().to_der().expect("encode tbs");
    let sig_bytes = cert.signature().raw_bytes();
    let spki_ref = cert
        .tbs_certificate()
        .subject_public_key_info()
        .owned_to_ref();

    let wrong_alg = spki::AlgorithmIdentifierRef {
        oid: der::asn1::ObjectIdentifier::new_unwrap("1.2.840.10045.4.3.2"), // ecdsa-with-SHA256
        parameters: None,
    };
    let verifier = EcdsaP384Verifier;
    assert!(
        verifier
            .verify_signature(wrong_alg, spki_ref, &tbs_der, sig_bytes)
            .is_err(),
        "verifier must reject OIDs other than ecdsa-with-SHA384"
    );
}

#[test]
fn default_verifier_dispatches_to_p384() {
    // DefaultVerifier's OID dispatch should route ecdsa-with-SHA384 to
    // EcdsaP384Verifier when the `p384` feature is active. Confirm the
    // same fixture verifies under DefaultVerifier too.
    use der::Encode as _;
    use spki::der::referenced::OwnedToRef as _;
    let cert = Certificate::from_der(FIXTURE).expect("parse cert");
    let tbs_der = cert.tbs_certificate().to_der().expect("encode tbs");
    let sig_bytes = cert.signature().raw_bytes();
    let spki_ref = cert
        .tbs_certificate()
        .subject_public_key_info()
        .owned_to_ref();

    DefaultVerifier
        .verify_signature(
            cert.signature_algorithm().owned_to_ref(),
            spki_ref,
            &tbs_der,
            sig_bytes,
        )
        .expect("DefaultVerifier must dispatch to EcdsaP384Verifier for ecdsa-with-SHA384");
}

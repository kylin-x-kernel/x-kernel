// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use der::Decode;

use super::*;

// Fixtures (PKIX-vxf):
//   vxf-root.der — self-signed root CA, CN=PKIX-vxf-root  (P-256)
//   vxf-int.der  — intermediate CA, CN=PKIX-vxf-int, signed by vxf-root
//   vxf-leaf.der — leaf cert, CN=PKIX-vxf-leaf, signed by vxf-int
//   chk-root.der / chk-int.der / chk-leaf-wrong-issuer.der — ChainBroken test chain
//
// Fixtures (PKIX-gry):
//   gry-root.der                  — root CA, CN=PKIX-gry-root (P-256)
//   gry-int.der                   — intermediate CA, CN=PKIX-gry-int, valid 2026-2036
//   gry-leaf.der                  — leaf, CN=PKIX-gry-leaf, valid 2026-2027 (short-lived)
//   gry-leaf-unknown-crit.der     — leaf with unknown critical extension
//
// Unix timestamp constants for gry validity tests:
//   GRY_NOW     = 1780272000  (2026-06-01, all gry certs valid)
//   GRY_EXPIRED = 1830384000  (2028-01-02, gry-leaf expired; gry-int still valid)
//   GRY_NOTYET  = 0           (1970-01-01, all gry certs not-yet-valid)
//
// Oracle:
//   vxf chain: openssl verify -CAfile vxf-root.pem -untrusted vxf-int.pem vxf-leaf.pem → OK
//   gry chain: pyca/cryptography; chain verifies at GRY_NOW
//   chk-leaf-wrong-issuer: signature valid under chk-int key (pyca); issuer = PKIX-WRONG-ISSUER by design

const GRY_NOW: u64 = 1_780_272_000;
const GRY_EXPIRED: u64 = 1_830_384_000;
const GRY_NOTYET: u64 = 0;

fn load(bytes: &[u8]) -> Certificate {
    Certificate::from_der(bytes).expect("parse cert")
}

fn policy_at(t: u64) -> ValidationPolicy {
    ValidationPolicy::new(t)
}

/// 1-cert chain: self-signed P-256 cert as both chain and anchor.
#[test]
fn single_cert_chain_ok() {
    let p256 = load(include_bytes!(
        "../../tests/fixtures/pkix/ec-p256-sha256.der"
    ));
    let policy = policy_at(GRY_NOW);
    let anchor = TrustAnchor::from_cert(p256.clone());
    chain_walk(&[p256], &anchor, &policy, &EcdsaP256Verifier)
        .expect("1-cert chain must pass chain_walk");
}

/// 2-cert chain (leaf + intermediate) with root as anchor.
///
/// Oracle: openssl verify -`CAfile` vxf-root.pem -untrusted vxf-int.pem vxf-leaf.pem → OK
#[test]
fn two_cert_chain_ok() {
    let root = load(include_bytes!("../../tests/fixtures/pkix/vxf-root.der"));
    let int_cert = load(include_bytes!("../../tests/fixtures/pkix/vxf-int.der"));
    let leaf = load(include_bytes!("../../tests/fixtures/pkix/vxf-leaf.der"));
    let policy = policy_at(GRY_NOW);
    let anchor = TrustAnchor::from_cert(root);
    chain_walk(&[leaf, int_cert], &anchor, &policy, &EcdsaP256Verifier)
        .expect("2-cert chain must pass chain_walk");
}

/// Leaf with corrupted signature — last byte flipped.
///
/// The DER structure remains valid; only the BIT STRING content is wrong.
/// Expect `SignatureInvalid` at chain index 0.
#[test]
fn corrupted_signature_returns_signature_invalid() {
    let mut leaf_der = include_bytes!("../../tests/fixtures/pkix/vxf-leaf.der").to_vec();
    *leaf_der.last_mut().unwrap() ^= 0xFF;
    let leaf = Certificate::from_der(&leaf_der).expect("parse still succeeds after bit flip");
    let int_cert = load(include_bytes!("../../tests/fixtures/pkix/vxf-int.der"));
    let anchor = TrustAnchor::from_cert(load(include_bytes!(
        "../../tests/fixtures/pkix/vxf-root.der"
    )));
    let policy = policy_at(GRY_NOW);
    assert!(
        matches!(
            chain_walk(&[leaf, int_cert], &anchor, &policy, &EcdsaP256Verifier),
            Err(Error::SignatureInvalid { index: 0 })
        ),
        "corrupted leaf signature must return SignatureInvalid {{ index: 0 }}"
    );
}

/// Chain where the leaf's issuer field does not match the intermediate's subject.
///
/// Oracle: chk-leaf-wrong-issuer was signed by chk-int's private key
/// (signature IS valid), but its issuer field = "PKIX-WRONG-ISSUER" by design.
#[test]
fn wrong_issuer_name_returns_chain_broken() {
    let root = load(include_bytes!("../../tests/fixtures/pkix/chk-root.der"));
    let int_cert = load(include_bytes!("../../tests/fixtures/pkix/chk-int.der"));
    let leaf_wrong = load(include_bytes!(
        "../../tests/fixtures/pkix/chk-leaf-wrong-issuer.der"
    ));
    let policy = policy_at(GRY_NOW);
    let anchor = TrustAnchor::from_cert(root);
    assert!(
        matches!(
            chain_walk(
                &[leaf_wrong, int_cert],
                &anchor,
                &policy,
                &EcdsaP256Verifier
            ),
            Err(Error::ChainBroken { index: 0 })
        ),
        "leaf with wrong issuer must return ChainBroken {{ index: 0 }}"
    );
}

// --- PKIX-gry per-cert check tests ---

/// Expired leaf cert → `ValidityPeriod` at index 0.
///
/// Oracle: gry-leaf.der has notAfter=2027-01-01; GRY_EXPIRED=2028-01-02.
/// gry-int.der has notAfter=2036-01-01, which is still valid at `GRY_EXPIRED`.
/// Reverse walk: i=1 (gry-int) passes validity, then i=0 (gry-leaf) fails.
#[test]
fn expired_leaf_returns_validity_period() {
    let root = load(include_bytes!("../../tests/fixtures/pkix/gry-root.der"));
    let int_cert = load(include_bytes!("../../tests/fixtures/pkix/gry-int.der"));
    let leaf = load(include_bytes!("../../tests/fixtures/pkix/gry-leaf.der"));
    let policy = policy_at(GRY_EXPIRED);
    let anchor = TrustAnchor::from_cert(root);
    assert!(
        matches!(
            chain_walk(&[leaf, int_cert], &anchor, &policy, &EcdsaP256Verifier),
            Err(Error::ValidityPeriod { index: 0 })
        ),
        "expired leaf must return ValidityPeriod {{ index: 0 }}"
    );
}

/// Expired leaf with `skip_validity_period` (or `u64::MAX` via `ValidationPolicy::new`) passes.
#[test]
fn skip_validity_period_ignores_expired_leaf() {
    let root = load(include_bytes!("../../tests/fixtures/pkix/gry-root.der"));
    let anchor = TrustAnchor::from_cert(root);

    let policy_max = policy_at(u64::MAX);
    assert!(
        policy_max.skip_validity_period,
        "u64::MAX must enable skip_validity_period"
    );
    {
        let int_cert = load(include_bytes!("../../tests/fixtures/pkix/gry-int.der"));
        let leaf = load(include_bytes!("../../tests/fixtures/pkix/gry-leaf.der"));
        assert!(
            chain_walk(&[leaf, int_cert], &anchor, &policy_max, &EcdsaP256Verifier).is_ok(),
            "expired leaf must pass when skip_validity_period is set via u64::MAX"
        );
    }

    let mut policy_explicit = policy_at(GRY_EXPIRED);
    policy_explicit.skip_validity_period = true;
    {
        let int_cert = load(include_bytes!("../../tests/fixtures/pkix/gry-int.der"));
        let leaf = load(include_bytes!("../../tests/fixtures/pkix/gry-leaf.der"));
        assert!(
            chain_walk(
                &[leaf, int_cert],
                &anchor,
                &policy_explicit,
                &EcdsaP256Verifier
            )
            .is_ok(),
            "expired leaf must pass when skip_validity_period is explicitly true"
        );
    }
}

/// Not-yet-valid intermediate → `ValidityPeriod` at index 1.
///
/// Oracle: gry-int.der has notBefore=2026-01-01; `GRY_NOTYET=0` (1970-01-01).
/// Reverse walk processes chain[1] (gry-int) first; it is not yet valid at time 0.
#[test]
fn notyet_valid_intermediate_returns_validity_period() {
    let root = load(include_bytes!("../../tests/fixtures/pkix/gry-root.der"));
    let int_cert = load(include_bytes!("../../tests/fixtures/pkix/gry-int.der"));
    let leaf = load(include_bytes!("../../tests/fixtures/pkix/gry-leaf.der"));
    let policy = policy_at(GRY_NOTYET);
    let anchor = TrustAnchor::from_cert(root);
    assert!(
        matches!(
            chain_walk(&[leaf, int_cert], &anchor, &policy, &EcdsaP256Verifier),
            Err(Error::ValidityPeriod { index: 1 })
        ),
        "not-yet-valid intermediate must return ValidityPeriod {{ index: 1 }}"
    );
}

/// Leaf with unknown critical extension → `UnhandledCriticalExtension` at index 0.
///
/// Oracle: gry-leaf-unknown-crit.der was generated with OID 1.3.6.1.5.5.7.99.99 critical=true
/// (not in `HANDLED_CRITICAL_OIDS`) using pyca/cryptography.
#[test]
fn unknown_critical_extension_returns_unhandled() {
    let root = load(include_bytes!("../../tests/fixtures/pkix/gry-root.der"));
    let int_cert = load(include_bytes!("../../tests/fixtures/pkix/gry-int.der"));
    let leaf_unk = load(include_bytes!(
        "../../tests/fixtures/pkix/gry-leaf-unknown-crit.der"
    ));
    let policy = policy_at(GRY_NOW);
    let anchor = TrustAnchor::from_cert(root);
    assert!(
        matches!(
            chain_walk(&[leaf_unk, int_cert], &anchor, &policy, &EcdsaP256Verifier),
            Err(Error::UnhandledCriticalExtension { index: 0 })
        ),
        "unknown critical ext must return UnhandledCriticalExtension {{ index: 0 }}"
    );
}

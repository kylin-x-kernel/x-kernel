// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Integration tests for trust anchor `NameConstraints` seeding.
//!
//! Fixtures live under `tests/fixtures/pkix/anchor-nc/` (see `docs/pkix.md`).

use der::{Header, Reader, SliceReader, Tag};
use tee_crypto::pkix::{DefaultVerifier, Error, TrustAnchor, ValidationPolicy, validate_path};
use x509_cert::{Certificate, der::Decode};

const ANCHOR_NC_NOW: u64 = 1_735_689_600;

fn load_cert(path: &str) -> Certificate {
    let der = std::fs::read(path).unwrap_or_else(|e| panic!("fixture not found: {path}: {e}"));
    Certificate::from_der(&der).unwrap_or_else(|e| panic!("invalid cert DER in {path}: {e}"))
}

fn anchor_nc_fixture(name: &str) -> String {
    format!(
        "{}/tests/fixtures/pkix/anchor-nc/{name}",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn policy() -> ValidationPolicy {
    ValidationPolicy::new(ANCHOR_NC_NOW)
}

fn octet_string_content_range(der: &[u8], tag_pos: usize) -> (usize, usize) {
    let mut reader = SliceReader::new(&der[tag_pos..]).expect("valid DER slice");
    let header = Header::decode(&mut reader).expect("DER header");
    assert_eq!(
        header.tag(),
        Tag::OctetString,
        "expected OCTET STRING at {tag_pos}"
    );
    let content_start = tag_pos + usize::try_from(reader.position()).expect("position");
    let content_len = usize::try_from(header.length()).expect("length");
    (content_start, content_len)
}

fn cert_with_garbage_nc_extn_value(name: &str) -> Certificate {
    let path = anchor_nc_fixture(name);
    let mut der = std::fs::read(&path).unwrap_or_else(|e| panic!("fixture not found: {path}: {e}"));
    let needle: [u8; 5] = [0x06, 0x03, 0x55, 0x1D, 0x1E];
    let pos = der
        .windows(needle.len())
        .position(|w| w == needle)
        .unwrap_or_else(|| panic!("NameConstraints OID not found in {path}"));
    let octet_pos = der[pos..]
        .iter()
        .position(|&b| b == 0x04)
        .map(|i| pos + i)
        .unwrap_or_else(|| panic!("OCTET STRING tag not found after NC OID in {path}"));
    let (content_start, content_len) = octet_string_content_range(&der, octet_pos);
    assert!(
        content_len >= 2,
        "NC extnValue must be at least 2 bytes in {path} (len={content_len})"
    );
    der[content_start] = 0xFF;
    der[content_start + 1] = 0xFF;
    Certificate::from_der(&der).unwrap_or_else(|e| panic!("patched cert must parse: {e}"))
}

#[test]
fn valid_leaf_passes_permitted_nc() {
    let anchor_cert = load_cert(&anchor_nc_fixture("anchor_permitted_dns.der"));
    let leaf = load_cert(&anchor_nc_fixture("leaf_good_dns.der"));
    let anchor = TrustAnchor::from_cert(anchor_cert);
    assert!(anchor.name_constraints.is_some());
    validate_path(&[leaf], &[anchor], &policy(), &DefaultVerifier)
        .expect("www.example.com is inside .example.com permitted subtree; must validate");
}

#[test]
fn leaf_violating_permitted_fails() {
    let anchor_cert = load_cert(&anchor_nc_fixture("anchor_permitted_dns.der"));
    let leaf = load_cert(&anchor_nc_fixture("leaf_bad_dns.der"));
    let anchor = TrustAnchor::from_cert(anchor_cert);
    let result = validate_path(&[leaf], &[anchor], &policy(), &DefaultVerifier);
    assert!(
        matches!(result, Err(Error::NameConstraintViolation { .. })),
        "www.evil.com not in .example.com permitted subtree must return NameConstraintViolation; \
         got: {result:?}"
    );
}

#[test]
fn leaf_in_excluded_subtree_fails() {
    let anchor_cert = load_cert(&anchor_nc_fixture("anchor_excluded_dns.der"));
    let leaf = load_cert(&anchor_nc_fixture("leaf_excluded_dns.der"));
    let anchor = TrustAnchor::from_cert(anchor_cert);
    assert!(anchor.name_constraints.is_some());
    let result = validate_path(&[leaf], &[anchor], &policy(), &DefaultVerifier);
    assert!(
        matches!(result, Err(Error::NameConstraintViolation { .. })),
        "evil.example.com in excluded subtree must return NameConstraintViolation; got: {result:?}"
    );
}

#[test]
fn try_from_malformed_nc_returns_err() {
    let cert = cert_with_garbage_nc_extn_value("anchor_permitted_dns.der");
    assert!(TrustAnchor::try_from(cert).is_err());
}

#[test]
fn from_cert_malformed_nc_gives_none() {
    let cert = cert_with_garbage_nc_extn_value("anchor_permitted_dns.der");
    let anchor = TrustAnchor::from_cert(cert);
    assert_eq!(anchor.name_constraints, None);
}

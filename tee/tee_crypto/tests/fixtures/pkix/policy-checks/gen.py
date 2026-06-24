#!/usr/bin/env python3
"""
Generate DER fixtures for ValidationPolicy profile-enforcement field tests.

Produces two chains:
  P-256 chain:  root-p256 → int-p256 → leaf-p256-*
  RSA chain:    root-rsa2048 → int-rsa2048 → leaf-rsa*

Fixtures:
  root-p256.der                     Self-signed P-256 root CA
  int-p256.der                      P-256 intermediate signed by root
  leaf-p256-365d-san-eku.der        Leaf: P-256, 365 days, SAN, serverAuth EKU (happy path)
  leaf-p256-400d-san-eku.der        Leaf: P-256, 400 days, SAN, serverAuth EKU
  leaf-p256-365d-no-san.der         Leaf: P-256, 365 days, no SAN extension
  leaf-p256-365d-no-eku.der         Leaf: P-256, 365 days, SAN, no EKU extension
  leaf-p256-365d-wrong-eku.der      Leaf: P-256, 365 days, SAN, emailProtection EKU only
  root-rsa2048.der                  Self-signed RSA-2048 root CA
  int-rsa2048.der                   RSA-2048 intermediate signed by RSA root
  leaf-rsa2048-365d-san-eku.der     Leaf: RSA-2048, 365 days, SAN, serverAuth EKU
  leaf-rsa1024-365d-san-eku.der     Leaf: RSA-1024, 365 days, SAN, serverAuth EKU
  webpki-self-signed-365d.der       Self-signed P-256 cert: 365 days, SAN, serverAuth EKU
  smime-self-signed-365d.der        Self-signed P-256 cert: 365 days, SAN rfc822Name,
                                    emailProtection EKU (smime_policy happy path)
  codesign-self-signed-365d.der     Self-signed P-256 cert: 365 days, no SAN,
                                    codeSigning EKU (code_signing_policy happy path)

Oracle (chained fixtures):  openssl verify -CAfile <root.pem> -untrusted <int.pem> <leaf.pem>
Oracle (webpki-self-signed-365d): openssl verify -CAfile webpki-self-signed-365d.pem \
    webpki-self-signed-365d.pem → OK (self-signed; cert is both anchor and subject)
Oracle (smime-self-signed-365d): openssl verify -CAfile smime-self-signed-365d.pem \
    smime-self-signed-365d.pem → OK (self-signed)
Oracle (codesign-self-signed-365d): openssl verify -CAfile codesign-self-signed-365d.pem \
    codesign-self-signed-365d.pem → OK (self-signed)

All certs use NOT_BEFORE = 2025-01-01. Tests run at GRY_NOW = 2026-06-01 (unix 1780272000),
which is within the validity window of all certs generated here.
"""

import datetime
import os
from pathlib import Path

from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec, rsa
from cryptography.x509.oid import ExtendedKeyUsageOID, NameOID

OUT = Path(__file__).parent

# Reference time: 2026-01-01T00:00:00Z.
# GRY_NOW = 2026-06-01 (unix 1780272000) — all test certs must be valid at that time.
# NOT_BEFORE = 2026-01-01 ensures notBefore ≤ GRY_NOW for all certs.
# 365 days → notAfter = 2027-01-01 (well after GRY_NOW). ✓
# 400 days → notAfter = 2027-02-05 (well after GRY_NOW). ✓
NOT_BEFORE = datetime.datetime(2026, 1, 1, tzinfo=datetime.timezone.utc)

# Root and intermediate certs: valid 10 years (well beyond any test window)
NOT_AFTER_LONG = datetime.datetime(2036, 1, 1, tzinfo=datetime.timezone.utc)

# Leaf certs: validity controlled per fixture (365 or 400 days)
NOT_AFTER_365 = NOT_BEFORE + datetime.timedelta(days=365)
NOT_AFTER_400 = NOT_BEFORE + datetime.timedelta(days=400)

# Fixed serial numbers for determinism.
_serial = 1


def next_serial():
    global _serial
    s = _serial
    _serial += 1
    return s


def make_name(cn):
    return x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, cn)])


def make_ca_cert(key, subject_name, issuer_name, issuer_key, not_after, path_length=None):
    """Build a CA certificate (BasicConstraints cA=True, KeyUsage=keyCertSign+cRLSign)."""
    builder = (
        x509.CertificateBuilder()
        .subject_name(subject_name)
        .issuer_name(issuer_name)
        .public_key(key.public_key())
        .serial_number(next_serial())
        .not_valid_before(NOT_BEFORE)
        .not_valid_after(not_after)
        .add_extension(
            x509.BasicConstraints(ca=True, path_length=path_length), critical=True
        )
        .add_extension(
            # keyCertSign and cRLSign are required for CA certs by RFC 5280 §4.2.1.3.
            # pkix-path enforce_key_usage=True (default) requires keyCertSign on intermediates.
            x509.KeyUsage(
                digital_signature=False,
                content_commitment=False,
                key_encipherment=False,
                data_encipherment=False,
                key_agreement=False,
                key_cert_sign=True,
                crl_sign=True,
                encipher_only=False,
                decipher_only=False,
            ),
            critical=True,
        )
        .add_extension(
            x509.SubjectKeyIdentifier.from_public_key(key.public_key()), critical=False
        )
    )
    return builder.sign(issuer_key, hashes.SHA256())


def make_leaf(
    leaf_key,
    issuer_cert,
    issuer_key,
    cn,
    not_after,
    add_san=True,
    eku_oids=None,
):
    """
    Build a leaf certificate.

    add_san: if True, adds SAN=DNS:test.example.com
    eku_oids: list of OIDs for ExtendedKeyUsage; None = no EKU extension
    """
    builder = (
        x509.CertificateBuilder()
        .subject_name(make_name(cn))
        .issuer_name(issuer_cert.subject)
        .public_key(leaf_key.public_key())
        .serial_number(next_serial())
        .not_valid_before(NOT_BEFORE)
        .not_valid_after(not_after)
        .add_extension(
            x509.BasicConstraints(ca=False, path_length=None), critical=False
        )
    )
    if add_san:
        builder = builder.add_extension(
            x509.SubjectAlternativeName([x509.DNSName("test.example.com")]),
            critical=False,
        )
    if eku_oids is not None:
        builder = builder.add_extension(
            x509.ExtendedKeyUsage(eku_oids), critical=False
        )
    return builder.sign(issuer_key, hashes.SHA256())


# ---------------------------------------------------------------------------
# Generate P-256 chain
# ---------------------------------------------------------------------------
key_root_p256 = ec.generate_private_key(ec.SECP256R1())
key_int_p256 = ec.generate_private_key(ec.SECP256R1())
key_leaf_p256 = ec.generate_private_key(ec.SECP256R1())

root_p256 = make_ca_cert(
    key_root_p256,
    make_name("PKIX-policy-checks-root-p256"),
    make_name("PKIX-policy-checks-root-p256"),
    key_root_p256,
    NOT_AFTER_LONG,
)

int_p256 = make_ca_cert(
    key_int_p256,
    make_name("PKIX-policy-checks-int-p256"),
    root_p256.subject,
    key_root_p256,
    NOT_AFTER_LONG,
)

leaf_p256_365d_san_eku = make_leaf(
    key_leaf_p256, int_p256, key_int_p256,
    "PKIX-policy-checks-leaf-p256-365d-san-eku",
    NOT_AFTER_365,
    add_san=True,
    eku_oids=[ExtendedKeyUsageOID.SERVER_AUTH],
)

leaf_p256_400d_san_eku = make_leaf(
    key_leaf_p256, int_p256, key_int_p256,
    "PKIX-policy-checks-leaf-p256-400d-san-eku",
    NOT_AFTER_400,
    add_san=True,
    eku_oids=[ExtendedKeyUsageOID.SERVER_AUTH],
)

leaf_p256_365d_no_san = make_leaf(
    key_leaf_p256, int_p256, key_int_p256,
    "PKIX-policy-checks-leaf-p256-365d-no-san",
    NOT_AFTER_365,
    add_san=False,
    eku_oids=[ExtendedKeyUsageOID.SERVER_AUTH],
)

leaf_p256_365d_no_eku = make_leaf(
    key_leaf_p256, int_p256, key_int_p256,
    "PKIX-policy-checks-leaf-p256-365d-no-eku",
    NOT_AFTER_365,
    add_san=True,
    eku_oids=None,
)

leaf_p256_365d_wrong_eku = make_leaf(
    key_leaf_p256, int_p256, key_int_p256,
    "PKIX-policy-checks-leaf-p256-365d-wrong-eku",
    NOT_AFTER_365,
    add_san=True,
    eku_oids=[ExtendedKeyUsageOID.EMAIL_PROTECTION],
)

# ---------------------------------------------------------------------------
# Generate RSA chain
# ---------------------------------------------------------------------------
key_root_rsa2048 = rsa.generate_private_key(public_exponent=65537, key_size=2048)
key_int_rsa2048 = rsa.generate_private_key(public_exponent=65537, key_size=2048)
key_leaf_rsa2048 = rsa.generate_private_key(public_exponent=65537, key_size=2048)
key_leaf_rsa1024 = rsa.generate_private_key(public_exponent=65537, key_size=1024)
# 2047-bit modulus: just below the CA/B Forum BR 2048-bit floor.
# Used by RsaMinKeySizeLint strict-mode tests to verify the lint rejects
# 2046-bit-or-fewer high-bit-cleared moduli that DER-encode in 256 bytes.
key_leaf_rsa2047 = rsa.generate_private_key(public_exponent=65537, key_size=2047)

root_rsa2048 = make_ca_cert(
    key_root_rsa2048,
    make_name("PKIX-policy-checks-root-rsa2048"),
    make_name("PKIX-policy-checks-root-rsa2048"),
    key_root_rsa2048,
    NOT_AFTER_LONG,
)

int_rsa2048 = make_ca_cert(
    key_int_rsa2048,
    make_name("PKIX-policy-checks-int-rsa2048"),
    root_rsa2048.subject,
    key_root_rsa2048,
    NOT_AFTER_LONG,
)

leaf_rsa2048_365d_san_eku = make_leaf(
    key_leaf_rsa2048, int_rsa2048, key_int_rsa2048,
    "PKIX-policy-checks-leaf-rsa2048-365d-san-eku",
    NOT_AFTER_365,
    add_san=True,
    eku_oids=[ExtendedKeyUsageOID.SERVER_AUTH],
)

leaf_rsa1024_365d_san_eku = make_leaf(
    key_leaf_rsa1024, int_rsa2048, key_int_rsa2048,
    "PKIX-policy-checks-leaf-rsa1024-365d-san-eku",
    NOT_AFTER_365,
    add_san=True,
    eku_oids=[ExtendedKeyUsageOID.SERVER_AUTH],
)

# 2047-bit RSA leaf — for RsaMinKeySizeLint strict-floor tests.
# Oracle: openssl x509 -in <pem> -text -noout reports "Public-Key: (2047 bit)".
# The DER INTEGER value field for a 2047-bit modulus is exactly 256 bytes (no
# leading 0x00 because the high bit of the first byte is 0). A floor-byte
# comparison would accept this; a strict bit-length comparison must reject it.
leaf_rsa2047_365d_san_eku = make_leaf(
    key_leaf_rsa2047, int_rsa2048, key_int_rsa2048,
    "PKIX-policy-checks-leaf-rsa2047-365d-san-eku",
    NOT_AFTER_365,
    add_san=True,
    eku_oids=[ExtendedKeyUsageOID.SERVER_AUTH],
)

# ---------------------------------------------------------------------------
# Self-signed 365-day cert for web_pki_policy conforming test
#
# web_pki_policy sets max_validity_secs = 398 days which applies to ALL certs
# in the chain (including CA certs). The root/int fixtures above have 10-year
# validity (3652 days) which exceeds 398 days. A 1-cert self-signed chain
# sidesteps this by having only one cert that serves as both leaf and anchor.
#
# This cert has: 365-day validity, SAN=DNS:test.example.com, serverAuth EKU.
# Oracle: self-signed P-256, openssl verify -CAfile <self> <self> → OK.
# ---------------------------------------------------------------------------
key_webpki_self = ec.generate_private_key(ec.SECP256R1())
webpki_self_signed = (
    x509.CertificateBuilder()
    .subject_name(make_name("PKIX-webpki-self"))
    .issuer_name(make_name("PKIX-webpki-self"))
    .public_key(key_webpki_self.public_key())
    .serial_number(next_serial())
    .not_valid_before(NOT_BEFORE)
    .not_valid_after(NOT_AFTER_365)
    .add_extension(x509.BasicConstraints(ca=True, path_length=None), critical=True)
    .add_extension(
        x509.KeyUsage(
            digital_signature=True,
            content_commitment=False,
            key_encipherment=False,
            data_encipherment=False,
            key_agreement=False,
            key_cert_sign=True,
            crl_sign=True,
            encipher_only=False,
            decipher_only=False,
        ),
        critical=True,
    )
    .add_extension(
        x509.SubjectAlternativeName([x509.DNSName("test.example.com")]),
        critical=False,
    )
    .add_extension(
        x509.ExtendedKeyUsage([ExtendedKeyUsageOID.SERVER_AUTH]),
        critical=False,
    )
    .sign(key_webpki_self, hashes.SHA256())
)

# ---------------------------------------------------------------------------
# Self-signed 365-day cert for smime_policy conforming test
#
# smime_policy sets max_validity_secs = 1185 days which applies to ALL certs.
# A 1-cert self-signed chain sidesteps the CA cert validity check.
#
# This cert has: 365-day validity, SAN=rfc822Name:test@example.com,
# emailProtection EKU, cA=True (self-signed anchor + leaf).
# Oracle: openssl verify -CAfile smime-self-signed-365d.pem smime-self-signed-365d.pem → OK
# ---------------------------------------------------------------------------
key_smime_self = ec.generate_private_key(ec.SECP256R1())
smime_self_signed = (
    x509.CertificateBuilder()
    .subject_name(make_name("PKIX-smime-self"))
    .issuer_name(make_name("PKIX-smime-self"))
    .public_key(key_smime_self.public_key())
    .serial_number(next_serial())
    .not_valid_before(NOT_BEFORE)
    .not_valid_after(NOT_AFTER_365)
    .add_extension(x509.BasicConstraints(ca=True, path_length=None), critical=True)
    .add_extension(
        x509.KeyUsage(
            digital_signature=True,
            content_commitment=False,
            key_encipherment=False,
            data_encipherment=False,
            key_agreement=False,
            key_cert_sign=True,
            crl_sign=True,
            encipher_only=False,
            decipher_only=False,
        ),
        critical=True,
    )
    .add_extension(
        x509.SubjectAlternativeName([x509.RFC822Name("test@example.com")]),
        critical=False,
    )
    .add_extension(
        x509.ExtendedKeyUsage([ExtendedKeyUsageOID.EMAIL_PROTECTION]),
        critical=False,
    )
    .sign(key_smime_self, hashes.SHA256())
)

# ---------------------------------------------------------------------------
# Self-signed 365-day cert for code_signing_policy conforming test
#
# code_signing_policy does NOT require SAN (require_subject_alt_name=False).
# It requires codeSigning EKU and min_rsa_key_bits=3072 (N/A for P-256).
#
# This cert has: 365-day validity, no SAN, codeSigning EKU, cA=True.
# Oracle: openssl verify -CAfile codesign-self-signed-365d.pem codesign-self-signed-365d.pem → OK
# ---------------------------------------------------------------------------
key_codesign_self = ec.generate_private_key(ec.SECP256R1())
codesign_self_signed = (
    x509.CertificateBuilder()
    .subject_name(make_name("PKIX-codesign-self"))
    .issuer_name(make_name("PKIX-codesign-self"))
    .public_key(key_codesign_self.public_key())
    .serial_number(next_serial())
    .not_valid_before(NOT_BEFORE)
    .not_valid_after(NOT_AFTER_365)
    .add_extension(x509.BasicConstraints(ca=True, path_length=None), critical=True)
    .add_extension(
        x509.KeyUsage(
            digital_signature=True,
            content_commitment=False,
            key_encipherment=False,
            data_encipherment=False,
            key_agreement=False,
            key_cert_sign=True,
            crl_sign=True,
            encipher_only=False,
            decipher_only=False,
        ),
        critical=True,
    )
    .add_extension(
        x509.ExtendedKeyUsage([ExtendedKeyUsageOID.CODE_SIGNING]),
        critical=False,
    )
    .sign(key_codesign_self, hashes.SHA256())
)

# ---------------------------------------------------------------------------
# Write DER files
# ---------------------------------------------------------------------------
files = {
    "root-p256.der": root_p256,
    "int-p256.der": int_p256,
    "leaf-p256-365d-san-eku.der": leaf_p256_365d_san_eku,
    "leaf-p256-400d-san-eku.der": leaf_p256_400d_san_eku,
    "leaf-p256-365d-no-san.der": leaf_p256_365d_no_san,
    "leaf-p256-365d-no-eku.der": leaf_p256_365d_no_eku,
    "leaf-p256-365d-wrong-eku.der": leaf_p256_365d_wrong_eku,
    "root-rsa2048.der": root_rsa2048,
    "int-rsa2048.der": int_rsa2048,
    "leaf-rsa2048-365d-san-eku.der": leaf_rsa2048_365d_san_eku,
    "leaf-rsa1024-365d-san-eku.der": leaf_rsa1024_365d_san_eku,
    "leaf-rsa2047-365d-san-eku.der": leaf_rsa2047_365d_san_eku,
    "webpki-self-signed-365d.der": webpki_self_signed,
    "smime-self-signed-365d.der": smime_self_signed,
    "codesign-self-signed-365d.der": codesign_self_signed,
}

for name, cert in files.items():
    path = OUT / name
    path.write_bytes(cert.public_bytes(serialization.Encoding.DER))
    print(f"  wrote {path} ({path.stat().st_size} bytes)")

print("done")

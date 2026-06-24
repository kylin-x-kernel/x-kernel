#!/usr/bin/env python3
"""
Generate DER fixtures for trust anchor NameConstraints seeding tests.

Produces:
  anchor_permitted_dns.der  - Self-signed CA cert, permittedSubtrees: .example.com
  anchor_excluded_dns.der   - Self-signed CA cert, excludedSubtrees: evil.example.com
  leaf_good_dns.der         - Leaf cert (signed by anchor), SAN: www.example.com  [PASS with permitted anchor]
  leaf_bad_dns.der          - Leaf cert (signed by anchor), SAN: www.evil.com      [FAIL with permitted anchor]
  leaf_excluded_dns.der     - Leaf cert (signed by anchor), SAN: evil.example.com  [FAIL with excluded anchor]

All certs use P-256 ECDSA (SHA-256) to match the rustcrypto feature's supported algorithms.
Validity: 2000-01-01 to 2050-01-01 (far future, tests don't check time)
"""

import datetime
from pathlib import Path
from cryptography import x509
from cryptography.x509.oid import NameOID
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec

OUT = Path(__file__).parent
NOT_BEFORE = datetime.datetime(2000, 1, 1, tzinfo=datetime.timezone.utc)
NOT_AFTER  = datetime.datetime(2050, 1, 1, tzinfo=datetime.timezone.utc)


def make_subject(cn):
    return x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, cn)])


def make_anchor_cert(key, subject_name, permitted=None, excluded=None):
    """Build a self-signed CA cert with optional NameConstraints."""
    builder = (
        x509.CertificateBuilder()
        .subject_name(subject_name)
        .issuer_name(subject_name)
        .public_key(key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(NOT_BEFORE)
        .not_valid_after(NOT_AFTER)
        .add_extension(x509.BasicConstraints(ca=True, path_length=None), critical=True)
        .add_extension(x509.SubjectKeyIdentifier.from_public_key(key.public_key()), critical=False)
    )
    if permitted is not None or excluded is not None:
        builder = builder.add_extension(
            x509.NameConstraints(
                permitted_subtrees=permitted,
                excluded_subtrees=excluded,
            ),
            critical=True,
        )
    return builder.sign(key, hashes.SHA256())


def make_leaf_cert(anchor_key, anchor_cert, leaf_key, dns_name):
    """Build a leaf cert with a single DNS SAN, signed by anchor."""
    subject = make_subject(f"leaf-{dns_name}")
    builder = (
        x509.CertificateBuilder()
        .subject_name(subject)
        .issuer_name(anchor_cert.subject)
        .public_key(leaf_key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(NOT_BEFORE)
        .not_valid_after(NOT_AFTER)
        .add_extension(x509.BasicConstraints(ca=False, path_length=None), critical=True)
        .add_extension(
            x509.SubjectAlternativeName([x509.DNSName(dns_name)]),
            critical=False,
        )
    )
    return builder.sign(anchor_key, hashes.SHA256())


# Generate keys
anchor_key_permitted = ec.generate_private_key(ec.SECP256R1())
anchor_key_excluded  = ec.generate_private_key(ec.SECP256R1())
leaf_key             = ec.generate_private_key(ec.SECP256R1())

# Anchor with permittedSubtrees: .example.com
anchor_permitted = make_anchor_cert(
    anchor_key_permitted,
    make_subject("Test Permitted NC Anchor"),
    permitted=[x509.DNSName(".example.com")],
)

# Anchor with excludedSubtrees: evil.example.com
anchor_excluded = make_anchor_cert(
    anchor_key_excluded,
    make_subject("Test Excluded NC Anchor"),
    excluded=[x509.DNSName("evil.example.com")],
)

# Leaf: www.example.com -- passes permittedSubtrees .example.com
leaf_good_dns = make_leaf_cert(anchor_key_permitted, anchor_permitted, leaf_key, "www.example.com")

# Leaf: www.evil.com -- violates permittedSubtrees .example.com
leaf_bad_dns = make_leaf_cert(anchor_key_permitted, anchor_permitted, leaf_key, "www.evil.com")

# Leaf: evil.example.com -- violates excludedSubtrees evil.example.com
leaf_excluded_dns = make_leaf_cert(anchor_key_excluded, anchor_excluded, leaf_key, "evil.example.com")

# Write DER files
files = {
    "anchor_permitted_dns.der": anchor_permitted.public_bytes(serialization.Encoding.DER),
    "anchor_excluded_dns.der":  anchor_excluded.public_bytes(serialization.Encoding.DER),
    "leaf_good_dns.der":        leaf_good_dns.public_bytes(serialization.Encoding.DER),
    "leaf_bad_dns.der":         leaf_bad_dns.public_bytes(serialization.Encoding.DER),
    "leaf_excluded_dns.der":    leaf_excluded_dns.public_bytes(serialization.Encoding.DER),
}

for name, data in files.items():
    path = OUT / name
    path.write_bytes(data)
    print(f"  wrote {path} ({len(data)} bytes)")

print("done")

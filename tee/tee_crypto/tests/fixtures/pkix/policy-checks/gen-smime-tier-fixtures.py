#!/usr/bin/env python3
"""Generate S/MIME tier-validated test fixtures for CA/B Forum S/MIME BR sub-profiles.

This is a sibling generator to `gen.py`, isolated so that adding a new tier
fixture does not regenerate the unrelated fixtures in `gen.py`'s output set.
Same convention as `gen-sc081-fixtures.py` (separate script for SC-081 cases).

Strict-generation fixtures (OID suffix `.3`) target the canonical modern
tier per PKIX-jbvb.6 (Mark 2026-05-13). Multipurpose-generation fixtures
(OID suffix `.2`) target sibling Profile types per PKIX-jbvb.9 (the
decision to ship Multipurpose siblings was recorded in PKIX-jbvb.8).
Legacy generation (`.1`) is BR-banned for new issuance effective
2025-07-15 per §7.1.6.1 line 2600 and is not represented in `-cabf`
Profile types.

Fixtures produced (PKIX-jbvb.7, Individual-validated tier, Strict generation):

  smime-individual-validated-self-signed-365d.der
    Self-signed P-256 cert with:
    - Subject DN: C=GB, givenName=Test, surname=Person, CN=Test Person
    - CertificatePolicies: 2.23.140.1.5.4.3 (Individual-validated Strict)
    - rfc822Name SAN: individual@example.com
    - emailProtection EKU
    - 365-day validity (notBefore 2026-01-01)
    - cA=TRUE (self-signed anchor pattern matches existing smime-self-signed-365d.der)

  smime-individual-pseudonym-self-signed-365d.der
    Self-signed P-256 cert with:
    - Subject DN: C=GB, pseudonym=TestBox, CN=TestBox
    - CertificatePolicies: 2.23.140.1.5.4.3 (Individual-validated Strict)
    - rfc822Name SAN: testbox@example.com
    - emailProtection EKU
    - 365-day validity, cA=TRUE
    - Exercises the `AnyOf(pseudonym, AllOf(givenName, surname))` branch
      of `pkix_path::DnAttrRule` for the Individual-validated tier.

Fixtures produced (PKIX-jbvb.7, Sponsor-validated tier, Strict generation):

  smime-sponsor-validated-self-signed-365d.der
    Self-signed P-256 cert with:
    - Subject DN: C=GB, O=Acme Sponsor Ltd, organizationIdentifier=VATGB-12345678,
                  givenName=Alice, surname=Sponsored, CN=Alice Sponsored
    - CertificatePolicies: 2.23.140.1.5.3.3 (Sponsor-validated Strict)
    - rfc822Name SAN: alice.sponsored@acme-sponsor.example.com
    - emailProtection EKU, 365-day validity, cA=TRUE

  smime-sponsor-pseudonym-self-signed-365d.der
    Self-signed P-256 cert with:
    - Subject DN: C=GB, O=Acme Sponsor Ltd, organizationIdentifier=VATGB-87654321,
                  pseudonym=SponsoredAlias, CN=SponsoredAlias
    - CertificatePolicies: 2.23.140.1.5.3.3 (Sponsor-validated Strict)
    - rfc822Name SAN: sponsored.alias@acme-sponsor.example.com
    - emailProtection EKU, 365-day validity, cA=TRUE
    - Exercises the AnyOf branch alongside the required organizationName
      and organizationIdentifier.

Fixtures produced (PKIX-jbvb.9.5, Individual-validated tier, Multipurpose generation):

  smime-individual-multipurpose-self-signed-365d.der
    Self-signed P-256 cert with:
    - Subject DN: C=GB, givenName=Test, surname=Person, CN=Test Person
    - CertificatePolicies: 2.23.140.1.5.4.2 (Individual-validated Multipurpose)
    - rfc822Name SAN: individual-mp@example.com
    - emailProtection EKU
    - 365-day validity (notBefore 2026-01-01)
    - cA=TRUE (self-signed anchor pattern)
    - Mirrors smime-individual-validated-self-signed-365d.der in every
      structural respect except the asserted policy OID.

  smime-individual-multipurpose-pseudonym-self-signed-365d.der
    Self-signed P-256 cert with:
    - Subject DN: C=GB, pseudonym=TestBoxMP, CN=TestBoxMP
    - CertificatePolicies: 2.23.140.1.5.4.2 (Individual-validated Multipurpose)
    - rfc822Name SAN: testbox-mp@example.com
    - emailProtection EKU, 365-day validity, cA=TRUE
    - Exercises the AnyOf(pseudonym, AllOf(givenName, surname)) branch
      of pkix_path::DnAttrRule for the Multipurpose generation.

Fixtures produced (PKIX-jbvb.9.4, Sponsor-validated tier, Multipurpose generation):

  smime-sponsor-multipurpose-self-signed-365d.der
    Self-signed P-256 cert with:
    - Subject DN: C=GB, O=Acme Sponsor Ltd, organizationIdentifier=VATGB-12345678,
                  givenName=Alice, surname=Sponsored, CN=Alice Sponsored
    - CertificatePolicies: 2.23.140.1.5.3.2 (Sponsor-validated Multipurpose)
    - rfc822Name SAN: alice.sponsored-mp@acme-sponsor.example.com
    - emailProtection EKU, 365-day validity, cA=TRUE
    - Mirrors smime-sponsor-validated-self-signed-365d.der except for the
      asserted policy OID.

  smime-sponsor-multipurpose-pseudonym-self-signed-365d.der
    Self-signed P-256 cert with:
    - Subject DN: C=GB, O=Acme Sponsor Ltd, organizationIdentifier=VATGB-87654321,
                  pseudonym=SponsoredAliasMP, CN=SponsoredAliasMP
    - CertificatePolicies: 2.23.140.1.5.3.2 (Sponsor-validated Multipurpose)
    - rfc822Name SAN: sponsored-mp.alias@acme-sponsor.example.com
    - emailProtection EKU, 365-day validity, cA=TRUE
    - Exercises the AnyOf branch alongside the required organizationName
      and organizationIdentifier.

# Provenance

Individual-tier fixtures modeled after zlint's `smime_leg1_iv_eff1.pem`
(Individual-validated tier marker) with the OID rewritten from Legacy
(2.23.140.1.5.4.1) to Strict (2.23.140.1.5.4.3) per PKIX-jbvb.6.
Sponsor-tier fixtures modeled after zlint's `smime_leg1_sv_eff1.pem`
(Sponsor-validated tier marker) with the same Legacy→Strict OID rewrite.
Both zlint published fixtures lack the BR-mandated tier-specific Subject
DN attributes (zlint's IV fixture has only `C=GB, CN=Leon Mandrake`;
zlint's SV fixture has `C=US, L=Nowhere, O=Some Company Ltd., CN=Leon
Mandrake` — present organizationName, but no
organizationIdentifier/givenName/surname/pseudonym). The workspace
fixtures include the full DN attribute shape required by CA/B Forum
S/MIME BR §7.1.4.2.5 (Sponsor) and §7.1.4.2.6 (Individual) Note 2 so
that pkix-path's `required_leaf_subject_dn_attrs` check has the
attribute coverage it tests for. pkilint's
`tests/integration_certificates/cabf/smime/` was checked but not cloned
at fixture-authoring time; modeling-against parity is documented on the
assertion that pkilint classifies tier-validated certs by the same
policy-OID + DN-attr criteria.

# Oracle

`openssl x509 -inform DER -text -noout < <file>.der` exposes:
  - Subject (multi-attribute DN)
  - X509v3 Subject Alternative Name: email:<addr>
  - X509v3 Extended Key Usage: E-mail Protection
  - X509v3 Certificate Policies: Policy: 2.23.140.1.5.4.3 (or .3.3 for Sponsor)
  - X509v3 Basic Constraints: critical, CA:TRUE

# Re-running

Re-running this script generates new random keys; the existing fixture bytes
will change. Tests assert structural properties (cert.subject contains
expected OIDs, policy OID is asserted, EKU/SAN shape) so byte-level changes
are safe.
"""

import datetime
from pathlib import Path

from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.x509.oid import ExtendedKeyUsageOID, NameOID

OUT = Path(__file__).parent

# Match gen.py's time convention so GRY_NOW = 2026-06-01 is within window.
NOT_BEFORE = datetime.datetime(2026, 1, 1, tzinfo=datetime.timezone.utc)
NOT_AFTER_365 = NOT_BEFORE + datetime.timedelta(days=365)

# Independent serial counter (does not interleave with gen.py's counter so the
# two scripts can run in either order without serial collisions).
_serial = 100


def next_serial():
    global _serial
    s = _serial
    _serial += 1
    return s


# CA/B Forum S/MIME BR reserved policy OIDs (§7.1.6.1 / Appendix A) —
# Strict generation (suffix `.3`) and Multipurpose generation (suffix `.2`).
SMIME_INDIVIDUAL_VALIDATED_STRICT_POLICY = x509.ObjectIdentifier("2.23.140.1.5.4.3")
SMIME_SPONSOR_VALIDATED_STRICT_POLICY = x509.ObjectIdentifier("2.23.140.1.5.3.3")
SMIME_INDIVIDUAL_VALIDATED_MULTIPURPOSE_POLICY = x509.ObjectIdentifier("2.23.140.1.5.4.2")
SMIME_SPONSOR_VALIDATED_MULTIPURPOSE_POLICY = x509.ObjectIdentifier("2.23.140.1.5.3.2")

# organizationIdentifier (RFC 4519 / X.520 OID 2.5.4.97). pyca's NameOID
# does not export this; construct from raw OID. Used by Sponsor-validated
# fixtures (SHALL across all generations per §7.1.4.2.5).
OID_ORGANIZATION_IDENTIFIER = x509.ObjectIdentifier("2.5.4.97")


def make_tier_cert(filename, subject_attrs, policy_oid, rfc822_san_email):
    """Build a self-signed S/MIME tier cert for use as both anchor and leaf.

    Uses the self-signed-anchor pattern from gen.py's smime_self_signed:
    cA=TRUE on the leaf so it can serve as its own trust anchor in tests
    without needing a separate CA chain.
    """
    key = ec.generate_private_key(ec.SECP256R1())
    subject = x509.Name(subject_attrs)
    cert = (
        x509.CertificateBuilder()
        .subject_name(subject)
        .issuer_name(subject)
        .public_key(key.public_key())
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
            x509.SubjectAlternativeName([x509.RFC822Name(rfc822_san_email)]),
            critical=False,
        )
        .add_extension(
            x509.ExtendedKeyUsage([ExtendedKeyUsageOID.EMAIL_PROTECTION]),
            critical=False,
        )
        .add_extension(
            x509.CertificatePolicies(
                [
                    x509.PolicyInformation(
                        policy_identifier=policy_oid, policy_qualifiers=None
                    ),
                ]
            ),
            critical=False,
        )
        .sign(key, hashes.SHA256())
    )
    path = OUT / filename
    path.write_bytes(cert.public_bytes(serialization.Encoding.DER))
    print(f"  wrote {path} ({path.stat().st_size} bytes)")


# ---------------------------------------------------------------------------
# Individual-validated tier, Strict generation (PKIX-jbvb.7)
# CA/B Forum S/MIME BR §7.6 + §7.1.4.2.6 Note 2 (Strict and Multipurpose).
#
# Subject DN rule:
#   AnyOf: pseudonym OR (givenName + surname)
#
# serialNumber is MAY in all generations (§7.1.4.2.6 table), so the workspace
# Profile does not require it. Fixtures omit it for minimal DN shape.
# ---------------------------------------------------------------------------

# Form 1: givenName + surname (most common Individual-validated shape).
make_tier_cert(
    "smime-individual-validated-self-signed-365d.der",
    [
        x509.NameAttribute(NameOID.COUNTRY_NAME, "GB"),
        x509.NameAttribute(NameOID.GIVEN_NAME, "Test"),
        x509.NameAttribute(NameOID.SURNAME, "Person"),
        x509.NameAttribute(NameOID.COMMON_NAME, "Test Person"),
    ],
    SMIME_INDIVIDUAL_VALIDATED_STRICT_POLICY,
    "individual@example.com",
)

# Form 2: pseudonym only. Exercises the AnyOf branch.
make_tier_cert(
    "smime-individual-pseudonym-self-signed-365d.der",
    [
        x509.NameAttribute(NameOID.COUNTRY_NAME, "GB"),
        x509.NameAttribute(NameOID.PSEUDONYM, "TestBox"),
        x509.NameAttribute(NameOID.COMMON_NAME, "TestBox"),
    ],
    SMIME_INDIVIDUAL_VALIDATED_STRICT_POLICY,
    "testbox@example.com",
)


# ---------------------------------------------------------------------------
# Sponsor-validated tier, Strict generation (PKIX-jbvb.7)
# CA/B Forum S/MIME BR §7.5 + §7.1.4.2.5 Note 2 (Strict and Multipurpose).
#
# Subject DN rule:
#   AllOf:
#     organizationName            (SHALL across all generations)
#     organizationIdentifier      (SHALL across all generations)
#     AnyOf: pseudonym OR (givenName + surname)
#
# Sponsor-validated = Individual-validated + organizationName + organizationIdentifier:
# an employer or sponsoring organization vouches for the named individual.
# ---------------------------------------------------------------------------

# Form 1: org + orgID + givenName + surname.
make_tier_cert(
    "smime-sponsor-validated-self-signed-365d.der",
    [
        x509.NameAttribute(NameOID.COUNTRY_NAME, "GB"),
        x509.NameAttribute(NameOID.ORGANIZATION_NAME, "Acme Sponsor Ltd"),
        x509.NameAttribute(OID_ORGANIZATION_IDENTIFIER, "VATGB-12345678"),
        x509.NameAttribute(NameOID.GIVEN_NAME, "Alice"),
        x509.NameAttribute(NameOID.SURNAME, "Sponsored"),
        x509.NameAttribute(NameOID.COMMON_NAME, "Alice Sponsored"),
    ],
    SMIME_SPONSOR_VALIDATED_STRICT_POLICY,
    "alice.sponsored@acme-sponsor.example.com",
)

# Form 2: org + orgID + pseudonym. Exercises the AnyOf branch.
make_tier_cert(
    "smime-sponsor-pseudonym-self-signed-365d.der",
    [
        x509.NameAttribute(NameOID.COUNTRY_NAME, "GB"),
        x509.NameAttribute(NameOID.ORGANIZATION_NAME, "Acme Sponsor Ltd"),
        x509.NameAttribute(OID_ORGANIZATION_IDENTIFIER, "VATGB-87654321"),
        x509.NameAttribute(NameOID.PSEUDONYM, "SponsoredAlias"),
        x509.NameAttribute(NameOID.COMMON_NAME, "SponsoredAlias"),
    ],
    SMIME_SPONSOR_VALIDATED_STRICT_POLICY,
    "sponsored.alias@acme-sponsor.example.com",
)


# ---------------------------------------------------------------------------
# Individual-validated tier, Multipurpose generation (PKIX-jbvb.9.5)
# CA/B Forum S/MIME BR §7.6 + §7.1.4.2.6 Note 2 (Strict and Multipurpose).
#
# Subject DN rule:
#   AnyOf: pseudonym OR (givenName + surname)
#
# Identical to the Strict-generation Individual-validated rule, only the
# asserted policy OID changes (.4.2 vs .4.3). Fixtures mirror the Strict
# pair to make the OID the sole structural difference observable to the
# validator.
# ---------------------------------------------------------------------------

# Form 1: givenName + surname (most common Individual-validated shape).
make_tier_cert(
    "smime-individual-multipurpose-self-signed-365d.der",
    [
        x509.NameAttribute(NameOID.COUNTRY_NAME, "GB"),
        x509.NameAttribute(NameOID.GIVEN_NAME, "Test"),
        x509.NameAttribute(NameOID.SURNAME, "Person"),
        x509.NameAttribute(NameOID.COMMON_NAME, "Test Person"),
    ],
    SMIME_INDIVIDUAL_VALIDATED_MULTIPURPOSE_POLICY,
    "individual-mp@example.com",
)

# Form 2: pseudonym only. Exercises the AnyOf branch.
make_tier_cert(
    "smime-individual-multipurpose-pseudonym-self-signed-365d.der",
    [
        x509.NameAttribute(NameOID.COUNTRY_NAME, "GB"),
        x509.NameAttribute(NameOID.PSEUDONYM, "TestBoxMP"),
        x509.NameAttribute(NameOID.COMMON_NAME, "TestBoxMP"),
    ],
    SMIME_INDIVIDUAL_VALIDATED_MULTIPURPOSE_POLICY,
    "testbox-mp@example.com",
)


# ---------------------------------------------------------------------------
# Sponsor-validated tier, Multipurpose generation (PKIX-jbvb.9.4)
# CA/B Forum S/MIME BR §7.5 + §7.1.4.2.5 Note 2 (Strict and Multipurpose).
#
# Subject DN rule (identical to Sponsor Strict):
#   AllOf:
#     organizationName            (SHALL across all generations)
#     organizationIdentifier      (SHALL across all generations)
#     AnyOf: pseudonym OR (givenName + surname)
#
# Only the asserted policy OID changes (.3.2 vs .3.3). Fixtures mirror
# the Strict-generation Sponsor pair to make the OID the sole structural
# difference observable to the validator.
# ---------------------------------------------------------------------------

# Form 1: org + orgID + givenName + surname.
make_tier_cert(
    "smime-sponsor-multipurpose-self-signed-365d.der",
    [
        x509.NameAttribute(NameOID.COUNTRY_NAME, "GB"),
        x509.NameAttribute(NameOID.ORGANIZATION_NAME, "Acme Sponsor Ltd"),
        x509.NameAttribute(OID_ORGANIZATION_IDENTIFIER, "VATGB-12345678"),
        x509.NameAttribute(NameOID.GIVEN_NAME, "Alice"),
        x509.NameAttribute(NameOID.SURNAME, "Sponsored"),
        x509.NameAttribute(NameOID.COMMON_NAME, "Alice Sponsored"),
    ],
    SMIME_SPONSOR_VALIDATED_MULTIPURPOSE_POLICY,
    "alice.sponsored-mp@acme-sponsor.example.com",
)

# Form 2: org + orgID + pseudonym. Exercises the AnyOf branch.
make_tier_cert(
    "smime-sponsor-multipurpose-pseudonym-self-signed-365d.der",
    [
        x509.NameAttribute(NameOID.COUNTRY_NAME, "GB"),
        x509.NameAttribute(NameOID.ORGANIZATION_NAME, "Acme Sponsor Ltd"),
        x509.NameAttribute(OID_ORGANIZATION_IDENTIFIER, "VATGB-87654321"),
        x509.NameAttribute(NameOID.PSEUDONYM, "SponsoredAliasMP"),
        x509.NameAttribute(NameOID.COMMON_NAME, "SponsoredAliasMP"),
    ],
    SMIME_SPONSOR_VALIDATED_MULTIPURPOSE_POLICY,
    "sponsored-mp.alias@acme-sponsor.example.com",
)

print("done")

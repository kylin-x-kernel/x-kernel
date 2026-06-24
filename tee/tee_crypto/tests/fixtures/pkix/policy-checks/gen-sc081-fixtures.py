#!/usr/bin/env python3
"""Generate SC-081 test fixtures for pkix-lint tests.

Fixtures:
1. leaf-p256-110d-post-sc081-100d.der
   notBefore: 2027-03-16 00:00:00Z (one day past 100d threshold)
   notAfter:  2027-07-04 00:00:00Z (110 days later → exceeds 100d cap)
   Expected: ValidityMaxLint should return Error

2. leaf-p256-50d-post-sc081-100d.der
   notBefore: 2027-03-16 00:00:00Z
   notAfter:  2027-05-05 00:00:00Z (50 days later → within 100d cap)
   Expected: ValidityMaxLint should return Pass

3. leaf-p256-50d-post-sc081-47d.der
   notBefore: 2029-03-16 00:00:00Z (one day past 47d threshold)
   notAfter:  2029-05-05 00:00:00Z (50 days → exceeds 47d cap)
   Expected: ValidityMaxLint should return Error

4. leaf-p256-45d-post-sc081-47d.der
   notBefore: 2029-03-16 00:00:00Z
   notAfter:  2029-04-30 00:00:00Z (45 days → within 47d cap)
   Expected: ValidityMaxLint should return Pass
"""

import datetime
from cryptography import x509
from cryptography.x509.oid import NameOID, ExtendedKeyUsageOID
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec

def make_leaf(not_before, not_after, filename):
    key = ec.generate_private_key(ec.SECP256R1())
    subject = x509.Name([
        x509.NameAttribute(NameOID.COMMON_NAME, "test"),
        x509.NameAttribute(NameOID.ORGANIZATION_NAME, "test"),
        x509.NameAttribute(NameOID.COUNTRY_NAME, "US"),
    ])
    builder = (
        x509.CertificateBuilder()
        .subject_name(subject)
        .issuer_name(subject)
        .public_key(key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(not_before)
        .not_valid_after(not_after)
        .add_extension(x509.BasicConstraints(ca=False, path_length=None), critical=True)
        .add_extension(
            x509.SubjectAlternativeName([x509.DNSName("test.example.com")]),
            critical=False,
        )
        .add_extension(
            x509.ExtendedKeyUsage([ExtendedKeyUsageOID.SERVER_AUTH]),
            critical=False,
        )
    )
    cert = builder.sign(key, hashes.SHA256())
    der = cert.public_bytes(serialization.Encoding.DER)
    with open(filename, "wb") as f:
        f.write(der)
    print(f"Generated {filename}: notBefore={not_before.date()} notAfter={not_after.date()} ({(not_after - not_before).days} days)")
    return cert

# 2027-03-16: one day past 100d phase start (2027-03-15)
t_2027_03_16 = datetime.datetime(2027, 3, 16, 0, 0, 0, tzinfo=datetime.timezone.utc)
# 2029-03-16: one day past 47d phase start (2029-03-15)
t_2029_03_16 = datetime.datetime(2029, 3, 16, 0, 0, 0, tzinfo=datetime.timezone.utc)

# 1. 110d cert → exceeds 100d cap
make_leaf(
    t_2027_03_16,
    t_2027_03_16 + datetime.timedelta(days=110),
    "/tmp/leaf-p256-110d-post-sc081-100d.der",
)
# 2. 50d cert → passes 100d cap
make_leaf(
    t_2027_03_16,
    t_2027_03_16 + datetime.timedelta(days=50),
    "/tmp/leaf-p256-50d-post-sc081-100d.der",
)
# 3. 50d cert → exceeds 47d cap
make_leaf(
    t_2029_03_16,
    t_2029_03_16 + datetime.timedelta(days=50),
    "/tmp/leaf-p256-50d-post-sc081-47d.der",
)
# 4. 45d cert → passes 47d cap
make_leaf(
    t_2029_03_16,
    t_2029_03_16 + datetime.timedelta(days=45),
    "/tmp/leaf-p256-45d-post-sc081-47d.der",
)

print("All fixtures generated.")

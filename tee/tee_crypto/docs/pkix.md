# PKIX path validation (`feature = "pkix"`)

## Scope

`tee_crypto::pkix` implements RFC 5280 certificate path validation. Cryptographic
signature checks for X.509 TBSCertificate bytes use **`pkix/x509_verify.rs`**
helpers (message + DER signature semantics), not `tee_ops` pre-hash APIs.

## Architecture

Implementation is **inlined** in `src/pkix_path/` (upstream baseline:
[crate-pkix](https://github.com/MarkAtwood/crate-pkix) `pkix-path` 0.3.2 +
`x509-cert` 0.3 accessor migration). `tee_crypto::pkix` is a thin facade that
re-exports `pkix_path` and adds GmSSL/SM2 wrappers in `src/pkix/`.

```
tee_crypto/src/
├── pkix_path/          # RFC 5280 path validation (canonical source)
│   ├── mod.rs
│   └── serde_der.rs
├── pkix/               # facade + SM2 DefaultVerifier + x509_verify helpers
│   ├── mod.rs
│   └── x509_verify.rs
tests/
├── fixtures/pkix/      # DER fixtures for lib + integration tests
├── pkix_anchor_nc.rs
└── pkix_stitch.rs
```

**Do not** add SM2 or tasign-specific logic to `src/pkix_path/mod.rs`.

Only `tee_crypto` is published to crates.io; there is no separate `pkix-path`
crate in this tree.

## Supported signature algorithms (`DefaultVerifier`)

| OID | Algorithm | Helper |
|-----|-----------|--------|
| 1.2.840.10045.4.3.2 | ECDSA P-256 + SHA-256 | `verify_ecdsa_p256_sha256` |
| 1.2.840.10045.4.3.3 | ECDSA P-384 + SHA-384 | `verify_ecdsa_p384_sha384` |
| 1.2.840.113549.1.1.11 | RSA PKCS#1 v1.5 + SHA-256 | `verify_rsa_pkcs1v15_sha256` |
| 1.2.840.113549.1.1.12 | RSA PKCS#1 v1.5 + SHA-384 | `verify_rsa_pkcs1v15_sha384` |
| 1.2.840.113549.1.1.13 | RSA PKCS#1 v1.5 + SHA-512 | `verify_rsa_pkcs1v15_sha512` |
| 1.2.156.10197.1.501 | SM2-with-SM3 | `verify_sm2_sign_sm3` |

## Encoding notes

- **ECDSA**: signature bytes are **DER** (`r`, `s` SEQUENCE). Verified with
  `VerifyingKey::verify(message, &DerSignature)` (hashes message internally).
- **RSA PKCS#1 v1.5**: signature bytes are **raw** PKCS#1 v1.5 block. Verified
  with `VerifyingKey::<Sha*>` over the **TBSCertificate DER** (DigestInfo built
  inside the RSA crate).
- **SM2**: signature bytes are **DER** `(r, s)`. Public key is taken from issuer
  SPKI `subjectPublicKey` SEC1 uncompressed octets. Default distinguishing ID
  `1234567812345678` (see `sm2::DEFAULT_DISTINGUISHING_ID`).

## Features

- `pkix` — path validation + `x509_verify` helpers
- `pkix-internal-tests` — large fixture test suite in `src/pkix_path/mod.rs`
  (optional, dev-only)

## Testing

Run from `tee/tee_crypto/` (host `cargo test`; no kernel `.config` required):

```bash
cd tee/tee_crypto

# In-crate lib tests: path validation, policy fields, signature helpers
cargo test --features "pkix,pkix-internal-tests" --lib

# Integration: trust-anchor NameConstraints seeding
cargo test --features pkix --test pkix_anchor_nc

# Integration: x509_verify stitch tests (ECDSA, RSA, SM2)
cargo test --features pkix --test pkix_stitch
```

| Command | Location | Coverage |
|---------|----------|----------|
| `--lib` + `pkix-internal-tests` | `src/pkix_path/mod.rs` | ECDSA/RSA verify helpers, `validate_path`, chain walk, policy fields, DN rules, required leaf policy OID |
| `--test pkix_anchor_nc` | `tests/pkix_anchor_nc.rs` | Permitted/excluded NC subtrees, malformed NC extension handling |
| `--test pkix_stitch` | `tests/pkix_stitch.rs` | End-to-end sign/verify for ECDSA P-256, RSA PKCS#1 v1.5, SM2 via `x509_verify` helpers |

Fixtures live under `tests/fixtures/pkix/` (included in the `tee_crypto` publish
tarball). Lib tests embed DER via `include_bytes!` from that directory;
`pkix_anchor_nc` reads `tests/fixtures/pkix/anchor-nc/` at runtime.

Optional fixture generators (`tests/fixtures/pkix/policy-checks/*.py`,
`anchor-nc/gen.py`) are for regenerating certs; they are not run by CI.

## Re-vendor from upstream

When syncing a newer upstream `pkix-path` release:

```bash
UPSTREAM=/tmp/crate-pkix/pkix-path
CANON=tee/tee_crypto/src/pkix_path/mod.rs

rsync -a "$UPSTREAM/src/lib.rs" "$CANON"
rsync -a "$UPSTREAM/tests/fixtures/" tee/tee_crypto/tests/fixtures/pkix/
```

Then edit `src/pkix_path/mod.rs`:

1. Remove `#![…]` crate attributes (module inside `tee_crypto`).
2. Replace `#[cfg(feature = "rsa"|"p256"|"p384")]` → `#[cfg(feature = "pkix")]`.
3. Replace `"../tests/fixtures/` → `"../../tests/fixtures/pkix/`.
4. Change `pub mod serde_der` → `mod serde_der`.
5. Fix patch artifacts: `#[cfg(feature = "pkix")]` / `doc(cfg(feature = "pkix"))` only
   (remove duplicated `any(feature = …)` and stale `p256`/`p384`/`rsa` doc labels).
6. Apply the `x509-cert` 0.3 accessor migration (see checklist below).

Regression:

```bash
cargo test -p tee_crypto --features "pkix,pkix-internal-tests" --lib
cargo test -p tee_crypto --features pkix --test pkix_anchor_nc
cargo test -p tee_crypto --features pkix --test pkix_stitch
```

### x509-cert 0.3 accessor checklist

| Upstream (0.2) | Migrated (0.3-rc.4) |
|----------------|---------------------|
| `cert.tbs_certificate.field` | `cert.tbs_certificate().field()` (`.clone()` when storing owned values) |
| `cert.signature_algorithm` | `cert.signature_algorithm()` |
| `cert.signature.raw_bytes()` | `cert.signature().raw_bytes()` |
| `subject.0.iter()` / `rdn.0.iter()` | `subject.as_ref().iter()` / `rdn.iter()` |
| `names_match` walking `Name` via `.0` | use `Name::as_ref()` and `RdnSequence::iter()` |

Local deltas to keep minimal:

| Area | Change | Why |
|------|--------|-----|
| `src/pkix_path/mod.rs` | x509-cert 0.3 accessor migration | `x509-cert` 0.3.0-rc.4 API |
| `src/pkix_path/mod.rs` | `feature = "pkix"` gates (was rsa/p256/p384) | parent crate features |
| `src/pkix/` only | SM2 / GmSSL | tee_crypto extensions |

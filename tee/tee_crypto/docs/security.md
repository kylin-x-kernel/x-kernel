# tee_crypto Security Notes

`tee_crypto` wraps RustCrypto primitives for use by the TEE stack. The crate is
not a new cryptographic implementation except where glue code is required for
TEE-compatible streaming, padding, XTS, or big-number behavior.

## Trust Boundaries

Inputs from `rust-libutee`, TA tests, object attributes, and serialized key
material are treated as untrusted. Public functions validate lengths, key
sizes, modulus constraints, padding, and encodings before calling backend
operations.

The crate returns typed `CryptoError` values. Higher layers, such as
`rust-libutee`, map those errors to `TEE_Result` values at the API boundary.

Raw buffers from TEE operation entry points are wrapped into semantic byte
types at the boundary. For example, a TEE signature buffer becomes
`SignatureBytes` with explicit algorithm and encoding metadata before reaching
RSA, ECC, or SM2 verification code.

## Backend Assumptions

Symmetric ciphers, hash functions, MACs, RSA, ECC, SM2, and big-number
arithmetic rely on the selected RustCrypto crates. Constant-time behavior is
therefore inherited from those backends where they provide it.

MD5 support is retained for GP TEE compatibility and is implemented through
the RustCrypto `md-5` backend. It should not be selected for new security
protocols.

Glue code must not introduce secret-dependent branches for operations that are
expected to be constant-time. Some compatibility paths, such as high-level TEE
object parsing and public metadata checks, are not secret-bearing.

## Randomness

`DeterministicRng` is useful for tests and reproducible TA validation.
Production key generation should use a TEE-provided entropy source through the
`CryptoRng` trait. Callers' RNGs are passed through to RustCrypto backends
directly; tee_crypto does not reseed a secondary deterministic RNG from caller
entropy. Do not treat deterministic seed helpers as production entropy.

## Padding And Verification

PKCS#7 padding validation returns `InvalidInput` on malformed padding. AEAD
and signature verification failures return `VerificationFailed`, allowing
higher layers to map them without exposing backend-specific details.

GCM tag checks use constant-time equality. Other verification behavior follows
the backend implementation.

Signature and ciphertext verification rejects metadata mismatches before
calling the backend. Wrong signature algorithm, wrong signature encoding,
wrong ciphertext algorithm, and wrong digest algorithm use specific
`CryptoError` variants rather than being collapsed into `InvalidInput`.

`StreamingCipherCtx` constructors and internal state use an explicit
`Direction` enum. This avoids boolean polarity mistakes at call sites and
inside mode dispatch.

## Big Numbers

`TeeBigInt` models signed GP TEE arithmetic values. Modular operations reject
invalid moduli with `InvalidModulus`; division by zero returns `DivideByZero`;
integer conversion overflow returns `ArithmeticOverflow`.

The BigInt APIs are compatibility helpers and do not claim constant-time
behavior for all arithmetic operations.

## Unsupported Algorithms

Unsupported algorithm combinations return `UnsupportedAlgorithm` rather than a
generic backend error. This is used for intentionally unsupported choices such
as unavailable curves or hash/padding combinations.

Shared `HashAlgorithm` metadata describes digest sizes and names only. It does
not imply every digest is supported by every operation; RSA PSS, OAEP, ECDSA,
and SM2 still enforce their own support matrices.

Raw signing APIs require `DigestBytes`; this prevents callers from accidentally
passing a message, a digest produced by a different hash, or a correctly sized
but unlabelled byte string. SM2 raw DSA requires an SM3 digest.

## Secret Material

Secret-bearing owned byte containers use `zeroize`-backed storage. Secret
contents are intentionally exposed through methods named `expose_secret()` and
`expose_secret_clone()` so review can spot places where private key material,
plaintext, or shared secrets leave their wrapper.

`SecretBytes`, `PlaintextBytes`, and `SharedSecretBytes` do not implement
`Deref` or `AsRef<[u8]>`. Callers must opt in to exposing secret material at
each read site. RSA APIs also keep backend-native `rsa` crate key types behind
`RsaKeypair` and `RsaPublic` wrappers, limiting backend representation leaks
at public boundaries.

## Error Details

`CryptoError::Backend(BackendError)` records the failing backend operation
class without storing backend error strings. This keeps the public error model
stable and avoids creating string-based protocols for upper layers.

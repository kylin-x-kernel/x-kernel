# tee_crypto Design

`tee_crypto` is the Rust cryptographic abstraction layer used by the TEE
stack. It replaces the former mbedtls dependency for TA-facing crypto and
big-number operations while keeping the crate usable in `no_std` kernel and
TA builds.

## Goals

- Provide stable, typed Rust APIs for the algorithms required by `rust-libutee`
  and TEE tests.
- Keep backend dependencies behind small wrapper modules so callers do not
  depend directly on RustCrypto implementation details.
- Preserve `no_std` operation with `alloc` where algorithm outputs require
  owned buffers.
- Provide integration tests from the crate root `tests/` directory so the
  public API is exercised like an external user.

## Module Layout

- `algorithms`: private source directory for cryptographic algorithms and
  primitive wrappers. Callers should use the crate-root public modules listed
  below, such as `tee_crypto::hash`, `tee_crypto::rsa`, and
  `tee_crypto::tee_ops`.
- `aead`: one-shot AEAD APIs for GCM and CCM.
- `block_cipher`: single-block ECB wrappers and static block-cipher metadata.
- `cipher`: one-shot CBC and CTR helpers.
- `ecc`: object-style ECC keypair API.
- `tee_ops::ecc`: stateless TEE-style ECC operations over raw key material,
  including typed byte-component return values. Sign and verify APIs accept
  `DigestBytes` rather than untagged byte slices.
- `hash`: digest wrappers plus `HashAlgorithm` / `HashSpec`
  metadata shared by RSA and ECC selectors. `Digest::finalize()` returns
  `DigestBytes`, which carries both digest bytes and the producing algorithm.
- `md5`: MD5 digest wrapper backed by RustCrypto. It lives outside `hash`
  because it exists for TEE compatibility and should remain visibly separate
  from the preferred hash family wrappers.
- `hkdf`: HKDF extract, expand, and one-shot helpers.
- `mac`: HMAC and CMAC wrappers.
- `material`: algorithm-tagged signatures, ciphertexts, and shared secrets
  used by asymmetric operations.
- `rsa`: object-style RSA keypair/public-key API.
- `tee_ops::rsa`: stateless TEE-style RSA operations over raw key material,
  encodings, hashes, and padding selectors. Sign and verify APIs accept
  `DigestBytes` rather than untagged byte slices.
- `sm2`: object-style SM2 DSA, PKE, and KEP wrappers.
- `asymmetric`: shared asymmetric traits and algorithm-neutral types. It is a
  common API contract, so it stays outside `algorithms`.
- `bytes`: base public, big-endian, secret, and plaintext byte containers.
  Algorithm-tagged asymmetric material lives in `material`; hash output lives
  in `hash`.
- `bignum`: signed and unsigned big-number wrappers backed by `crypto-bigint`.
  This is the TEE arithmetic/MPI compatibility layer, so it stays outside
  `algorithms`.
- `rng`: RNG abstraction aligned with `rand_core` crypto RNG traits. It is a
  provider interface, so it stays outside `algorithms`.
- `streaming_cipher`: mbedtls-like update/final context for block, stream,
  and AEAD algorithms. It is a state-machine compatibility layer and therefore
  stays outside `algorithms`.
- `tee_ops`: operation-level APIs shaped around TEE object attributes and raw
  operation entry points. These functions expose tee_crypto wrapper types,
  typed byte containers, and component structs rather than backend crate key
  types.
- `xts`: XTS mode wrappers for AES and SM4.

Single-file algorithms stay as files under `src/algorithms/` to keep the tree
compact. Directories are reserved for modules with real internal structure,
currently root-level `src/bignum`.

The module root files expose object-style or one-shot Rust APIs. The
`tee_ops` files expose raw stateless operations that are easier for
`rust-libutee` and TEE object attributes to call.

## Semantic Byte Types

Byte buffers that cross public crypto APIs are tagged by purpose:

- `PublicBytes` and `BigEndianBytes` carry public byte strings and integer
  encodings.
- `SecretBytes` and `PlaintextBytes` zeroize owned secret buffers on drop and
  expose secret material only through `expose_secret()` /
  `expose_secret_clone()`. They intentionally do not implement `Deref` or
  `AsRef<[u8]>`, so secret reads remain visible during review.
- `SignatureBytes` records both the signature algorithm and encoding.
- `CiphertextBytes` records the encryption algorithm that produced the
  ciphertext.
- `SharedSecretBytes` records the key-agreement algorithm and zeroizes owned
  shared-secret material on drop. It follows the same explicit
  `expose_secret()` policy as `SecretBytes`.
- `DigestBytes` records the hash algorithm that produced a digest.

This keeps algorithm metadata at the API boundary instead of relying on tuple
position, comments, or caller discipline. `SignatureBytes`,
`CiphertextBytes`, and `SharedSecretBytes` live in `material` because their
algorithm tags are asymmetric-operation contracts. `DigestBytes` lives in
`hash` because it is tied to `HashAlgorithm`. Untagged `Vec<u8>` is still
allowed for external boundary input, raw mathematical results, DER/PKCS blobs,
and backend glue where the byte meaning is already fixed by the enclosing
function.

## Asymmetric API Contracts

Object-style `Signer` / `Verifier` implementations take messages and perform
the algorithm's normal message-signing flow internally.

TEE-style `tee_ops::rsa`, `tee_ops::ecc`, and SM2 raw DSA helpers operate on
precomputed digests. Those functions require `DigestBytes` and verify that the
digest algorithm and output length match the requested hash selector before
calling the backend.

RSA operation APIs expose `RsaKeypair`, `RsaPublic`, or component structs.
Backend-native `rsa::RsaPrivateKey` and `rsa::RsaPublicKey` stay inside
tee_crypto wrappers and are only available to crate-internal glue.

Public key components are strongly typed:

- `RsaPublicComponents` holds modulus `n` and exponent `e`.
- `EccPublicPoint` holds a curve plus affine coordinates.
- `Sm2PublicPoint` holds SM2 affine coordinates.

Callers use accessors such as `point.x()` and `public.n()` rather than matching
field names on a large algorithm-neutral enum.

TEE boundary code is responsible for turning raw input buffers into these
semantic types. Kernel-side helpers in the TEE crypto layer centralize
conversion from TEE algorithm IDs to `DigestBytes`, `SignatureBytes`, and
`CiphertextBytes`.

## Streaming Cipher

`streaming_cipher` is split into:

- `algo.rs`: `StreamingCipherAlgo`, `AlgorithmSpec`, and padding mode metadata.
- `context.rs`: `StreamingCipherCtx` state fields and public state-machine
  methods.
- `mode/`: mode-specific dispatch and processing for block modes, CTR, and
  AEAD.
- `padding.rs`: shared PKCS#7 padding helpers.

The context buffers partial blocks for ECB/CBC, processes CTR immediately, and
uses a single facade for GCM/CCM to match the mbedtls-style operation API used
by existing TEE code.

`StreamingCipherCtx` constructors take a `Direction` enum instead of a boolean
flag. This keeps encrypt/decrypt polarity explicit at call sites and inside
mode dispatch.

## Error Model

All public APIs return `Result<T, CryptoError>` unless they are pure metadata
queries. `CryptoError` uses stable variants such as `InvalidKey`,
`InvalidLength`, `BufferTooSmall`, `InvalidModulus`, and `VerificationFailed`.
Backend failures are represented by `CryptoError::Backend(BackendError)`
instead of formatted strings so higher layers can map errors consistently.

## Algorithm Metadata

Simple wrapper families use declaration macros where the algorithm is mainly a
backend type plus static metadata. For example, `block_cipher` uses one macro
to declare the wrapper type, trait implementation, and `BlockCipherSpec`.

Hash selection uses `HashAlgorithm` and `HashSpec` as the shared metadata
layer. RSA and ECC keep their legacy selector enums for source compatibility,
but expose conversions to the shared selector so future TEE algorithm-id
mapping can be centralized.

More complex algorithms such as RSA, ECC, and SM2 keep explicit code paths
because key formats, padding, hash selection, and backend error handling carry
different semantics.

## Tests

All crate-level tests live in `tee/tee_crypto/tests/` as Cargo integration
tests. They import `tee_crypto` through public APIs, which prevents tests from
accidentally depending on private implementation details.

Shared test helpers live under `tests/common/`. Each integration test compiles
as an independent crate, so helper functions should stay small and tolerate
being used by only a subset of test targets.

## x-kernel integration

The kernel workspace lists `tee/tee_crypto` as a **workspace member** so
developers can run `cargo test -p tee_crypto` and `cargo publish -p tee_crypto`
from the repository root. Runtime dependencies (`tee_kernel`, `devfs`, …) still
resolve **`tee_crypto` from crates.io** via `[workspace.dependencies]`:

```toml
tee_crypto = { version = "0.1", default-features = false }
```

There is no `path = "tee/tee_crypto"` in workspace dependencies, so member
crates do not link the in-tree copy unless patched.

### Standalone crate tests (default for `tee_crypto` changes)

```bash
# from repo root
cargo test -p tee_crypto
cargo test -p tee_crypto --features "pkix,pkix-internal-tests" --lib
cargo test -p tee_crypto --features pkix --test pkix_anchor_nc
cargo test -p tee_crypto --features pkix --test pkix_stitch

# or from the crate directory
cd tee/tee_crypto && cargo test ...
```

See `docs/pkix.md` for PKIX-specific commands.

### Publish to crates.io

```bash
cargo publish -p tee_crypto --registry crates-io --dry-run
```

Run from the **workspace root** (`x-kernel/`), not only from `tee/tee_crypto/`.

### Kernel build against local `tee/tee_crypto`

To exercise in-tree changes through `make build`, `make clippy`, or `make run`,
temporarily add to the **repository root** `Cargo.toml`:

```toml
[patch.crates-io]
tee_crypto = { path = "tee/tee_crypto" }
```

Remove the `[patch.crates-io]` block before merge unless the release intent is
to ship unpublished `tee_crypto` APIs. After publishing a new crates.io
release, bump the workspace `version = "0.1"` constraint only when the kernel
must depend on that release (Cargo resolves the latest compatible `0.1.x`).

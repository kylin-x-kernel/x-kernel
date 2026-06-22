// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Configurable RSA operations — multiple hash algorithms and padding schemes.
//!
//! Provides RSA sign/verify (PKCS#1 v1.5, PSS), encrypt/decrypt (PKCS#1 v1.5,
//! OAEP), raw RSA, and key construction from components.

use alloc::vec::Vec;

use crypto_bigint::BoxedUint;
use rsa::traits::{PrivateKeyParts, PublicKeyParts};

use crate::{
    bytes::{BigEndianBytes, PlaintextBytes, SecretBytes},
    error::{BackendError, CryptoError, Result},
    hash::{DigestBytes, HashAlgorithm},
    material::{
        CiphertextAlgorithm, CiphertextBytes, SignatureAlgorithm, SignatureBytes, SignatureEncoding,
    },
    rng::CryptoRng,
    rsa::{RsaKeypair, RsaPublic},
};

/// RSA hash algorithm selector.
///
/// `Md5` and `Sha1` variants exist for GlobalPlatform TEE API compatibility
/// (PKCS#1 v1.5, PSS, OAEP). Both are weak and must not be used for new
/// security-sensitive RSA operations; prefer SHA-256 or stronger.
#[derive(Clone, Debug, Copy)]
pub enum RsaHashAlgo {
    /// Legacy RSA hash — MD5; not recommended.
    Md5,
    /// Legacy RSA hash — SHA-1; not recommended.
    Sha1,
    Sha224,
    Sha256,
    Sha384,
    Sha512,
}

impl RsaHashAlgo {
    pub const fn hash_algorithm(self) -> HashAlgorithm {
        match self {
            Self::Md5 => HashAlgorithm::Md5,
            Self::Sha1 => HashAlgorithm::Sha1,
            Self::Sha224 => HashAlgorithm::Sha224,
            Self::Sha256 => HashAlgorithm::Sha256,
            Self::Sha384 => HashAlgorithm::Sha384,
            Self::Sha512 => HashAlgorithm::Sha512,
        }
    }
}

impl From<RsaHashAlgo> for HashAlgorithm {
    fn from(value: RsaHashAlgo) -> Self {
        value.hash_algorithm()
    }
}

impl TryFrom<HashAlgorithm> for RsaHashAlgo {
    type Error = CryptoError;

    fn try_from(value: HashAlgorithm) -> Result<Self> {
        match value {
            HashAlgorithm::Md5 => Ok(Self::Md5),
            HashAlgorithm::Sha1 => Ok(Self::Sha1),
            HashAlgorithm::Sha224 => Ok(Self::Sha224),
            HashAlgorithm::Sha256 => Ok(Self::Sha256),
            HashAlgorithm::Sha384 => Ok(Self::Sha384),
            HashAlgorithm::Sha512 => Ok(Self::Sha512),
            HashAlgorithm::Sm3 => Err(CryptoError::UnsupportedAlgorithm),
        }
    }
}

/// RSA signature padding selector.
#[derive(Clone, Debug, Copy)]
pub enum RsaSignPadding {
    Pkcs1v15,
    Pss,
}

/// RSA encryption padding selector.
#[derive(Clone, Debug, Copy)]
pub enum RsaEncPadding {
    Pkcs1v15,
    Oaep,
}

fn validate_digest(digest: &DigestBytes, hash_algo: RsaHashAlgo) -> Result<()> {
    let expected = hash_algo.hash_algorithm();
    if digest.algorithm() != expected {
        return Err(CryptoError::InvalidDigestAlgorithm);
    }
    if digest.as_bytes().len() != expected.output_size() {
        return Err(CryptoError::InvalidLength);
    }
    Ok(())
}

/// Convert big-endian bytes to BoxedUint with auto-detected bit precision.
fn bytes_to_uint(bytes: &[u8]) -> Result<BoxedUint> {
    let bits = (bytes.len() * 8) as u32;
    BoxedUint::from_be_slice(bytes, bits)
        .map_err(|_| CryptoError::Backend(BackendError::InvalidEncoding))
}

/// Construct an RSA private key from byte-level components.
///
/// When `p` and `q` are both non-empty, CRT factors are used. Otherwise the key
/// is built from `(n, e, d)` only, matching OP-TEE / GP TEE transient objects that
/// carry a private exponent without prime factors.
pub fn rsa_key_from_components(
    n: &[u8],
    e: &[u8],
    d: &[u8],
    p: &[u8],
    q: &[u8],
) -> Result<RsaKeypair> {
    let n = bytes_to_uint(n)?;
    let e = bytes_to_uint(e)?;
    let d = bytes_to_uint(d)?;
    let primes =
        if p.is_empty() || q.is_empty() || p.iter().all(|&b| b == 0) || q.iter().all(|&b| b == 0) {
            alloc::vec![]
        } else {
            let p = bytes_to_uint(p)?;
            let q = bytes_to_uint(q)?;
            alloc::vec![p, q]
        };
    rsa::RsaPrivateKey::from_components(n, e, d, primes)
        .map(RsaKeypair::from_private_key)
        .map_err(|_| CryptoError::Backend(BackendError::RsaKeyConstruction))
}

/// Construct an RSA private key from p, q, and public exponent.
pub fn rsa_key_from_p_q(e: &[u8], p: &[u8], q: &[u8]) -> Result<RsaKeypair> {
    let p = bytes_to_uint(p)?;
    let q = bytes_to_uint(q)?;
    let e = bytes_to_uint(e)?;
    rsa::RsaPrivateKey::from_p_q(p, q, e)
        .map(RsaKeypair::from_private_key)
        .map_err(|_| CryptoError::Backend(BackendError::RsaKeyConstruction))
}

/// Construct an RSA public key from n and e.
pub fn rsa_public_from_components(n: &[u8], e: &[u8]) -> Result<RsaPublic> {
    let n = bytes_to_uint(n)?;
    let e = bytes_to_uint(e)?;
    rsa::RsaPublicKey::new(n, e)
        .map(RsaPublic::from_public_key)
        .map_err(|_| CryptoError::Backend(BackendError::RsaPublicKey))
}

/// Parse an RSA public key from PKCS#1 DER bytes.
pub fn rsa_public_key_from_pkcs1_der(der: &[u8]) -> Result<RsaPublic> {
    use pkcs1::DecodeRsaPublicKey;
    rsa::RsaPublicKey::from_pkcs1_der(der)
        .map(RsaPublic::from_public_key)
        .map_err(|_| CryptoError::Backend(BackendError::RsaParseKey))
}

/// Parse an RSA private key from PKCS#8 DER bytes.
pub fn rsa_private_key_from_pkcs8_der(der: &[u8]) -> Result<RsaKeypair> {
    use pkcs8::DecodePrivateKey;
    rsa::RsaPrivateKey::from_pkcs8_der(der)
        .map(RsaKeypair::from_private_key)
        .map_err(|_| CryptoError::Backend(BackendError::RsaParseKey))
}

/// Parse an RSA private key from PKCS#1 DER bytes.
pub fn rsa_private_key_from_pkcs1_der(der: &[u8]) -> Result<RsaKeypair> {
    use pkcs1::DecodeRsaPrivateKey;
    rsa::RsaPrivateKey::from_pkcs1_der(der)
        .map(RsaKeypair::from_private_key)
        .map_err(|_| CryptoError::Backend(BackendError::RsaParseKey))
}

/// Extracted RSA key components in big-endian bytes.
pub struct RsaKeyComponents {
    pub n: BigEndianBytes,
    pub e: BigEndianBytes,
    pub d: SecretBytes,
    pub p: SecretBytes,
    pub q: SecretBytes,
    pub dp: SecretBytes,
    pub dq: SecretBytes,
    pub qp: SecretBytes,
}

/// Parse PKCS#8 DER and extract all RSA key components.
pub fn rsa_key_components_from_pkcs8_der(der: &[u8]) -> Result<RsaKeyComponents> {
    let pk = rsa_private_key_from_pkcs8_der(der)?;
    Ok(rsa_key_components(pk.as_inner()))
}

/// Parse PKCS#1 DER and extract all RSA key components.
pub fn rsa_key_components_from_pkcs1_der(der: &[u8]) -> Result<RsaKeyComponents> {
    let pk = rsa_private_key_from_pkcs1_der(der)?;
    Ok(rsa_key_components(pk.as_inner()))
}

fn rsa_key_components(pk: &rsa::RsaPrivateKey) -> RsaKeyComponents {
    let primes = pk.primes();
    RsaKeyComponents {
        n: BigEndianBytes::new(pk.n().to_be_bytes().into_vec()),
        e: BigEndianBytes::new(pk.e().to_be_bytes().into_vec()),
        d: SecretBytes::new(pk.d().to_be_bytes().into_vec()),
        p: primes
            .first()
            .map(|v| SecretBytes::new(v.to_be_bytes().into_vec()))
            .unwrap_or_default(),
        q: primes
            .get(1)
            .map(|v| SecretBytes::new(v.to_be_bytes().into_vec()))
            .unwrap_or_default(),
        dp: pk
            .dp()
            .map(|v| SecretBytes::new(v.to_be_bytes().into_vec()))
            .unwrap_or_default(),
        dq: pk
            .dq()
            .map(|v| SecretBytes::new(v.to_be_bytes().into_vec()))
            .unwrap_or_default(),
        qp: pk
            .crt_coefficient()
            .map(|v| SecretBytes::new(v.to_be_bytes().into_vec()))
            .unwrap_or_default(),
    }
}

/// Generate an RSA keypair with a specific public exponent.
pub fn rsa_keygen(rng: &mut dyn CryptoRng, key_size_bits: usize, exp: u32) -> Result<RsaKeypair> {
    let exp_uint = BoxedUint::from_be_slice(&exp.to_be_bytes(), 32)
        .map_err(|_| CryptoError::Backend(BackendError::InvalidExponent))?;
    // OP-TEE xtest generates 256–896 bit RSA keys (mbedtls allows this).
    // The `rsa` crate enforces 1024-bit minimum unless hazmat unchecked API is used.
    let key = if key_size_bits < 1024 {
        rsa::RsaPrivateKey::new_with_exp_unchecked(rng, key_size_bits, exp_uint)
    } else {
        rsa::RsaPrivateKey::new_with_exp(rng, key_size_bits, exp_uint)
    };
    key.map(RsaKeypair::from_private_key)
        .map_err(|_| CryptoError::Backend(BackendError::RsaKeygen))
}

/// Get the modulus bytes (big-endian) from a private key.
pub fn rsa_get_n(key: &RsaKeypair) -> Vec<u8> {
    key.as_inner().n().as_ref().to_be_bytes().to_vec()
}

/// Get the public exponent bytes (big-endian).
pub fn rsa_get_e(key: &RsaKeypair) -> Vec<u8> {
    key.as_inner().e().to_be_bytes().to_vec()
}

/// Get the private exponent bytes (big-endian).
pub fn rsa_get_d(key: &RsaKeypair) -> SecretBytes {
    SecretBytes::new(key.as_inner().d().to_be_bytes().to_vec())
}

/// Get the prime factors as secret big-endian byte vectors.
pub fn rsa_get_primes(key: &RsaKeypair) -> Vec<SecretBytes> {
    key.as_inner()
        .primes()
        .iter()
        .map(|p| SecretBytes::new(p.to_be_bytes().to_vec()))
        .collect()
}

/// Get dp (d mod p-1) as big-endian bytes.
pub fn rsa_get_dp(key: &RsaKeypair) -> SecretBytes {
    key.as_inner()
        .dp()
        .map(|v| SecretBytes::new(v.to_be_bytes().to_vec()))
        .unwrap_or_default()
}

/// Get dq (d mod q-1) as big-endian bytes.
pub fn rsa_get_dq(key: &RsaKeypair) -> SecretBytes {
    key.as_inner()
        .dq()
        .map(|v| SecretBytes::new(v.to_be_bytes().to_vec()))
        .unwrap_or_default()
}

/// Get qinv (q^-1 mod p) as big-endian bytes.
pub fn rsa_get_qinv(key: &RsaKeypair) -> SecretBytes {
    key.as_inner()
        .qinv()
        .map(|v| SecretBytes::new(v.retrieve().to_be_bytes().to_vec()))
        .unwrap_or_default()
}

/// RSA sign with configurable hash and padding.
/// `msg` should be the pre-hashed message digest.
/// For PSS, `pss_salt_len` overrides the default (hash output size) when set.
pub fn rsa_sign(
    key: &RsaKeypair,
    hash_algo: RsaHashAlgo,
    padding: RsaSignPadding,
    digest: &DigestBytes,
    rng: &mut dyn CryptoRng,
    pss_salt_len: Option<usize>,
) -> Result<SignatureBytes> {
    validate_digest(digest, hash_algo)?;
    let sig = match padding {
        RsaSignPadding::Pkcs1v15 => sign_pkcs1v15(key.as_inner(), hash_algo, digest.as_bytes()),
        RsaSignPadding::Pss => sign_pss(
            key.as_inner(),
            hash_algo,
            digest.as_bytes(),
            rng,
            pss_salt_len,
        ),
    }?;
    Ok(SignatureBytes::new(
        sig,
        signature_algorithm(padding),
        SignatureEncoding::Raw,
    ))
}

/// Verify an RSA signature using a PKCS#1 DER-encoded public key.
pub fn rsa_verify_pkcs1_public_der(
    der: &[u8],
    hash_algo: RsaHashAlgo,
    padding: RsaSignPadding,
    digest: &DigestBytes,
    signature: &SignatureBytes,
) -> Result<()> {
    let key = rsa_public_key_from_pkcs1_der(der)?;
    rsa_verify(&key, hash_algo, padding, digest, signature)
}

/// RSA verify with configurable hash and padding.
pub fn rsa_verify(
    key: &RsaPublic,
    hash_algo: RsaHashAlgo,
    padding: RsaSignPadding,
    digest: &DigestBytes,
    signature: &SignatureBytes,
) -> Result<()> {
    validate_digest(digest, hash_algo)?;
    if signature.algorithm() != signature_algorithm(padding) {
        return Err(CryptoError::AlgorithmMismatch);
    }
    if signature.encoding() != SignatureEncoding::Raw {
        return Err(CryptoError::InvalidSignatureEncoding);
    }
    match padding {
        RsaSignPadding::Pkcs1v15 => verify_pkcs1v15(
            key.as_inner(),
            hash_algo,
            digest.as_bytes(),
            signature.as_bytes(),
        ),
        RsaSignPadding::Pss => verify_pss(
            key.as_inner(),
            hash_algo,
            digest.as_bytes(),
            signature.as_bytes(),
        ),
    }
}

fn signature_algorithm(padding: RsaSignPadding) -> SignatureAlgorithm {
    match padding {
        RsaSignPadding::Pkcs1v15 => SignatureAlgorithm::RsaPkcs1v15,
        RsaSignPadding::Pss => SignatureAlgorithm::RsaPss,
    }
}

macro_rules! dispatch_hash_sign_pkcs1v15 {
    ($key:expr, $hash:ident, $msg:expr) => {
        match $hash {
            RsaHashAlgo::Md5 => {
                let scheme = rsa::pkcs1v15::Pkcs1v15Sign::new_unprefixed();
                $key.sign(scheme, $msg)
            }
            RsaHashAlgo::Sha1 => {
                let scheme = rsa::pkcs1v15::Pkcs1v15Sign::new::<sha1::Sha1>();
                $key.sign(scheme, $msg)
            }
            RsaHashAlgo::Sha224 => {
                let scheme = rsa::pkcs1v15::Pkcs1v15Sign::new::<sha2::Sha224>();
                $key.sign(scheme, $msg)
            }
            RsaHashAlgo::Sha256 => {
                let scheme = rsa::pkcs1v15::Pkcs1v15Sign::new::<sha2::Sha256>();
                $key.sign(scheme, $msg)
            }
            RsaHashAlgo::Sha384 => {
                let scheme = rsa::pkcs1v15::Pkcs1v15Sign::new::<sha2::Sha384>();
                $key.sign(scheme, $msg)
            }
            RsaHashAlgo::Sha512 => {
                let scheme = rsa::pkcs1v15::Pkcs1v15Sign::new::<sha2::Sha512>();
                $key.sign(scheme, $msg)
            }
        }
    };
}

fn sign_pkcs1v15(key: &rsa::RsaPrivateKey, hash_algo: RsaHashAlgo, msg: &[u8]) -> Result<Vec<u8>> {
    dispatch_hash_sign_pkcs1v15!(key, hash_algo, msg)
        .map_err(|_| CryptoError::Backend(BackendError::RsaSign))
}

fn sign_pss(
    key: &rsa::RsaPrivateKey,
    hash_algo: RsaHashAlgo,
    msg: &[u8],
    rng: &mut dyn CryptoRng,
    salt_len: Option<usize>,
) -> Result<Vec<u8>> {
    let salt_len = salt_len.unwrap_or_else(|| hash_algo.hash_algorithm().output_size());
    let result = match hash_algo {
        RsaHashAlgo::Md5 => {
            let scheme = rsa::pss::Pss::<md5::Md5>::new_with_salt(salt_len);
            key.sign_with_rng(rng, scheme, msg)
        }
        RsaHashAlgo::Sha1 => {
            let scheme = rsa::pss::Pss::<sha1::Sha1>::new_with_salt(salt_len);
            key.sign_with_rng(rng, scheme, msg)
        }
        RsaHashAlgo::Sha224 => {
            let scheme = rsa::pss::Pss::<sha2::Sha224>::new_with_salt(salt_len);
            key.sign_with_rng(rng, scheme, msg)
        }
        RsaHashAlgo::Sha256 => {
            let scheme = rsa::pss::Pss::<sha2::Sha256>::new_with_salt(salt_len);
            key.sign_with_rng(rng, scheme, msg)
        }
        RsaHashAlgo::Sha384 => {
            let scheme = rsa::pss::Pss::<sha2::Sha384>::new_with_salt(salt_len);
            key.sign_with_rng(rng, scheme, msg)
        }
        RsaHashAlgo::Sha512 => {
            let scheme = rsa::pss::Pss::<sha2::Sha512>::new_with_salt(salt_len);
            key.sign_with_rng(rng, scheme, msg)
        }
    };
    result.map_err(|_| CryptoError::Backend(BackendError::RsaSign))
}

fn pss_scheme_for_verify<D: digest::Digest + digest::FixedOutputReset + Default>()
-> rsa::pss::Pss<D> {
    rsa::pss::Pss {
        blinded: false,
        digest: D::default(),
        // OP-TEE vectors may use a salt length different from the hash size
        // (e.g. SHA-224 with 20-byte salt). Auto-detect from the signature.
        salt_len: None,
    }
}

fn verify_pkcs1v15(
    key: &rsa::RsaPublicKey,
    hash_algo: RsaHashAlgo,
    msg: &[u8],
    sig: &[u8],
) -> Result<()> {
    match hash_algo {
        RsaHashAlgo::Md5 => {
            let scheme = rsa::pkcs1v15::Pkcs1v15Sign::new_unprefixed();
            key.verify(scheme, msg, sig)
        }
        RsaHashAlgo::Sha1 => {
            let scheme = rsa::pkcs1v15::Pkcs1v15Sign::new::<sha1::Sha1>();
            key.verify(scheme, msg, sig)
        }
        RsaHashAlgo::Sha224 => {
            let scheme = rsa::pkcs1v15::Pkcs1v15Sign::new::<sha2::Sha224>();
            key.verify(scheme, msg, sig)
        }
        RsaHashAlgo::Sha256 => {
            let scheme = rsa::pkcs1v15::Pkcs1v15Sign::new::<sha2::Sha256>();
            key.verify(scheme, msg, sig)
        }
        RsaHashAlgo::Sha384 => {
            let scheme = rsa::pkcs1v15::Pkcs1v15Sign::new::<sha2::Sha384>();
            key.verify(scheme, msg, sig)
        }
        RsaHashAlgo::Sha512 => {
            let scheme = rsa::pkcs1v15::Pkcs1v15Sign::new::<sha2::Sha512>();
            key.verify(scheme, msg, sig)
        }
    }
    .map_err(|_| CryptoError::VerificationFailed)
}

fn verify_pss(
    key: &rsa::RsaPublicKey,
    hash_algo: RsaHashAlgo,
    msg: &[u8],
    sig: &[u8],
) -> Result<()> {
    let result = match hash_algo {
        RsaHashAlgo::Md5 => {
            let scheme = pss_scheme_for_verify::<md5::Md5>();
            key.verify(scheme, msg, sig)
        }
        RsaHashAlgo::Sha1 => {
            let scheme = pss_scheme_for_verify::<sha1::Sha1>();
            key.verify(scheme, msg, sig)
        }
        RsaHashAlgo::Sha224 => {
            let scheme = pss_scheme_for_verify::<sha2::Sha224>();
            key.verify(scheme, msg, sig)
        }
        RsaHashAlgo::Sha256 => {
            let scheme = pss_scheme_for_verify::<sha2::Sha256>();
            key.verify(scheme, msg, sig)
        }
        RsaHashAlgo::Sha384 => {
            let scheme = pss_scheme_for_verify::<sha2::Sha384>();
            key.verify(scheme, msg, sig)
        }
        RsaHashAlgo::Sha512 => {
            let scheme = pss_scheme_for_verify::<sha2::Sha512>();
            key.verify(scheme, msg, sig)
        }
    };
    result.map_err(|_| CryptoError::VerificationFailed)
}

/// RSA encrypt with PKCS#1 v1.5 padding.
pub fn rsa_encrypt_pkcs1v15(
    key: &RsaPublic,
    msg: &[u8],
    rng: &mut dyn CryptoRng,
) -> Result<CiphertextBytes> {
    let ciphertext = key
        .as_inner()
        .encrypt(rng, rsa::pkcs1v15::Pkcs1v15Encrypt, msg)
        .map_err(|_| CryptoError::Backend(BackendError::RsaEncrypt))?;
    Ok(CiphertextBytes::new(
        ciphertext,
        CiphertextAlgorithm::RsaPkcs1v15,
    ))
}

/// RSA decrypt with PKCS#1 v1.5 padding.
pub fn rsa_decrypt_pkcs1v15(
    key: &RsaKeypair,
    ciphertext: &CiphertextBytes,
) -> Result<PlaintextBytes> {
    if ciphertext.algorithm() != CiphertextAlgorithm::RsaPkcs1v15 {
        return Err(CryptoError::InvalidInput);
    }
    let plaintext = key
        .as_inner()
        .decrypt(rsa::pkcs1v15::Pkcs1v15Encrypt, ciphertext.as_bytes())
        .map_err(|_| CryptoError::Backend(BackendError::RsaDecrypt))?;
    Ok(PlaintextBytes::new(plaintext))
}

/// RSA encrypt with OAEP padding and configurable hash.
pub fn rsa_encrypt_oaep(
    key: &RsaPublic,
    hash_algo: RsaHashAlgo,
    label: &[u8],
    msg: &[u8],
    rng: &mut dyn CryptoRng,
) -> Result<CiphertextBytes> {
    let ciphertext = match hash_algo {
        RsaHashAlgo::Sha1 => {
            let padding = rsa::oaep::Oaep::<sha1::Sha1>::new_with_label(label);
            key.as_inner().encrypt(rng, padding, msg)
        }
        RsaHashAlgo::Sha256 => {
            let padding = rsa::oaep::Oaep::<sha2::Sha256>::new_with_label(label);
            key.as_inner().encrypt(rng, padding, msg)
        }
        RsaHashAlgo::Sha384 => {
            let padding = rsa::oaep::Oaep::<sha2::Sha384>::new_with_label(label);
            key.as_inner().encrypt(rng, padding, msg)
        }
        RsaHashAlgo::Sha512 => {
            let padding = rsa::oaep::Oaep::<sha2::Sha512>::new_with_label(label);
            key.as_inner().encrypt(rng, padding, msg)
        }
        RsaHashAlgo::Md5 => {
            let padding = rsa::oaep::Oaep::<sha2::Sha256>::new_with_label(label);
            key.as_inner().encrypt(rng, padding, msg)
        }
        RsaHashAlgo::Sha224 => {
            let padding = rsa::oaep::Oaep::<sha2::Sha224>::new_with_label(label);
            key.as_inner().encrypt(rng, padding, msg)
        }
    }
    .map_err(|_| CryptoError::Backend(BackendError::RsaEncrypt))?;
    Ok(CiphertextBytes::new(
        ciphertext,
        CiphertextAlgorithm::RsaOaep,
    ))
}

/// RSA decrypt with OAEP padding and configurable hash.
pub fn rsa_decrypt_oaep(
    key: &RsaKeypair,
    hash_algo: RsaHashAlgo,
    label: &[u8],
    ciphertext: &CiphertextBytes,
) -> Result<PlaintextBytes> {
    if ciphertext.algorithm() != CiphertextAlgorithm::RsaOaep {
        return Err(CryptoError::InvalidInput);
    }
    let plaintext = match hash_algo {
        RsaHashAlgo::Sha1 => {
            let padding = rsa::oaep::Oaep::<sha1::Sha1>::new_with_label(label);
            key.as_inner().decrypt(padding, ciphertext.as_bytes())
        }
        RsaHashAlgo::Sha256 => {
            let padding = rsa::oaep::Oaep::<sha2::Sha256>::new_with_label(label);
            key.as_inner().decrypt(padding, ciphertext.as_bytes())
        }
        RsaHashAlgo::Sha384 => {
            let padding = rsa::oaep::Oaep::<sha2::Sha384>::new_with_label(label);
            key.as_inner().decrypt(padding, ciphertext.as_bytes())
        }
        RsaHashAlgo::Sha512 => {
            let padding = rsa::oaep::Oaep::<sha2::Sha512>::new_with_label(label);
            key.as_inner().decrypt(padding, ciphertext.as_bytes())
        }
        RsaHashAlgo::Md5 => {
            let padding = rsa::oaep::Oaep::<sha2::Sha256>::new_with_label(label);
            key.as_inner().decrypt(padding, ciphertext.as_bytes())
        }
        RsaHashAlgo::Sha224 => {
            let padding = rsa::oaep::Oaep::<sha2::Sha224>::new_with_label(label);
            key.as_inner().decrypt(padding, ciphertext.as_bytes())
        }
    }
    .map_err(|_| CryptoError::Backend(BackendError::RsaDecrypt))?;
    Ok(PlaintextBytes::new(plaintext))
}

/// Strip leading zero bytes from a raw RSA block (OP-TEE rsanopad semantics).
fn rsa_nopad_out_len(buf: &[u8]) -> usize {
    let mod_size = buf.len();
    let mut offset = 0usize;
    while offset < mod_size.saturating_sub(1) && buf[offset] == 0 {
        offset += 1;
    }
    mod_size - offset
}

/// Raw RSA public operation in-place on a modulus-sized block.
pub fn rsa_nopad_public_in_place(key: &RsaPublic, block: &mut [u8]) -> Result<()> {
    let m = bytes_to_uint(block)?;
    let n = key.as_inner().n();
    if m >= *n.as_ref() {
        return Err(CryptoError::InvalidInput);
    }
    let result = rsa::hazmat::rsa_encrypt(key.as_inner(), &m)
        .map_err(|_| CryptoError::Backend(BackendError::RsaRawPublic))?;
    let be = result.to_be_bytes();
    let block_len = block.len();
    if be.len() > block_len {
        return Err(CryptoError::InvalidInput);
    }
    block.fill(0);
    block[block_len - be.len()..].copy_from_slice(&be);
    Ok(())
}

/// Raw RSA private operation in-place on a modulus-sized block.
pub fn rsa_nopad_private_in_place(key: &RsaKeypair, block: &mut [u8]) -> Result<()> {
    let m = bytes_to_uint(block)?;
    let n = key.as_inner().n();
    if m >= *n.as_ref() {
        return Err(CryptoError::InvalidInput);
    }
    let result = rsa::hazmat::rsa_decrypt(None::<&mut dyn CryptoRng>, key.as_inner(), &m)
        .map_err(|_| CryptoError::Backend(BackendError::RsaRawPrivate))?;
    let be = result.to_be_bytes();
    let block_len = block.len();
    if be.len() > block_len {
        return Err(CryptoError::InvalidInput);
    }
    block.fill(0);
    block[block_len - be.len()..].copy_from_slice(&be);
    Ok(())
}

/// RSA NOPAD encrypt: left-pad `src` to modulus width, public op, strip leading zeros.
pub fn rsa_nopad_encrypt(key: &RsaPublic, src: &[u8], dst: &mut [u8]) -> Result<usize> {
    let mod_size = key.as_inner().size();
    if mod_size == 0 || src.len() > mod_size {
        return Err(CryptoError::InvalidInput);
    }
    let mut buf = alloc::vec![0u8; mod_size];
    buf[mod_size - src.len()..].copy_from_slice(src);
    rsa_nopad_public_in_place(key, &mut buf)?;
    let offset = mod_size - rsa_nopad_out_len(&buf);
    let out_len = mod_size - offset;
    if dst.len() < out_len {
        return Err(CryptoError::InvalidLength);
    }
    dst[..out_len].copy_from_slice(&buf[offset..offset + out_len]);
    Ok(out_len)
}

/// RSA NOPAD decrypt: left-pad `src` to modulus width, private op, strip leading zeros.
pub fn rsa_nopad_decrypt(key: &RsaKeypair, src: &[u8], dst: &mut [u8]) -> Result<usize> {
    let mod_size = key.as_inner().size();
    if mod_size == 0 || src.len() > mod_size {
        return Err(CryptoError::InvalidInput);
    }
    let mut buf = alloc::vec![0u8; mod_size];
    buf[mod_size - src.len()..].copy_from_slice(src);
    rsa_nopad_private_in_place(key, &mut buf)?;
    let offset = mod_size - rsa_nopad_out_len(&buf);
    let out_len = mod_size - offset;
    if dst.len() < out_len {
        return Err(CryptoError::InvalidLength);
    }
    dst[..out_len].copy_from_slice(&buf[offset..offset + out_len]);
    Ok(out_len)
}

/// Raw RSA private operation (no padding) using hazmat.
pub fn rsa_raw_private(key: &RsaKeypair, msg: &[u8]) -> Result<Vec<u8>> {
    let m = bytes_to_uint(msg)?;
    let key = key.as_inner();
    let n = key.n();
    if m >= *n.as_ref() {
        return Err(CryptoError::InvalidInput);
    }
    let result = rsa::hazmat::rsa_decrypt(None::<&mut dyn CryptoRng>, key, &m)
        .map_err(|_| CryptoError::Backend(BackendError::RsaRawPrivate))?;
    Ok(result.to_be_bytes().to_vec())
}

/// Raw RSA public operation (no padding).
pub fn rsa_raw_public(key: &RsaPublic, msg: &[u8]) -> Result<Vec<u8>> {
    let m = bytes_to_uint(msg)?;
    let key = key.as_inner();
    let n = key.n();
    if m >= *n.as_ref() {
        return Err(CryptoError::InvalidInput);
    }
    let result = rsa::hazmat::rsa_encrypt(key, &m)
        .map_err(|_| CryptoError::Backend(BackendError::RsaRawPublic))?;
    Ok(result.to_be_bytes().to_vec())
}

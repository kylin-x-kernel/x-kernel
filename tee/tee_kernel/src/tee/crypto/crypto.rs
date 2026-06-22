// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, format, sync::Arc, vec, vec::Vec};
use core::{default::Default, fmt, fmt::Debug};

use ksync::Mutex;
use tee_crypto::{
    asymmetric::EccCurve,
    hash::{Digest, DigestBytes, HashAlgorithm, Sha1, Sha224, Sha256, Sha384, Sha512, Sm3},
    mac::{
        Aes128Cmac, Aes192Cmac, Aes256Cmac, Des3Cmac, HmacMd5, HmacSha1, HmacSha224, HmacSha256,
        HmacSha384, HmacSha512, HmacSm3, Mac, Sm4Cmac,
    },
    material::{
        CiphertextAlgorithm, CiphertextBytes, SignatureAlgorithm, SignatureBytes, SignatureEncoding,
    },
    md5::Md5,
    streaming_cipher::{Direction, PaddingMode, StreamingCipherAlgo, StreamingCipherCtx},
    tee_ops::{
        ecc::{self as ecc_ops, EccHashAlgo},
        rsa::{self as rsa_ops, RsaEncPadding, RsaHashAlgo, RsaSignPadding},
    },
};
use tee_raw_sys::*;

use crate::tee::{
    TEE_ALG_DES3_CMAC, TEE_ALG_RSAES_PKCS1_OAEP_MGF1_MD5, TEE_ALG_RSASSA_PKCS1_PSS_MGF1_MD5,
    TeeResult,
    crypto::{
        bignum::{BigNum, crypto_bignum_allocate},
        crypto_impl::{
            CryptoEccKeypairOpsGenerate, EccComKeyPair, EccKeypairOpsCtx, Sm2DsaKeyPair,
            Sm2KepKeyPair, Sm2PkeKeyPair,
        },
    },
    rng_software::TeeSoftwareRng,
    tee_api_defines_extensions::TEE_ALG_SM4_XTS,
    tee_obj::{TeeObjIdType, tee_obj_get},
    tee_svc_cryp::{CryptoAttrRef, TeeCryptObj, TeeCryptoOps},
    tee_svc_cryp2::{
        CipherPaddingMode, CmacContext, CrypCtx, CrypState, HashContext, HmacContext, TeeCrypState,
    },
};

/// Asymmetric context storing raw key components and algorithm metadata.
/// Replaces mbedtls Pk for RSA/ECC operations.
#[derive(Clone)]
pub(crate) enum AsymmetricCtx {
    /// RSA public key (n, e)
    RsaPublic { n: Vec<u8>, e: Vec<u8> },
    /// RSA private key (n, e, d, p, q)
    RsaPrivate {
        n: Vec<u8>,
        e: Vec<u8>,
        d: Vec<u8>,
        p: Vec<u8>,
        q: Vec<u8>,
    },
    /// ECC public key (x, y) on a specific curve
    EccPublic {
        curve: EccCurve,
        x: Vec<u8>,
        y: Vec<u8>,
    },
    /// ECC private key (secret scalar) on a specific curve
    EccPrivate { curve: EccCurve, secret: Vec<u8> },
}

/// Helper: extract big-endian bytes from a BigNum (minimal width, no leading zeros).
fn bn_to_bytes(bn: &BigNum) -> Vec<u8> {
    bn.to_bytes().unwrap_or_default()
}

fn ecc_curve_field_byte_len(curve: EccCurve) -> usize {
    match curve {
        EccCurve::P192 => 24,
        EccCurve::P224 => 28,
        EccCurve::P256 => 32,
        EccCurve::P384 => 48,
        EccCurve::P521 => 66,
        EccCurve::Sm2 => 32,
    }
}

/// Big-endian field element bytes fixed to the curve's coordinate/scalar width.
fn bn_to_ecc_field_bytes(bn: &BigNum, field_len: usize) -> Vec<u8> {
    let bytes = bn_to_bytes(bn);
    if bytes.len() >= field_len {
        bytes[bytes.len() - field_len..].to_vec()
    } else {
        let mut out = alloc::vec![0u8; field_len];
        out[field_len - bytes.len()..].copy_from_slice(&bytes);
        out
    }
}

fn bignum_is_present(bn: &BigNum) -> bool {
    bn.bit_length() > 0
}

fn rsa_bn_to_secret_bytes(bn: &BigNum) -> Vec<u8> {
    if bignum_is_present(bn) {
        bn_to_bytes(bn)
    } else {
        Vec::new()
    }
}

#[inline]
fn vec_as_bytes(v: &Vec<u8>) -> &[u8] {
    v.as_slice()
}

fn check_rsa_modulus_output(mod_size: usize, output_len: usize, required: &mut usize) -> TeeResult {
    *required = mod_size;
    if output_len < mod_size {
        Err(TEE_ERROR_SHORT_BUFFER)
    } else {
        Ok(())
    }
}

fn ecc_max_signature_len(curve: EccCurve, algo: u32) -> usize {
    if algo == TEE_ALG_SM2_DSA_SM3 {
        return 64;
    }
    match curve {
        EccCurve::P192 => 48,
        EccCurve::P224 => 56,
        EccCurve::P256 => 72,
        EccCurve::P384 => 104,
        EccCurve::P521 => 139,
        EccCurve::Sm2 => 64,
    }
}

fn ecc_raw_signature_len(curve: EccCurve) -> usize {
    match curve {
        EccCurve::P192 => 48,
        EccCurve::P224 => 56,
        EccCurve::P256 => 64,
        EccCurve::P384 => 96,
        EccCurve::P521 => 132,
        EccCurve::Sm2 => 64,
    }
}

/// Helper: map TEE algorithm to RsaHashAlgo.
fn algo_to_rsa_hash(algo: u32) -> RsaHashAlgo {
    match algo {
        TEE_ALG_RSASSA_PKCS1_V1_5_MD5
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_MD5
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_MD5 => RsaHashAlgo::Md5,
        TEE_ALG_RSASSA_PKCS1_V1_5_SHA1
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA1
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA1 => RsaHashAlgo::Sha1,
        TEE_ALG_RSASSA_PKCS1_V1_5_SHA224
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA224
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA224 => RsaHashAlgo::Sha224,
        TEE_ALG_RSASSA_PKCS1_V1_5_SHA256
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA256
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA256 => RsaHashAlgo::Sha256,
        TEE_ALG_RSASSA_PKCS1_V1_5_SHA384
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA384
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA384 => RsaHashAlgo::Sha384,
        TEE_ALG_RSASSA_PKCS1_V1_5_SHA512
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA512
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA512 => RsaHashAlgo::Sha512,
        _ => RsaHashAlgo::Sha256,
    }
}

fn rsa_hash_to_hash_algorithm(hash_algo: RsaHashAlgo) -> HashAlgorithm {
    match hash_algo {
        RsaHashAlgo::Md5 => HashAlgorithm::Md5,
        RsaHashAlgo::Sha1 => HashAlgorithm::Sha1,
        RsaHashAlgo::Sha224 => HashAlgorithm::Sha224,
        RsaHashAlgo::Sha256 => HashAlgorithm::Sha256,
        RsaHashAlgo::Sha384 => HashAlgorithm::Sha384,
        RsaHashAlgo::Sha512 => HashAlgorithm::Sha512,
    }
}

fn ecc_hash_to_hash_algorithm(hash_algo: EccHashAlgo) -> HashAlgorithm {
    match hash_algo {
        EccHashAlgo::Sha1 => HashAlgorithm::Sha1,
        EccHashAlgo::Sha224 => HashAlgorithm::Sha224,
        EccHashAlgo::Sha256 => HashAlgorithm::Sha256,
        EccHashAlgo::Sha384 => HashAlgorithm::Sha384,
        EccHashAlgo::Sha512 => HashAlgorithm::Sha512,
        EccHashAlgo::Sm3 => HashAlgorithm::Sm3,
    }
}

fn rsa_digest_from_tee(hash_algo: RsaHashAlgo, digest: &[u8]) -> DigestBytes {
    DigestBytes::new(digest.to_vec(), rsa_hash_to_hash_algorithm(hash_algo))
}

fn ecc_digest_from_tee(hash_algo: EccHashAlgo, digest: &[u8]) -> DigestBytes {
    DigestBytes::new(digest.to_vec(), ecc_hash_to_hash_algorithm(hash_algo))
}

fn signature_from_tee(
    signature: &[u8],
    algorithm: SignatureAlgorithm,
    encoding: SignatureEncoding,
) -> SignatureBytes {
    SignatureBytes::new(signature.to_vec(), algorithm, encoding)
}

fn ciphertext_from_tee(ciphertext: &[u8], algorithm: CiphertextAlgorithm) -> CiphertextBytes {
    CiphertextBytes::new(ciphertext.to_vec(), algorithm)
}

/// Helper: map TEE algorithm to RsaSignPadding.
fn algo_to_sign_padding(algo: u32) -> RsaSignPadding {
    match algo {
        TEE_ALG_RSASSA_PKCS1_PSS_MGF1_MD5
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA1
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA224
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA256
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA384
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA512 => RsaSignPadding::Pss,
        _ => RsaSignPadding::Pkcs1v15,
    }
}

/// Helper: map TEE algorithm to RsaEncPadding.
fn algo_to_enc_padding(algo: u32) -> RsaEncPadding {
    match algo {
        TEE_ALG_RSAES_PKCS1_V1_5 => RsaEncPadding::Pkcs1v15,
        _ => RsaEncPadding::Oaep,
    }
}

/// Helper: map TEE curve constant to EccCurve.
fn tee_curve_to_ecc_curve(curve: u32) -> TeeResult<EccCurve> {
    match curve {
        TEE_ECC_CURVE_NIST_P192 => Ok(EccCurve::P192),
        TEE_ECC_CURVE_NIST_P224 => Ok(EccCurve::P224),
        TEE_ECC_CURVE_NIST_P256 => Ok(EccCurve::P256),
        TEE_ECC_CURVE_NIST_P384 => Ok(EccCurve::P384),
        TEE_ECC_CURVE_NIST_P521 => Ok(EccCurve::P521),
        TEE_ECC_CURVE_SM2 => Ok(EccCurve::Sm2),
        _ => Err(TEE_ERROR_NOT_SUPPORTED),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
/// GP: `struct ecc_public_key`
pub struct EccPublicKey {
    pub x: BigNum,
    pub y: BigNum,
    curve: u32,
    // ops: Box<dyn crypto_ecc_public_ops>,
}

impl TeeCryptoOps for EccPublicKey {
    fn new(key_type: u32, key_size_bits: usize) -> TeeResult<Self> {
        let mut curve = 0;
        match key_type {
            TEE_TYPE_SM2_DSA_PUBLIC_KEY
            | TEE_TYPE_SM2_PKE_PUBLIC_KEY
            | TEE_TYPE_SM2_KEP_PUBLIC_KEY => {
                curve = TEE_ECC_CURVE_SM2;
            }
            _ => {}
        };

        Ok(EccPublicKey {
            x: crypto_bignum_allocate(key_size_bits)?,
            y: crypto_bignum_allocate(key_size_bits)?,
            curve,
        })
    }

    fn get_attr_by_id(&mut self, attr_id: TeeObjIdType) -> TeeResult<CryptoAttrRef<'_>> {
        match attr_id as u32 {
            TEE_ATTR_ECC_PUBLIC_VALUE_X => Ok(CryptoAttrRef::BigNum(&mut self.x)),
            TEE_ATTR_ECC_PUBLIC_VALUE_Y => Ok(CryptoAttrRef::BigNum(&mut self.y)),
            TEE_ATTR_ECC_CURVE => Ok(CryptoAttrRef::U32(&mut self.curve)),
            _ => Err(TEE_ERROR_ITEM_NOT_FOUND),
        }
    }
}
#[derive(Default)]
/// GP: `struct ecc_keypair`
pub struct EccKeypair {
    pub d: BigNum,
    pub x: BigNum,
    pub y: BigNum,
    pub curve: u32,
    // TODO: add ops
    // pub ops: Box<dyn crypto_ecc_keypair_ops>,
}

impl Debug for EccKeypair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EccKeypair")
            .field("d", &self.d)
            .field("x", &self.x)
            .field("y", &self.y)
            .field("curve", &format!("{:#010X?}", self.curve))
            .finish()
    }
}

impl TeeCryptoOps for EccKeypair {
    fn new(key_type: u32, key_size_bits: usize) -> TeeResult<Self> {
        let mut curve = 0;

        match key_type {
            TEE_TYPE_ECDSA_KEYPAIR | TEE_TYPE_ECDH_KEYPAIR => {}
            TEE_TYPE_SM2_DSA_KEYPAIR | TEE_TYPE_SM2_PKE_KEYPAIR | TEE_TYPE_SM2_KEP_KEYPAIR => {
                curve = TEE_ECC_CURVE_SM2;
            }
            _ => return Err(TEE_ERROR_NOT_IMPLEMENTED),
        }

        Ok(EccKeypair {
            d: crypto_bignum_allocate(key_size_bits)?,
            x: crypto_bignum_allocate(key_size_bits)?,
            y: crypto_bignum_allocate(key_size_bits)?,
            curve,
            // ops,
        })
    }

    fn get_attr_by_id(&mut self, attr_id: TeeObjIdType) -> TeeResult<CryptoAttrRef<'_>> {
        match attr_id as u32 {
            TEE_ATTR_ECC_PRIVATE_VALUE => Ok(CryptoAttrRef::BigNum(&mut self.d)),
            TEE_ATTR_ECC_PUBLIC_VALUE_X => Ok(CryptoAttrRef::BigNum(&mut self.x)),
            TEE_ATTR_ECC_PUBLIC_VALUE_Y => Ok(CryptoAttrRef::BigNum(&mut self.y)),
            TEE_ATTR_ECC_CURVE => Ok(CryptoAttrRef::U32(&mut self.curve)),
            _ => Err(TEE_ERROR_ITEM_NOT_FOUND),
        }
    }
}

impl PartialEq for EccKeypair {
    fn eq(&self, other: &Self) -> bool {
        self.d == other.d && self.x == other.x && self.y == other.y && self.curve == other.curve
    }
}

impl Eq for EccKeypair {}

/// GP: `struct rsa_keypair`
pub struct RsaKeypair {
    pub e: BigNum, // Public exponent
    pub d: BigNum, // Private exponent
    pub n: BigNum, // Modulus

    // Optional CRT parameters (all NULL if unused)
    pub p: BigNum, // N = pq
    pub q: BigNum,
    pub qp: BigNum, // 1/q mod p
    pub dp: BigNum, // d mod (p-1)
    pub dq: BigNum, // d mod (q-1)
}

impl Debug for RsaKeypair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RsaKeypair")
            .field("e", &self.e)
            .field("d", &self.d)
            .field("n", &self.n)
            .field("p", &self.p)
            .field("q", &self.q)
            .field("qp", &self.qp)
            .field("dp", &self.dp)
            .field("dq", &self.dq)
            .finish()
    }
}

impl TeeCryptoOps for RsaKeypair {
    fn new(_key_type: u32, key_size_bits: usize) -> TeeResult<Self> {
        Ok(RsaKeypair {
            e: crypto_bignum_allocate(key_size_bits)?,
            d: crypto_bignum_allocate(key_size_bits)?,
            n: crypto_bignum_allocate(key_size_bits)?,
            p: BigNum::default(),
            q: BigNum::default(),
            qp: BigNum::default(),
            dp: BigNum::default(),
            dq: BigNum::default(),
        })
    }

    fn get_attr_by_id(&mut self, attr_id: TeeObjIdType) -> TeeResult<CryptoAttrRef<'_>> {
        match attr_id as u32 {
            TEE_ATTR_RSA_MODULUS => Ok(CryptoAttrRef::BigNum(&mut self.n)),
            TEE_ATTR_RSA_PUBLIC_EXPONENT => Ok(CryptoAttrRef::BigNum(&mut self.e)),
            TEE_ATTR_RSA_PRIVATE_EXPONENT => Ok(CryptoAttrRef::BigNum(&mut self.d)),
            TEE_ATTR_RSA_PRIME1 => Ok(CryptoAttrRef::BigNum(&mut self.p)),
            TEE_ATTR_RSA_PRIME2 => Ok(CryptoAttrRef::BigNum(&mut self.q)),
            TEE_ATTR_RSA_EXPONENT1 => Ok(CryptoAttrRef::BigNum(&mut self.dp)),
            TEE_ATTR_RSA_EXPONENT2 => Ok(CryptoAttrRef::BigNum(&mut self.dq)),
            TEE_ATTR_RSA_COEFFICIENT => Ok(CryptoAttrRef::BigNum(&mut self.qp)),
            _ => Err(TEE_ERROR_ITEM_NOT_FOUND),
        }
    }
}

/// GP: `struct rsa_public_key`
pub struct RsaPublicKey {
    pub e: BigNum, // Public exponent
    pub n: BigNum, // Modulus
}

impl Debug for RsaPublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RsaPublicKey")
            .field("e", &self.e)
            .field("n", &self.n)
            .finish()
    }
}

impl TeeCryptoOps for RsaPublicKey {
    fn new(_key_type: u32, key_size_bits: usize) -> TeeResult<Self> {
        Ok(RsaPublicKey {
            e: crypto_bignum_allocate(key_size_bits)?,
            n: crypto_bignum_allocate(key_size_bits)?,
        })
    }

    fn get_attr_by_id(&mut self, attr_id: TeeObjIdType) -> TeeResult<CryptoAttrRef<'_>> {
        match attr_id as u32 {
            TEE_ATTR_RSA_MODULUS => Ok(CryptoAttrRef::BigNum(&mut self.n)),
            TEE_ATTR_RSA_PUBLIC_EXPONENT => Ok(CryptoAttrRef::BigNum(&mut self.e)),
            _ => Err(TEE_ERROR_ITEM_NOT_FOUND),
        }
    }
}

pub fn crypto_acipher_gen_ecc_key(
    key: &mut EccKeypair,
    key_size_bits: usize,
    object_type: u32,
) -> TeeResult {
    let mut key: Box<dyn CryptoEccKeypairOpsGenerate> = match object_type {
        TEE_TYPE_ECDSA_KEYPAIR | TEE_TYPE_ECDH_KEYPAIR => {
            Box::new(EccKeypairOpsCtx::<EccComKeyPair>::new(key))
        }
        TEE_TYPE_SM2_PKE_KEYPAIR => Box::new(EccKeypairOpsCtx::<Sm2PkeKeyPair>::new(key)),
        TEE_TYPE_SM2_DSA_KEYPAIR => Box::new(EccKeypairOpsCtx::<Sm2DsaKeyPair>::new(key)),
        TEE_TYPE_SM2_KEP_KEYPAIR => Box::new(EccKeypairOpsCtx::<Sm2KepKeyPair>::new(key)),
        _ => return Err(TEE_ERROR_NOT_IMPLEMENTED),
    };
    key.generate(key_size_bits)
}

/// Returns Ok for hash algorithms exposed through the GP TEE API.
///
/// MD5 and SHA-1 are included for legacy interoperability; they are weak and
/// should not be selected for new security-sensitive workloads.
fn hash_algo_supported(algo: u32) -> TeeResult {
    match algo {
        TEE_ALG_MD5 | TEE_ALG_SHA1 | TEE_ALG_SHA224 | TEE_ALG_SHA256 | TEE_ALG_SHA384
        | TEE_ALG_SHA512 | TEE_ALG_SM3 => Ok(()),
        _ => Err(TEE_ERROR_NOT_IMPLEMENTED),
    }
}

/// GP `crypto_hash_alloc_ctx`: allocate hash context placeholder at state alloc time.
/// The actual hash context is created in `crypto_hash_init`.
pub(crate) fn crypto_hash_alloc_ctx(algo: u32) -> TeeResult<CrypCtx> {
    hash_algo_supported(algo)?;
    Ok(CrypCtx::Others)
}

pub(crate) fn crypto_hash_init(cs: Arc<Mutex<TeeCrypState>>) -> TeeResult {
    let mut cs_guard = cs.lock();
    let algo = cs_guard.algo;
    hash_algo_supported(algo)?;
    let hash_ctx = match algo {
        TEE_ALG_MD5 => HashContext::Md5(Md5::new()),
        TEE_ALG_SHA1 => HashContext::Sha1(Sha1::new()),
        TEE_ALG_SHA224 => HashContext::Sha224(Sha224::new()),
        TEE_ALG_SHA256 => HashContext::Sha256(Sha256::new()),
        TEE_ALG_SHA384 => HashContext::Sha384(Sha384::new()),
        TEE_ALG_SHA512 => HashContext::Sha512(Sha512::new()),
        TEE_ALG_SM3 => HashContext::Sm3(Sm3::new()),
        _ => return Err(TEE_ERROR_NOT_IMPLEMENTED),
    };
    cs_guard.ctx = CrypCtx::HashCtx(hash_ctx);
    cs_guard.state = CrypState::Initialized;
    Ok(())
}

pub(crate) fn crypto_hash_update(cs: Arc<Mutex<TeeCrypState>>, data: &[u8]) -> TeeResult {
    let mut cs_guard = cs.lock();

    match &mut cs_guard.ctx {
        CrypCtx::HashCtx(HashContext::Md5(h)) => {
            h.update(data);
            Ok(())
        }
        CrypCtx::HashCtx(HashContext::Sha1(h)) => {
            h.update(data);
            Ok(())
        }
        CrypCtx::HashCtx(HashContext::Sha224(h)) => {
            h.update(data);
            Ok(())
        }
        CrypCtx::HashCtx(HashContext::Sha256(h)) => {
            h.update(data);
            Ok(())
        }
        CrypCtx::HashCtx(HashContext::Sha384(h)) => {
            h.update(data);
            Ok(())
        }
        CrypCtx::HashCtx(HashContext::Sha512(h)) => {
            h.update(data);
            Ok(())
        }
        CrypCtx::HashCtx(HashContext::Sm3(h)) => {
            h.update(data);
            Ok(())
        }
        _ => Err(TEE_ERROR_BAD_PARAMETERS),
    }
}

pub(crate) fn crypto_hash_final(cs: Arc<Mutex<TeeCrypState>>, hash: &mut [u8]) -> TeeResult<usize> {
    let cs_guard = cs.lock();

    // Clone the live hash state and finalize the clone so callers that need
    // digest-extract semantics (GP `TEE_DigestExtract` → `TEE_CopyOperation`)
    // can still copy/continue the operation after `final`.
    match &cs_guard.ctx {
        CrypCtx::HashCtx(HashContext::Md5(h)) => {
            let digest = h.clone().finalize();
            let digest = digest.as_bytes();
            let len = digest.len().min(hash.len());
            hash[..len].copy_from_slice(&digest[..len]);
            Ok(len)
        }
        CrypCtx::HashCtx(HashContext::Sha1(h)) => {
            let digest = h.clone().finalize();
            let digest = digest.as_bytes();
            let len = digest.len().min(hash.len());
            hash[..len].copy_from_slice(&digest[..len]);
            Ok(len)
        }
        CrypCtx::HashCtx(HashContext::Sha224(h)) => {
            let digest = h.clone().finalize();
            let digest = digest.as_bytes();
            let len = digest.len().min(hash.len());
            hash[..len].copy_from_slice(&digest[..len]);
            Ok(len)
        }
        CrypCtx::HashCtx(HashContext::Sha256(h)) => {
            let digest = h.clone().finalize();
            let digest = digest.as_bytes();
            let len = digest.len().min(hash.len());
            hash[..len].copy_from_slice(&digest[..len]);
            Ok(len)
        }
        CrypCtx::HashCtx(HashContext::Sha384(h)) => {
            let digest = h.clone().finalize();
            let digest = digest.as_bytes();
            let len = digest.len().min(hash.len());
            hash[..len].copy_from_slice(&digest[..len]);
            Ok(len)
        }
        CrypCtx::HashCtx(HashContext::Sha512(h)) => {
            let digest = h.clone().finalize();
            let digest = digest.as_bytes();
            let len = digest.len().min(hash.len());
            hash[..len].copy_from_slice(&digest[..len]);
            Ok(len)
        }
        CrypCtx::HashCtx(HashContext::Sm3(h)) => {
            let digest = h.clone().finalize();
            let digest = digest.as_bytes();
            let len = digest.len().min(hash.len());
            hash[..len].copy_from_slice(&digest[..len]);
            Ok(len)
        }
        _ => Err(TEE_ERROR_BAD_PARAMETERS),
    }
}

// defining mac operations for cryptographic hashing
pub(crate) fn crypto_mac_alloc_ctx(algo: u32) -> TeeResult<CrypCtx> {
    match algo {
        TEE_ALG_HMAC_MD5 | TEE_ALG_HMAC_SHA1 | TEE_ALG_HMAC_SHA224 | TEE_ALG_HMAC_SHA256
        | TEE_ALG_HMAC_SHA384 | TEE_ALG_HMAC_SHA512 | TEE_ALG_HMAC_SM3 | TEE_ALG_AES_CMAC
        | TEE_ALG_DES3_CMAC | TEE_ALG_SM4_CMAC => Ok(CrypCtx::Others),
        _ => Err(TEE_ERROR_NOT_SUPPORTED),
    }
}

pub(crate) fn crypto_mac_init(cs: Arc<Mutex<TeeCrypState>>, key: &[u8]) -> TeeResult {
    let mut cs_guard = cs.lock();
    let algo = cs_guard.algo;
    match algo {
        TEE_ALG_HMAC_MD5 => {
            let hmac = HmacMd5::new(key).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
            cs_guard.ctx = CrypCtx::HmacCtx(HmacContext::HmacMd5(hmac));
            cs_guard.state = CrypState::Initialized;
            Ok(())
        }
        TEE_ALG_HMAC_SHA1 => {
            let hmac = HmacSha1::new(key).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
            cs_guard.ctx = CrypCtx::HmacCtx(HmacContext::HmacSha1(hmac));
            cs_guard.state = CrypState::Initialized;
            Ok(())
        }
        TEE_ALG_HMAC_SHA224 => {
            let hmac = HmacSha224::new(key).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
            cs_guard.ctx = CrypCtx::HmacCtx(HmacContext::HmacSha224(hmac));
            cs_guard.state = CrypState::Initialized;
            Ok(())
        }
        TEE_ALG_HMAC_SHA256 => {
            let hmac = HmacSha256::new(key).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
            cs_guard.ctx = CrypCtx::HmacCtx(HmacContext::HmacSha256(hmac));
            cs_guard.state = CrypState::Initialized;
            Ok(())
        }
        TEE_ALG_HMAC_SHA512 => {
            let hmac = HmacSha512::new(key).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
            cs_guard.ctx = CrypCtx::HmacCtx(HmacContext::HmacSha512(hmac));
            cs_guard.state = CrypState::Initialized;
            Ok(())
        }
        TEE_ALG_HMAC_SHA384 => {
            let hmac = HmacSha384::new(key).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
            cs_guard.ctx = CrypCtx::HmacCtx(HmacContext::HmacSha384(hmac));
            cs_guard.state = CrypState::Initialized;
            Ok(())
        }
        TEE_ALG_HMAC_SM3 => {
            let hmac = HmacSm3::new(key).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
            cs_guard.ctx = CrypCtx::HmacCtx(HmacContext::HmacSm3(hmac));
            cs_guard.state = CrypState::Initialized;
            Ok(())
        }
        TEE_ALG_AES_CMAC => {
            let cmac = match key.len() {
                16 => Aes128Cmac::new(key).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?,
                24 => {
                    let c = Aes192Cmac::new(key).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
                    cs_guard.ctx = CrypCtx::CmacCtx(CmacContext::Aes192(c));
                    cs_guard.state = CrypState::Initialized;
                    return Ok(());
                }
                32 => {
                    let c = Aes256Cmac::new(key).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
                    cs_guard.ctx = CrypCtx::CmacCtx(CmacContext::Aes256(c));
                    cs_guard.state = CrypState::Initialized;
                    return Ok(());
                }
                _ => return Err(TEE_ERROR_BAD_PARAMETERS),
            };
            cs_guard.ctx = CrypCtx::CmacCtx(CmacContext::Aes128(cmac));
            cs_guard.state = CrypState::Initialized;
            Ok(())
        }
        TEE_ALG_DES3_CMAC => {
            let cmac = Des3Cmac::new(key).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
            cs_guard.ctx = CrypCtx::CmacCtx(CmacContext::Des3(cmac));
            cs_guard.state = CrypState::Initialized;
            Ok(())
        }
        TEE_ALG_SM4_CMAC => {
            let cmac = Sm4Cmac::new(key).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
            cs_guard.ctx = CrypCtx::CmacCtx(CmacContext::Sm4(cmac));
            cs_guard.state = CrypState::Initialized;
            Ok(())
        }
        _ => Err(TEE_ERROR_NOT_IMPLEMENTED),
    }
}

// Crypto MAC update
pub(crate) fn crypto_mac_update(cs: Arc<Mutex<TeeCrypState>>, data: &[u8]) -> TeeResult {
    let mut guard = cs.lock();

    match &mut guard.ctx {
        CrypCtx::HmacCtx(HmacContext::HmacMd5(h)) => {
            h.update(data);
            Ok(())
        }
        CrypCtx::HmacCtx(HmacContext::HmacSha1(h)) => {
            h.update(data);
            Ok(())
        }
        CrypCtx::HmacCtx(HmacContext::HmacSha224(h)) => {
            h.update(data);
            Ok(())
        }
        CrypCtx::HmacCtx(HmacContext::HmacSha256(h)) => {
            h.update(data);
            Ok(())
        }
        CrypCtx::HmacCtx(HmacContext::HmacSha384(h)) => {
            h.update(data);
            Ok(())
        }
        CrypCtx::HmacCtx(HmacContext::HmacSha512(h)) => {
            h.update(data);
            Ok(())
        }
        CrypCtx::HmacCtx(HmacContext::HmacSm3(h)) => {
            h.update(data);
            Ok(())
        }
        CrypCtx::CmacCtx(CmacContext::Aes128(c)) => {
            c.update(data);
            Ok(())
        }
        CrypCtx::CmacCtx(CmacContext::Aes192(c)) => {
            c.update(data);
            Ok(())
        }
        CrypCtx::CmacCtx(CmacContext::Aes256(c)) => {
            c.update(data);
            Ok(())
        }
        CrypCtx::CmacCtx(CmacContext::Sm4(c)) => {
            c.update(data);
            Ok(())
        }
        CrypCtx::CmacCtx(CmacContext::Des3(c)) => {
            c.update(data);
            Ok(())
        }
        _ => Err(TEE_ERROR_BAD_PARAMETERS),
    }
}

// Crypto MAC finalization
pub(crate) fn crypto_mac_final(cs: Arc<Mutex<TeeCrypState>>, hash: &mut [u8]) -> TeeResult<usize> {
    let cs_guard = cs.lock();

    // Clone the live MAC state and finalize the clone so callers that need
    // digest-extract semantics (GP `TEE_DigestExtract` → `TEE_CopyOperation`)
    // can still copy/continue the operation after `final`.
    match &cs_guard.ctx {
        CrypCtx::HmacCtx(HmacContext::HmacMd5(h)) => {
            let tag = h.clone().finalize();
            let len = tag.len().min(hash.len());
            hash[..len].copy_from_slice(&tag[..len]);
            Ok(len)
        }
        CrypCtx::HmacCtx(HmacContext::HmacSha1(h)) => {
            let tag = h.clone().finalize();
            let len = tag.len().min(hash.len());
            hash[..len].copy_from_slice(&tag[..len]);
            Ok(len)
        }
        CrypCtx::HmacCtx(HmacContext::HmacSha224(h)) => {
            let tag = h.clone().finalize();
            let len = tag.len().min(hash.len());
            hash[..len].copy_from_slice(&tag[..len]);
            Ok(len)
        }
        CrypCtx::HmacCtx(HmacContext::HmacSha256(h)) => {
            let tag = h.clone().finalize();
            let len = tag.len().min(hash.len());
            hash[..len].copy_from_slice(&tag[..len]);
            Ok(len)
        }
        CrypCtx::HmacCtx(HmacContext::HmacSha384(h)) => {
            let tag = h.clone().finalize();
            let len = tag.len().min(hash.len());
            hash[..len].copy_from_slice(&tag[..len]);
            Ok(len)
        }
        CrypCtx::HmacCtx(HmacContext::HmacSha512(h)) => {
            let tag = h.clone().finalize();
            let len = tag.len().min(hash.len());
            hash[..len].copy_from_slice(&tag[..len]);
            Ok(len)
        }
        CrypCtx::HmacCtx(HmacContext::HmacSm3(h)) => {
            let tag = h.clone().finalize();
            let len = tag.len().min(hash.len());
            hash[..len].copy_from_slice(&tag[..len]);
            Ok(len)
        }
        CrypCtx::CmacCtx(CmacContext::Aes128(c)) => {
            let tag = c.clone().finalize();
            let len = tag.len().min(hash.len());
            hash[..len].copy_from_slice(&tag[..len]);
            Ok(len)
        }
        CrypCtx::CmacCtx(CmacContext::Aes192(c)) => {
            let tag = c.clone().finalize();
            let len = tag.len().min(hash.len());
            hash[..len].copy_from_slice(&tag[..len]);
            Ok(len)
        }
        CrypCtx::CmacCtx(CmacContext::Aes256(c)) => {
            let tag = c.clone().finalize();
            let len = tag.len().min(hash.len());
            hash[..len].copy_from_slice(&tag[..len]);
            Ok(len)
        }
        CrypCtx::CmacCtx(CmacContext::Sm4(c)) => {
            let tag = c.clone().finalize();
            let len = tag.len().min(hash.len());
            hash[..len].copy_from_slice(&tag[..len]);
            Ok(len)
        }
        CrypCtx::CmacCtx(CmacContext::Des3(c)) => {
            let tag = c.clone().finalize();
            let len = tag.len().min(hash.len());
            hash[..len].copy_from_slice(&tag[..len]);
            Ok(len)
        }
        _ => Err(TEE_ERROR_BAD_PARAMETERS),
    }
}

pub(crate) fn crypto_cipher_init(
    cs: Arc<Mutex<TeeCrypState>>,
    key: &[u8],
    iv: Option<&[u8]>,
    padding_mode: CipherPaddingMode,
) -> TeeResult {
    let mut cs_guard = cs.lock();
    let algo = cs_guard.algo;
    let mode = cs_guard.mode;

    let direction = match mode {
        TEE_OperationMode::TEE_MODE_ENCRYPT => Direction::Encrypt,
        TEE_OperationMode::TEE_MODE_DECRYPT => Direction::Decrypt,
        _ => return Err(TEE_ERROR_BAD_PARAMETERS),
    };

    let padding = match padding_mode {
        CipherPaddingMode::None => PaddingMode::None,
        CipherPaddingMode::Pkcs7 => PaddingMode::Pkcs7,
        _ => PaddingMode::None,
    };

    // 2-key DES3 (K1+K2) expands to 3-key (K1+K2+K1); the underlying cipher
    // implementation always wants 24 bytes. Reject anything that's not 16 or 24.
    let des3_key_storage;
    let key = if matches!(algo, TEE_ALG_DES3_ECB_NOPAD | TEE_ALG_DES3_CBC_NOPAD) {
        match key.len() {
            16 => {
                let mut expanded = Vec::with_capacity(24);
                expanded.extend_from_slice(&key[..8]);
                expanded.extend_from_slice(&key[8..]);
                expanded.extend_from_slice(&key[..8]);
                des3_key_storage = expanded;
                des3_key_storage.as_slice()
            }
            24 => key,
            _ => return Err(TEE_ERROR_BAD_PARAMETERS),
        }
    } else {
        key
    };

    let cipher_algo = match algo {
        TEE_ALG_AES_ECB_NOPAD => StreamingCipherAlgo::Aes128Ecb,
        TEE_ALG_AES_CBC_NOPAD => match key.len() {
            16 => StreamingCipherAlgo::Aes128Cbc,
            32 => StreamingCipherAlgo::Aes256Cbc,
            _ => return Err(TEE_ERROR_BAD_PARAMETERS),
        },
        TEE_ALG_AES_CTR => match key.len() {
            16 => StreamingCipherAlgo::Aes128Ctr,
            32 => StreamingCipherAlgo::Aes256Ctr,
            _ => return Err(TEE_ERROR_BAD_PARAMETERS),
        },
        TEE_ALG_AES_XTS => {
            let decrypt = matches!(direction, Direction::Decrypt);
            let xts_state = crate::tee::crypto::aes_xts::aes_xts_init(key, iv, decrypt)
                .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
            let xts_ctx = crate::tee::crypto::aes_xts::TeeCipherXtsCtx::new(xts_state);
            cs_guard.state = CrypState::Initialized;
            cs_guard.ctx = CrypCtx::XtsCtx(xts_ctx);
            return Ok(());
        }
        TEE_ALG_DES_ECB_NOPAD => StreamingCipherAlgo::DesEcb,
        TEE_ALG_DES3_ECB_NOPAD => StreamingCipherAlgo::Des3Ecb,
        TEE_ALG_DES_CBC_NOPAD => StreamingCipherAlgo::DesCbc,
        TEE_ALG_DES3_CBC_NOPAD => StreamingCipherAlgo::Des3Cbc,
        TEE_ALG_SM4_ECB_NOPAD => StreamingCipherAlgo::Sm4Ecb,
        TEE_ALG_SM4_CBC_NOPAD => StreamingCipherAlgo::Sm4Cbc,
        TEE_ALG_SM4_CTR => StreamingCipherAlgo::Sm4Ctr,
        TEE_ALG_SM4_XTS => {
            return Err(TEE_ERROR_NOT_IMPLEMENTED);
        }
        _ => return Err(TEE_ERROR_NOT_IMPLEMENTED),
    };

    let iv_bytes = iv.unwrap_or(&[]);
    let ctx = StreamingCipherCtx::new(cipher_algo, key, iv_bytes, direction, padding)
        .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
    cs_guard.state = CrypState::Initialized;
    cs_guard.ctx = CrypCtx::CipherCtx(ctx);
    Ok(())
}

/// Compute the maximum output length for a cipher update operation.
pub(crate) fn crypto_cipher_max_output_len(
    cs: Arc<Mutex<TeeCrypState>>,
    input_len: usize,
) -> TeeResult<usize> {
    let cs_guard = cs.lock();
    match &cs_guard.ctx {
        CrypCtx::CipherCtx(ctx) => Ok(ctx.max_update_output_len(input_len)),
        CrypCtx::XtsCtx(xts) => Ok(xts.max_update_output_len(input_len)),
        _ => Err(TEE_ERROR_BAD_PARAMETERS),
    }
}

pub(crate) fn crypto_cipher_update(
    cs: Arc<Mutex<TeeCrypState>>,
    input: &[u8],
    output: &mut [u8],
) -> TeeResult<usize> {
    let mut cs_guard = cs.lock();
    match &mut cs_guard.ctx {
        CrypCtx::CipherCtx(ctx) => {
            let result = ctx.update(input).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
            let len = result.len().min(output.len());
            output[..len].copy_from_slice(&result[..len]);
            Ok(len)
        }
        CrypCtx::XtsCtx(xts) => {
            let written = xts.cipher_update(input, output)?;
            Ok(written)
        }
        _ => Err(TEE_ERROR_BAD_PARAMETERS),
    }
}

pub(crate) fn crypto_cipher_final(
    cs: Arc<Mutex<TeeCrypState>>,
    output: &mut [u8],
) -> TeeResult<usize> {
    let mut cs_guard = cs.lock();
    match &mut cs_guard.ctx {
        CrypCtx::CipherCtx(ctx) => {
            let result = ctx.r#final().map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
            let len = result.len().min(output.len());
            output[..len].copy_from_slice(&result[..len]);
            Ok(len)
        }
        CrypCtx::XtsCtx(xts) => {
            let written = xts.cipher_final(&[], output)?;
            Ok(written)
        }
        _ => Err(TEE_ERROR_BAD_PARAMETERS),
    }
}

/// XTS-specific final entry: consumes any trailing `input` plus flushes pending bytes
/// within a single `in_final_syscall=true` scope so ciphertext-stealing tweak handling
/// matches the GP single-shot reference.
pub(crate) fn crypto_xts_cipher_final(
    cs: Arc<Mutex<TeeCrypState>>,
    input: &[u8],
    output: &mut [u8],
) -> TeeResult<usize> {
    let mut cs_guard = cs.lock();
    if let CrypCtx::XtsCtx(xts) = &mut cs_guard.ctx {
        xts.cipher_final(input, output)
    } else {
        Err(TEE_ERROR_BAD_PARAMETERS)
    }
}

/// mbedtls不支持CCM模式的流式加密
/// 暂不支持CCM模式
pub(crate) fn crypto_authenc_init(
    cs: Arc<Mutex<TeeCrypState>>,
    key: &[u8],
    nonce: &[u8],
    _aad_len: Option<usize>,
    tag_len: Option<usize>,
    _payload_len: Option<usize>,
) -> TeeResult {
    let mut cs_guard = cs.lock();
    let algo = cs_guard.algo;
    let mode = cs_guard.mode;

    let direction = match mode {
        TEE_OperationMode::TEE_MODE_ENCRYPT => Direction::Encrypt,
        TEE_OperationMode::TEE_MODE_DECRYPT => Direction::Decrypt,
        _ => return Err(TEE_ERROR_BAD_PARAMETERS),
    };

    let aead_algo = match algo {
        TEE_ALG_AES_GCM => match key.len() {
            16 => StreamingCipherAlgo::Aes128Gcm,
            24 => StreamingCipherAlgo::Aes192Gcm,
            32 => StreamingCipherAlgo::Aes256Gcm,
            _ => return Err(TEE_ERROR_BAD_PARAMETERS),
        },
        TEE_ALG_SM4_GCM => StreamingCipherAlgo::Sm4Gcm,
        TEE_ALG_AES_CCM => match key.len() {
            16 => StreamingCipherAlgo::Aes128Ccm,
            32 => StreamingCipherAlgo::Aes256Ccm,
            _ => return Err(TEE_ERROR_BAD_PARAMETERS),
        },
        TEE_ALG_SM4_CCM => {
            return Err(TEE_ERROR_NOT_IMPLEMENTED);
        }
        _ => return Err(TEE_ERROR_NOT_IMPLEMENTED),
    };

    let tl = tag_len.unwrap_or(16);
    let ctx = StreamingCipherCtx::new_aead(aead_algo, key, nonce, direction, tl)
        .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
    cs_guard.state = CrypState::Initialized;
    cs_guard.ctx = CrypCtx::CipherCtx(ctx);
    Ok(())
}

pub(crate) fn crypto_authenc_update_aad(cs: Arc<Mutex<TeeCrypState>>, aad: &[u8]) -> TeeResult {
    let mut cs_guard = cs.lock();
    if let CrypCtx::CipherCtx(ctx) = &mut cs_guard.ctx {
        if ctx.payload_started() {
            return Err(TEE_ERROR_BAD_PARAMETERS);
        }
        ctx.update_aad(aad);
        Ok(())
    } else {
        Err(TEE_ERROR_BAD_PARAMETERS)
    }
}

pub(crate) fn crypto_authenc_update_payload(
    cs: Arc<Mutex<TeeCrypState>>,
    input: &[u8],
    output: &mut [u8],
) -> TeeResult<usize> {
    crypto_cipher_update(cs, input, output)
}

pub(crate) fn crypto_authenc_enc_final(
    cs: Arc<Mutex<TeeCrypState>>,
    input: Option<&[u8]>,
    output: &mut [u8],
    tag: &mut [u8],
) -> TeeResult<usize> {
    let mut cs_guard = cs.lock();
    let ctx = core::mem::replace(&mut cs_guard.ctx, CrypCtx::Finalized);
    if let CrypCtx::CipherCtx(ctx) = ctx {
        let (ct, tag_val) = ctx
            .encrypt_final_with_input(input)
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        let ct_len = ct.len().min(output.len());
        output[..ct_len].copy_from_slice(&ct[..ct_len]);
        let tag_len = tag_val.len().min(tag.len());
        tag[..tag_len].copy_from_slice(&tag_val[..tag_len]);
        Ok(ct_len)
    } else {
        cs_guard.ctx = ctx; // put back
        Err(TEE_ERROR_BAD_PARAMETERS)
    }
}

pub(crate) fn crypto_authenc_dec_final(
    cs: Arc<Mutex<TeeCrypState>>,
    input: Option<&[u8]>,
    output: &mut [u8],
    tag: &[u8],
) -> TeeResult<usize> {
    let mut cs_guard = cs.lock();
    let ctx = core::mem::replace(&mut cs_guard.ctx, CrypCtx::Finalized);
    if let CrypCtx::CipherCtx(ctx) = ctx {
        let pt = ctx
            .decrypt_final_with_input(input, tag)
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        let pt_len = pt.len().min(output.len());
        output[..pt_len].copy_from_slice(&pt[..pt_len]);
        Ok(pt_len)
    } else {
        cs_guard.ctx = ctx; // put back
        Err(TEE_ERROR_BAD_PARAMETERS)
    }
}

pub(crate) fn crypto_rsa_init(cs: Arc<Mutex<TeeCrypState>>, mode: TEE_OperationMode) -> TeeResult {
    let mut cs_guard = cs.lock();
    let key1 = cs_guard.key1;

    if let Some(k) = key1 {
        let obj_key1 = tee_obj_get(k as _)?;
        let obj_key1_guard = obj_key1.lock();

        if obj_key1_guard.attr.is_empty() {
            return Err(TEE_ERROR_BAD_STATE);
        }

        match mode {
            TEE_OperationMode::TEE_MODE_ENCRYPT | TEE_OperationMode::TEE_MODE_VERIFY => {
                if let TeeCryptObj::RsaPublicKey(rsa_key) = &obj_key1_guard.attr[0] {
                    cs_guard.ctx = CrypCtx::AsyCtx(AsymmetricCtx::RsaPublic {
                        n: bn_to_bytes(&rsa_key.n),
                        e: bn_to_bytes(&rsa_key.e),
                    });
                } else {
                    return Err(TEE_ERROR_BAD_STATE);
                }
            }
            TEE_OperationMode::TEE_MODE_DECRYPT | TEE_OperationMode::TEE_MODE_SIGN => {
                if let TeeCryptObj::RsaKeypair(rsa_key) = &obj_key1_guard.attr[0] {
                    cs_guard.ctx = CrypCtx::AsyCtx(AsymmetricCtx::RsaPrivate {
                        n: bn_to_bytes(&rsa_key.n),
                        e: bn_to_bytes(&rsa_key.e),
                        d: rsa_bn_to_secret_bytes(&rsa_key.d),
                        p: rsa_bn_to_secret_bytes(&rsa_key.p),
                        q: rsa_bn_to_secret_bytes(&rsa_key.q),
                    });
                } else {
                    return Err(TEE_ERROR_BAD_STATE);
                }
            }
            _ => return Err(TEE_ERROR_BAD_PARAMETERS),
        }
    } else {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    };
    Ok(())
}

pub(crate) fn crypto_acipher_rsanopad_encrypt(
    cs: Arc<Mutex<TeeCrypState>>,
    input: &[u8],
    output: &mut [u8],
    required: &mut usize,
) -> TeeResult<usize> {
    let cs_guard = cs.lock();
    if let CrypCtx::AsyCtx(AsymmetricCtx::RsaPublic { n, e }) = &cs_guard.ctx {
        let mod_size = n.len();
        if mod_size == 0 || input.len() > mod_size {
            return Err(TEE_ERROR_BAD_PARAMETERS);
        }
        let pubkey = rsa_ops::rsa_public_from_components(vec_as_bytes(n), vec_as_bytes(e))
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        let mut scratch = vec![0u8; mod_size];
        let out_len = rsa_ops::rsa_nopad_encrypt(&pubkey, input, &mut scratch)
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        *required = out_len;
        if output.len() < out_len {
            return Err(TEE_ERROR_SHORT_BUFFER);
        }
        output[..out_len].copy_from_slice(&scratch[..out_len]);
        Ok(out_len)
    } else {
        Err(TEE_ERROR_BAD_PARAMETERS)
    }
}

pub(crate) fn crypto_acipher_rsanopad_decrypt(
    cs: Arc<Mutex<TeeCrypState>>,
    input: &[u8],
    output: &mut [u8],
    required: &mut usize,
) -> TeeResult<usize> {
    let cs_guard = cs.lock();
    if let CrypCtx::AsyCtx(AsymmetricCtx::RsaPrivate { n, e, d, p, q }) = &cs_guard.ctx {
        let mod_size = n.len();
        if mod_size == 0 || input.len() > mod_size {
            return Err(TEE_ERROR_BAD_PARAMETERS);
        }
        let keypair = rsa_ops::rsa_key_from_components(
            vec_as_bytes(n),
            vec_as_bytes(e),
            vec_as_bytes(d),
            vec_as_bytes(p),
            vec_as_bytes(q),
        )
        .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        let mut scratch = vec![0u8; mod_size];
        let out_len = rsa_ops::rsa_nopad_decrypt(&keypair, input, &mut scratch)
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        *required = out_len;
        if output.len() < out_len {
            return Err(TEE_ERROR_SHORT_BUFFER);
        }
        output[..out_len].copy_from_slice(&scratch[..out_len]);
        Ok(out_len)
    } else {
        Err(TEE_ERROR_BAD_PARAMETERS)
    }
}

pub(crate) fn crypto_ecc_init(cs: Arc<Mutex<TeeCrypState>>, is_sm2: bool) -> TeeResult {
    let mut cs_guard = cs.lock();
    let key1 = cs_guard.key1;
    let mode = cs_guard.mode;

    if let Some(k) = key1 {
        let obj_key1 = tee_obj_get(k as _)?;
        let obj_key1_guard = obj_key1.lock();

        if obj_key1_guard.attr.is_empty() {
            return Err(TEE_ERROR_BAD_STATE);
        }

        match mode {
            TEE_OperationMode::TEE_MODE_ENCRYPT | TEE_OperationMode::TEE_MODE_VERIFY => {
                if let TeeCryptObj::EccPublicKey(ecc_key) = &obj_key1_guard.attr[0] {
                    let curve = if is_sm2 {
                        EccCurve::Sm2
                    } else {
                        tee_curve_to_ecc_curve(ecc_key.curve)?
                    };
                    let field_len = ecc_curve_field_byte_len(curve);
                    cs_guard.ctx = CrypCtx::AsyCtx(AsymmetricCtx::EccPublic {
                        curve,
                        x: bn_to_ecc_field_bytes(&ecc_key.x, field_len),
                        y: bn_to_ecc_field_bytes(&ecc_key.y, field_len),
                    });
                } else {
                    return Err(TEE_ERROR_BAD_STATE);
                }
            }
            TEE_OperationMode::TEE_MODE_DECRYPT | TEE_OperationMode::TEE_MODE_SIGN => {
                if let TeeCryptObj::EccKeypair(ecc_key) = &obj_key1_guard.attr[0] {
                    let curve = if is_sm2 {
                        EccCurve::Sm2
                    } else {
                        tee_curve_to_ecc_curve(ecc_key.curve)?
                    };
                    let field_len = ecc_curve_field_byte_len(curve);
                    cs_guard.ctx = CrypCtx::AsyCtx(AsymmetricCtx::EccPrivate {
                        curve,
                        secret: bn_to_ecc_field_bytes(&ecc_key.d, field_len),
                    });
                } else {
                    return Err(TEE_ERROR_BAD_STATE);
                }
            }
            _ => return Err(TEE_ERROR_BAD_PARAMETERS),
        }
    }
    Ok(())
}

pub(crate) fn crypto_acipher_sm2_pke_encrypt(
    cs: Arc<Mutex<TeeCrypState>>,
    input: &[u8],
    output: &mut [u8],
) -> TeeResult<usize> {
    let cs_guard = cs.lock();
    if let CrypCtx::AsyCtx(AsymmetricCtx::EccPublic { x, y, .. }) = &cs_guard.ctx {
        let mut rng = TeeSoftwareRng::new();
        let ct =
            tee_crypto::sm2::sm2_pke_encrypt(vec_as_bytes(x), vec_as_bytes(y), input, &mut rng)
                .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        let ct = ct.as_bytes();
        let len = ct.len().min(output.len());
        output[..len].copy_from_slice(&ct[..len]);
        Ok(len)
    } else {
        Err(TEE_ERROR_BAD_PARAMETERS)
    }
}

pub(crate) fn crypto_acipher_sm2_pke_decrypt(
    cs: Arc<Mutex<TeeCrypState>>,
    input: &[u8],
    output: &mut [u8],
) -> TeeResult<usize> {
    let cs_guard = cs.lock();
    if let CrypCtx::AsyCtx(AsymmetricCtx::EccPrivate { secret, .. }) = &cs_guard.ctx {
        let ciphertext = ciphertext_from_tee(input, CiphertextAlgorithm::Sm2Pke);
        let pt = tee_crypto::sm2::sm2_pke_decrypt(vec_as_bytes(secret), &ciphertext)
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        let pt = pt.expose_secret();
        let len = pt.len().min(output.len());
        output[..len].copy_from_slice(&pt[..len]);
        Ok(len)
    } else {
        Err(TEE_ERROR_BAD_PARAMETERS)
    }
}

pub(crate) fn crypto_acipher_rsaes_encrypt(
    cs: Arc<Mutex<TeeCrypState>>,
    input: &[u8],
    output: &mut [u8],
    _label: &[u8],
    required: &mut usize,
) -> TeeResult<usize> {
    let cs_guard = cs.lock();
    let algo = cs_guard.algo;

    if let CrypCtx::AsyCtx(AsymmetricCtx::RsaPublic { n, e }) = &cs_guard.ctx {
        check_rsa_modulus_output(n.len(), output.len(), required)?;
        let mut rng = TeeSoftwareRng::new();
        let pubkey = rsa_ops::rsa_public_from_components(vec_as_bytes(n), vec_as_bytes(e))
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        let hash_algo = algo_to_rsa_hash(algo);
        let ct = match algo_to_enc_padding(algo) {
            RsaEncPadding::Pkcs1v15 => rsa_ops::rsa_encrypt_pkcs1v15(&pubkey, input, &mut rng)
                .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?,
            RsaEncPadding::Oaep => {
                rsa_ops::rsa_encrypt_oaep(&pubkey, hash_algo, _label, input, &mut rng)
                    .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?
            }
        };
        let ct = ct.as_bytes();
        output[..ct.len()].copy_from_slice(ct);
        Ok(ct.len())
    } else {
        Err(TEE_ERROR_BAD_PARAMETERS)
    }
}

pub(crate) fn crypto_acipher_rsaes_decrypt(
    cs: Arc<Mutex<TeeCrypState>>,
    input: &[u8],
    output: &mut [u8],
    _label: &[u8],
    required: &mut usize,
) -> TeeResult<usize> {
    let cs_guard = cs.lock();
    let algo = cs_guard.algo;

    if let CrypCtx::AsyCtx(AsymmetricCtx::RsaPrivate { n, e, d, p, q }) = &cs_guard.ctx {
        let keypair = rsa_ops::rsa_key_from_components(
            vec_as_bytes(n),
            vec_as_bytes(e),
            vec_as_bytes(d),
            vec_as_bytes(p),
            vec_as_bytes(q),
        )
        .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        let hash_algo = algo_to_rsa_hash(algo);
        let pt = match algo_to_enc_padding(algo) {
            RsaEncPadding::Pkcs1v15 => {
                let ciphertext = ciphertext_from_tee(input, CiphertextAlgorithm::RsaPkcs1v15);
                rsa_ops::rsa_decrypt_pkcs1v15(&keypair, &ciphertext)
                    .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?
            }
            RsaEncPadding::Oaep => {
                let ciphertext = ciphertext_from_tee(input, CiphertextAlgorithm::RsaOaep);
                rsa_ops::rsa_decrypt_oaep(&keypair, hash_algo, _label, &ciphertext)
                    .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?
            }
        };
        let pt = pt.expose_secret();
        *required = pt.len();
        if output.len() < pt.len() {
            return Err(TEE_ERROR_SHORT_BUFFER);
        }
        output[..pt.len()].copy_from_slice(pt);
        Ok(pt.len())
    } else {
        Err(TEE_ERROR_BAD_PARAMETERS)
    }
}

pub(crate) fn crypto_acipher_rsassa_sign(
    cs: Arc<Mutex<TeeCrypState>>,
    input: &[u8],
    output: &mut [u8],
    required: &mut usize,
    pss_salt_len: Option<usize>,
) -> TeeResult<usize> {
    let cs_guard = cs.lock();
    let algo = cs_guard.algo;

    if let CrypCtx::AsyCtx(AsymmetricCtx::RsaPrivate { n, e, d, p, q }) = &cs_guard.ctx {
        check_rsa_modulus_output(n.len(), output.len(), required)?;
        let mut rng = TeeSoftwareRng::new();
        let keypair = rsa_ops::rsa_key_from_components(
            vec_as_bytes(n),
            vec_as_bytes(e),
            vec_as_bytes(d),
            vec_as_bytes(p),
            vec_as_bytes(q),
        )
        .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        let hash_algo = algo_to_rsa_hash(algo);
        let digest = rsa_digest_from_tee(hash_algo, input);
        let sig = match algo_to_sign_padding(algo) {
            RsaSignPadding::Pkcs1v15 => rsa_ops::rsa_sign(
                &keypair,
                hash_algo,
                RsaSignPadding::Pkcs1v15,
                &digest,
                &mut rng,
                None,
            )
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?,
            RsaSignPadding::Pss => rsa_ops::rsa_sign(
                &keypair,
                hash_algo,
                RsaSignPadding::Pss,
                &digest,
                &mut rng,
                pss_salt_len,
            )
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?,
        };
        let sig = sig.as_bytes();
        output[..sig.len()].copy_from_slice(sig);
        Ok(sig.len())
    } else {
        Err(TEE_ERROR_BAD_PARAMETERS)
    }
}

pub(crate) fn crypto_acipher_rsassa_verify(
    cs: Arc<Mutex<TeeCrypState>>,
    hash: &[u8],
    signature: &[u8],
) -> TeeResult {
    let cs_guard = cs.lock();
    let algo = cs_guard.algo;

    if let CrypCtx::AsyCtx(AsymmetricCtx::RsaPublic { n, e }) = &cs_guard.ctx {
        let pubkey = rsa_ops::rsa_public_from_components(vec_as_bytes(n), vec_as_bytes(e))
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        let hash_algo = algo_to_rsa_hash(algo);
        let digest = rsa_digest_from_tee(hash_algo, hash);
        match algo_to_sign_padding(algo) {
            RsaSignPadding::Pkcs1v15 => {
                let signature = signature_from_tee(
                    signature,
                    SignatureAlgorithm::RsaPkcs1v15,
                    SignatureEncoding::Raw,
                );
                rsa_ops::rsa_verify(
                    &pubkey,
                    hash_algo,
                    RsaSignPadding::Pkcs1v15,
                    &digest,
                    &signature,
                )
                .map_err(map_rsa_verify_err)
            }
            RsaSignPadding::Pss => {
                let signature = signature_from_tee(
                    signature,
                    SignatureAlgorithm::RsaPss,
                    SignatureEncoding::Raw,
                );
                rsa_ops::rsa_verify(&pubkey, hash_algo, RsaSignPadding::Pss, &digest, &signature)
                    .map_err(map_rsa_verify_err)
            }
        }
    } else {
        Err(TEE_ERROR_BAD_PARAMETERS)
    }
}

fn map_rsa_verify_err(err: tee_crypto::CryptoError) -> u32 {
    match err {
        tee_crypto::CryptoError::VerificationFailed | tee_crypto::CryptoError::InvalidInput => {
            TEE_ERROR_SIGNATURE_INVALID
        }
        _ => TEE_ERROR_BAD_PARAMETERS,
    }
}

pub(crate) fn crypto_acipher_ecc_sign(
    cs: Arc<Mutex<TeeCrypState>>,
    input: &[u8],
    output: &mut [u8],
    required: &mut usize,
) -> TeeResult<usize> {
    let cs_guard = cs.lock();
    let algo = cs_guard.algo;

    if let CrypCtx::AsyCtx(AsymmetricCtx::EccPrivate { curve, secret }) = &cs_guard.ctx {
        let max_len = ecc_max_signature_len(*curve, algo);
        *required = max_len;
        if output.len() < max_len {
            return Err(TEE_ERROR_SHORT_BUFFER);
        }
        let mut rng = TeeSoftwareRng::new();
        match algo {
            TEE_ALG_SM2_DSA_SM3 => {
                let sig = tee_crypto::sm2::sm2_dsa_sign(vec_as_bytes(secret), input, &mut rng)
                    .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
                let sig = sig.as_bytes();
                *required = sig.len();
                if output.len() < sig.len() {
                    return Err(TEE_ERROR_SHORT_BUFFER);
                }
                output[..sig.len()].copy_from_slice(sig);
                Ok(sig.len())
            }
            _ => {
                let hash_algo = match algo {
                    TEE_ALG_ECDSA_SHA1 => EccHashAlgo::Sha1,
                    TEE_ALG_ECDSA_SHA224 => EccHashAlgo::Sha224,
                    TEE_ALG_ECDSA_SHA256 => EccHashAlgo::Sha256,
                    TEE_ALG_ECDSA_SHA384 => EccHashAlgo::Sha384,
                    TEE_ALG_ECDSA_SHA512 => EccHashAlgo::Sha512,
                    _ => return Err(TEE_ERROR_BAD_PARAMETERS),
                };
                let digest = ecc_digest_from_tee(hash_algo, input);
                let sig =
                    ecc_ops::ecc_sign(*curve, hash_algo, vec_as_bytes(secret), &digest, &mut rng)
                        .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
                let sig = sig.as_bytes();
                *required = sig.len();
                if output.len() < sig.len() {
                    return Err(TEE_ERROR_SHORT_BUFFER);
                }
                output[..sig.len()].copy_from_slice(sig);
                Ok(sig.len())
            }
        }
    } else {
        Err(TEE_ERROR_BAD_PARAMETERS)
    }
}

pub(crate) fn crypto_acipher_ecc_verify(
    cs: Arc<Mutex<TeeCrypState>>,
    hash: &[u8],
    signature: &[u8],
) -> TeeResult {
    let cs_guard = cs.lock();
    let algo = cs_guard.algo;

    if let CrypCtx::AsyCtx(AsymmetricCtx::EccPublic { curve, x, y }) = &cs_guard.ctx {
        match algo {
            TEE_ALG_SM2_DSA_SM3 => {
                // SM2 signatures may arrive either as raw `r||s` (64B) or DER
                // (`0x30 len ...`). GP-generated sigs are raw, but vectors
                // copied from specs are usually DER — accept both.
                let encoding = if signature.len() == 64 {
                    SignatureEncoding::Raw
                } else {
                    SignatureEncoding::Der
                };
                let signature = signature_from_tee(signature, SignatureAlgorithm::Sm2Dsa, encoding);
                tee_crypto::sm2::sm2_dsa_verify(vec_as_bytes(x), vec_as_bytes(y), hash, &signature)
                    .map_err(map_rsa_verify_err)
            }
            _ => {
                let hash_algo = match algo {
                    TEE_ALG_ECDSA_SHA1 => EccHashAlgo::Sha1,
                    TEE_ALG_ECDSA_SHA224 => EccHashAlgo::Sha224,
                    TEE_ALG_ECDSA_SHA256 => EccHashAlgo::Sha256,
                    TEE_ALG_ECDSA_SHA384 => EccHashAlgo::Sha384,
                    TEE_ALG_ECDSA_SHA512 => EccHashAlgo::Sha512,
                    _ => return Err(TEE_ERROR_BAD_PARAMETERS),
                };
                let digest = ecc_digest_from_tee(hash_algo, hash);
                let raw_len = ecc_raw_signature_len(*curve);
                let encoding = if signature.len() == raw_len {
                    SignatureEncoding::Raw
                } else {
                    SignatureEncoding::Der
                };
                let signature =
                    signature_from_tee(signature, SignatureAlgorithm::Ecdsa(*curve), encoding);
                ecc_ops::ecc_verify(
                    *curve,
                    hash_algo,
                    vec_as_bytes(x),
                    vec_as_bytes(y),
                    &digest,
                    &signature,
                )
                .map_err(map_rsa_verify_err)
            }
        }
    } else {
        Err(TEE_ERROR_BAD_PARAMETERS)
    }
}

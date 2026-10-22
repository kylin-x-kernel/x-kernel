// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, format, sync::Arc, vec, vec::Vec};
use core::{default::Default, fmt, fmt::Debug};

use ksync::Mutex;
use tee_crypto::{
    asymmetric::EccCurve,
    hash::{Digest, DigestBytes, HashAlgorithm, Sha1, Sha224, Sha256, Sha384, Sha512, Sm3},
    hkdf::hkdf,
    kdf::{concat_kdf, pbkdf2_hmac_sha1},
    mac::{
        Aes128Cmac, Aes192Cmac, Aes256Cmac, Des3Cmac, HmacMd5, HmacSha1, HmacSha224, HmacSha256,
        HmacSha384, HmacSha512, HmacSm3, Mac, Sm4Cmac,
    },
    material::{
        CiphertextAlgorithm, CiphertextBytes, SignatureAlgorithm, SignatureBytes, SignatureEncoding,
    },
    md5::Md5,
    sm2::{ConfirmationMode, Sm2KepDeriveParams, Sm2KepKeypair, Sm2KepPeerKeys, sm2_kep_derive},
    streaming_cipher::{Direction, PaddingMode, StreamingCipherAlgo, StreamingCipherCtx},
    tee_ops::{
        ecc::{self as ecc_ops, EccHashAlgo},
        ed25519::{
            self as ed25519_ops, ED25519_CTX_MAX_LENGTH, ED25519_KEY_SIZE_BYTES,
            ED25519_SIGNATURE_SIZE_BYTES, Ed25519Variant,
        },
        rsa::{self as rsa_ops, RsaEncPadding, RsaHashAlgo, RsaSignPadding},
        x448::{self as x448_ops, X448_KEY_SIZE_BYTES},
        x25519::{self as x25519_ops, X25519_KEY_SIZE_BYTES},
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
    libutee::utee_defines::{tee_alg_get_class, tee_alg_get_main_alg},
    rng_software::TeeSoftwareRng,
    tee_api_defines_extensions::{
        __OPTEE_TEE_ATTR_HKDF_INFO, __OPTEE_TEE_ATTR_HKDF_SALT, TEE_ALG_CONCAT_KDF_SHA1_DERIVE_KEY,
        TEE_ALG_CONCAT_KDF_SHA224_DERIVE_KEY, TEE_ALG_CONCAT_KDF_SHA256_DERIVE_KEY,
        TEE_ALG_CONCAT_KDF_SHA384_DERIVE_KEY, TEE_ALG_CONCAT_KDF_SHA512_DERIVE_KEY,
        TEE_ALG_HKDF_MD5_DERIVE_KEY, TEE_ALG_HKDF_SHA1_DERIVE_KEY, TEE_ALG_HKDF_SHA224_DERIVE_KEY,
        TEE_ALG_HKDF_SHA256_DERIVE_KEY, TEE_ALG_HKDF_SHA384_DERIVE_KEY,
        TEE_ALG_HKDF_SHA512_DERIVE_KEY, TEE_ALG_PBKDF2_HMAC_SHA1_DERIVE_KEY, TEE_ALG_SM4_XTS,
        TEE_ALG_X448, TEE_ATTR_CONCAT_KDF_DKM_LENGTH, TEE_ATTR_CONCAT_KDF_OTHER_INFO,
        TEE_ATTR_HKDF_OKM_LENGTH, TEE_ATTR_PBKDF2_DKM_LENGTH, TEE_ATTR_PBKDF2_ITERATION_COUNT,
        TEE_ATTR_PBKDF2_SALT,
    },
    tee_obj::{TeeObj, TeeObjIdType, tee_obj_get},
    tee_svc_cryp::{
        CryptoAttrRef, KernelAttribute, TEE_CRYP_OBJ_SECRET_VALUE_ATTRS, TeeCrypObjSecretWrapper,
        TeeCrypObjTypeProps, TeeCryptObj, TeeCryptoOps, set_attribute, tee_svc_find_type_props,
        write_user_memref_out,
    },
    tee_svc_cryp2::{
        CipherPaddingMode, CmacContext, CrypCtx, CrypState, HashContext, HmacContext, TeeCrypState,
        tee_cryp_state_get, translate_compat_algo,
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

pub(crate) fn ecc_curve_field_byte_len(curve: EccCurve) -> usize {
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
pub(crate) fn bn_to_ecc_field_bytes(bn: &BigNum, field_len: usize) -> Vec<u8> {
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
pub(crate) fn tee_curve_to_ecc_curve(curve: u32) -> TeeResult<EccCurve> {
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
pub(crate) struct EccPublicKey {
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
pub(crate) struct EccKeypair {
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
pub(crate) struct RsaKeypair {
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
pub(crate) struct RsaPublicKey {
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

pub(crate) fn crypto_acipher_gen_ecc_key(
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
    let mut cs_guard = cs.lock();

    // Clone the live hash state and finalize the clone so callers that need
    // digest-extract semantics (GP `TEE_DigestExtract` → `TEE_CopyOperation`)
    // can still copy the operation after `final`. Mark Finalized so a later
    // `update` returns BAD_STATE (GP DigestDoFinal).
    let len = match &cs_guard.ctx {
        CrypCtx::HashCtx(HashContext::Md5(h)) => {
            let digest = h.clone().finalize();
            let digest = digest.as_bytes();
            let len = digest.len().min(hash.len());
            hash[..len].copy_from_slice(&digest[..len]);
            len
        }
        CrypCtx::HashCtx(HashContext::Sha1(h)) => {
            let digest = h.clone().finalize();
            let digest = digest.as_bytes();
            let len = digest.len().min(hash.len());
            hash[..len].copy_from_slice(&digest[..len]);
            len
        }
        CrypCtx::HashCtx(HashContext::Sha224(h)) => {
            let digest = h.clone().finalize();
            let digest = digest.as_bytes();
            let len = digest.len().min(hash.len());
            hash[..len].copy_from_slice(&digest[..len]);
            len
        }
        CrypCtx::HashCtx(HashContext::Sha256(h)) => {
            let digest = h.clone().finalize();
            let digest = digest.as_bytes();
            let len = digest.len().min(hash.len());
            hash[..len].copy_from_slice(&digest[..len]);
            len
        }
        CrypCtx::HashCtx(HashContext::Sha384(h)) => {
            let digest = h.clone().finalize();
            let digest = digest.as_bytes();
            let len = digest.len().min(hash.len());
            hash[..len].copy_from_slice(&digest[..len]);
            len
        }
        CrypCtx::HashCtx(HashContext::Sha512(h)) => {
            let digest = h.clone().finalize();
            let digest = digest.as_bytes();
            let len = digest.len().min(hash.len());
            hash[..len].copy_from_slice(&digest[..len]);
            len
        }
        CrypCtx::HashCtx(HashContext::Sm3(h)) => {
            let digest = h.clone().finalize();
            let digest = digest.as_bytes();
            let len = digest.len().min(hash.len());
            hash[..len].copy_from_slice(&digest[..len]);
            len
        }
        _ => return Err(TEE_ERROR_BAD_PARAMETERS),
    };
    cs_guard.state = CrypState::Finalized;
    Ok(len)
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
    let mut cs_guard = cs.lock();

    // Clone the live MAC state and finalize the clone so callers that need
    // digest-extract semantics (GP `TEE_DigestExtract` → `TEE_CopyOperation`)
    // can still copy the operation after `final`. Mark Finalized so a later
    // `update` returns BAD_STATE (GP MACCompareFinal / MACComputeFinal).
    let len = match &cs_guard.ctx {
        CrypCtx::HmacCtx(HmacContext::HmacMd5(h)) => {
            let tag = h.clone().finalize();
            let len = tag.len().min(hash.len());
            hash[..len].copy_from_slice(&tag[..len]);
            len
        }
        CrypCtx::HmacCtx(HmacContext::HmacSha1(h)) => {
            let tag = h.clone().finalize();
            let len = tag.len().min(hash.len());
            hash[..len].copy_from_slice(&tag[..len]);
            len
        }
        CrypCtx::HmacCtx(HmacContext::HmacSha224(h)) => {
            let tag = h.clone().finalize();
            let len = tag.len().min(hash.len());
            hash[..len].copy_from_slice(&tag[..len]);
            len
        }
        CrypCtx::HmacCtx(HmacContext::HmacSha256(h)) => {
            let tag = h.clone().finalize();
            let len = tag.len().min(hash.len());
            hash[..len].copy_from_slice(&tag[..len]);
            len
        }
        CrypCtx::HmacCtx(HmacContext::HmacSha384(h)) => {
            let tag = h.clone().finalize();
            let len = tag.len().min(hash.len());
            hash[..len].copy_from_slice(&tag[..len]);
            len
        }
        CrypCtx::HmacCtx(HmacContext::HmacSha512(h)) => {
            let tag = h.clone().finalize();
            let len = tag.len().min(hash.len());
            hash[..len].copy_from_slice(&tag[..len]);
            len
        }
        CrypCtx::HmacCtx(HmacContext::HmacSm3(h)) => {
            let tag = h.clone().finalize();
            let len = tag.len().min(hash.len());
            hash[..len].copy_from_slice(&tag[..len]);
            len
        }
        CrypCtx::CmacCtx(CmacContext::Aes128(c)) => {
            let tag = c.clone().finalize();
            let len = tag.len().min(hash.len());
            hash[..len].copy_from_slice(&tag[..len]);
            len
        }
        CrypCtx::CmacCtx(CmacContext::Aes192(c)) => {
            let tag = c.clone().finalize();
            let len = tag.len().min(hash.len());
            hash[..len].copy_from_slice(&tag[..len]);
            len
        }
        CrypCtx::CmacCtx(CmacContext::Aes256(c)) => {
            let tag = c.clone().finalize();
            let len = tag.len().min(hash.len());
            hash[..len].copy_from_slice(&tag[..len]);
            len
        }
        CrypCtx::CmacCtx(CmacContext::Sm4(c)) => {
            let tag = c.clone().finalize();
            let len = tag.len().min(hash.len());
            hash[..len].copy_from_slice(&tag[..len]);
            len
        }
        CrypCtx::CmacCtx(CmacContext::Des3(c)) => {
            let tag = c.clone().finalize();
            let len = tag.len().min(hash.len());
            hash[..len].copy_from_slice(&tag[..len]);
            len
        }
        _ => return Err(TEE_ERROR_BAD_PARAMETERS),
    };
    cs_guard.state = CrypState::Finalized;
    Ok(len)
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

/// AEAD encrypt-final. On `SHORT_BUFFER`, writes required sizes to
/// `required_dst` / `required_tag` and leaves the operation intact.
pub(crate) fn crypto_authenc_enc_final(
    cs: Arc<Mutex<TeeCrypState>>,
    input: Option<&[u8]>,
    output: &mut [u8],
    tag: &mut [u8],
    required_dst: &mut usize,
    required_tag: &mut usize,
) -> TeeResult<usize> {
    let mut cs_guard = cs.lock();
    let CrypCtx::CipherCtx(ctx) = &cs_guard.ctx else {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    };

    // Probe on a clone so SHORT_BUFFER does not consume the live operation.
    let (ct, tag_val) = ctx
        .clone()
        .encrypt_final_with_input(input)
        .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
    *required_dst = ct.len();
    *required_tag = tag_val.len();
    if output.len() < ct.len() || tag.len() < tag_val.len() {
        return Err(TEE_ERROR_SHORT_BUFFER);
    }

    let _ = core::mem::replace(&mut cs_guard.ctx, CrypCtx::Finalized);
    output[..ct.len()].copy_from_slice(&ct);
    tag[..tag_val.len()].copy_from_slice(&tag_val);
    Ok(ct.len())
}

/// AEAD decrypt-final. On `SHORT_BUFFER`, writes required dst size to
/// `required_dst` and leaves the operation intact.
pub(crate) fn crypto_authenc_dec_final(
    cs: Arc<Mutex<TeeCrypState>>,
    input: Option<&[u8]>,
    output: &mut [u8],
    tag: &[u8],
    required_dst: &mut usize,
) -> TeeResult<usize> {
    let mut cs_guard = cs.lock();
    let CrypCtx::CipherCtx(ctx) = &cs_guard.ctx else {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    };

    let pt = ctx
        .clone()
        .decrypt_final_with_input(input, tag)
        .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
    *required_dst = pt.len();
    if output.len() < pt.len() {
        return Err(TEE_ERROR_SHORT_BUFFER);
    }

    let _ = core::mem::replace(&mut cs_guard.ctx, CrypCtx::Finalized);
    output[..pt.len()].copy_from_slice(&pt);
    Ok(pt.len())
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
    label: &[u8],
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
                rsa_ops::rsa_encrypt_oaep(&pubkey, hash_algo, label, input, &mut rng)
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
    label: &[u8],
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
                rsa_ops::rsa_decrypt_oaep(&keypair, hash_algo, label, &ciphertext)
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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Ed25519Keypair {
    pub priv_key: [u8; ED25519_KEY_SIZE_BYTES],
    pub pub_key: [u8; ED25519_KEY_SIZE_BYTES],
    pub curve: u32,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Ed25519PublicKey {
    pub pub_key: [u8; ED25519_KEY_SIZE_BYTES],
    pub curve: u32,
}

impl TeeCryptoOps for Ed25519Keypair {
    fn new(key_type: u32, key_size_bits: usize) -> TeeResult<Self> {
        if key_type != TEE_TYPE_ED25519_KEYPAIR || key_size_bits != 256 {
            return Err(TEE_ERROR_NOT_SUPPORTED);
        }
        Ok(Self {
            curve: TEE_ECC_CURVE_25519,
            ..Default::default()
        })
    }

    fn get_attr_by_id(&mut self, attr_id: TeeObjIdType) -> TeeResult<CryptoAttrRef<'_>> {
        match attr_id as u32 {
            TEE_ATTR_ED25519_PRIVATE_VALUE => Ok(CryptoAttrRef::Key32(&mut self.priv_key)),
            TEE_ATTR_ED25519_PUBLIC_VALUE => Ok(CryptoAttrRef::Key32(&mut self.pub_key)),
            _ => Err(TEE_ERROR_ITEM_NOT_FOUND),
        }
    }
}

impl TeeCryptoOps for Ed25519PublicKey {
    fn new(key_type: u32, key_size_bits: usize) -> TeeResult<Self> {
        if key_type != TEE_TYPE_ED25519_PUBLIC_KEY || key_size_bits != 256 {
            return Err(TEE_ERROR_NOT_SUPPORTED);
        }
        Ok(Self {
            curve: TEE_ECC_CURVE_25519,
            ..Default::default()
        })
    }

    fn get_attr_by_id(&mut self, attr_id: TeeObjIdType) -> TeeResult<CryptoAttrRef<'_>> {
        match attr_id as u32 {
            TEE_ATTR_ED25519_PUBLIC_VALUE => Ok(CryptoAttrRef::Key32(&mut self.pub_key)),
            _ => Err(TEE_ERROR_ITEM_NOT_FOUND),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct X25519Keypair {
    pub priv_key: [u8; X25519_KEY_SIZE_BYTES],
    pub pub_key: [u8; X25519_KEY_SIZE_BYTES],
    pub curve: u32,
}

impl TeeCryptoOps for X25519Keypair {
    fn new(key_type: u32, key_size_bits: usize) -> TeeResult<Self> {
        if key_type != TEE_TYPE_X25519_KEYPAIR || key_size_bits != 256 {
            return Err(TEE_ERROR_NOT_SUPPORTED);
        }
        Ok(Self {
            curve: TEE_ECC_CURVE_25519,
            ..Default::default()
        })
    }

    fn get_attr_by_id(&mut self, attr_id: TeeObjIdType) -> TeeResult<CryptoAttrRef<'_>> {
        match attr_id as u32 {
            TEE_ATTR_X25519_PRIVATE_VALUE => Ok(CryptoAttrRef::Key32(&mut self.priv_key)),
            TEE_ATTR_X25519_PUBLIC_VALUE => Ok(CryptoAttrRef::Key32(&mut self.pub_key)),
            _ => Err(TEE_ERROR_ITEM_NOT_FOUND),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct X25519PublicKey {
    pub pub_key: [u8; X25519_KEY_SIZE_BYTES],
    pub curve: u32,
}

impl TeeCryptoOps for X25519PublicKey {
    fn new(key_type: u32, key_size_bits: usize) -> TeeResult<Self> {
        if key_type != TEE_TYPE_X25519_PUBLIC_KEY || key_size_bits != 256 {
            return Err(TEE_ERROR_NOT_SUPPORTED);
        }
        Ok(Self {
            curve: TEE_ECC_CURVE_25519,
            ..Default::default()
        })
    }

    fn get_attr_by_id(&mut self, attr_id: TeeObjIdType) -> TeeResult<CryptoAttrRef<'_>> {
        match attr_id as u32 {
            TEE_ATTR_X25519_PUBLIC_VALUE => Ok(CryptoAttrRef::Key32(&mut self.pub_key)),
            _ => Err(TEE_ERROR_ITEM_NOT_FOUND),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct X448PublicKey {
    pub pub_key: [u8; X448_KEY_SIZE_BYTES],
}

impl Default for X448PublicKey {
    fn default() -> Self {
        Self {
            pub_key: [0u8; X448_KEY_SIZE_BYTES],
        }
    }
}

impl TeeCryptoOps for X448PublicKey {
    fn new(key_type: u32, key_size_bits: usize) -> TeeResult<Self> {
        if key_type != TEE_TYPE_X448_PUBLIC_KEY || key_size_bits != 448 {
            return Err(TEE_ERROR_NOT_SUPPORTED);
        }
        Ok(Self::default())
    }

    fn get_attr_by_id(&mut self, attr_id: TeeObjIdType) -> TeeResult<CryptoAttrRef<'_>> {
        match attr_id as u32 {
            TEE_ATTR_X448_PUBLIC_VALUE => Ok(CryptoAttrRef::Key56(&mut self.pub_key)),
            _ => Err(TEE_ERROR_ITEM_NOT_FOUND),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct X448Keypair {
    pub priv_key: [u8; X448_KEY_SIZE_BYTES],
    pub pub_key: [u8; X448_KEY_SIZE_BYTES],
}

impl Default for X448Keypair {
    fn default() -> Self {
        Self {
            priv_key: [0u8; X448_KEY_SIZE_BYTES],
            pub_key: [0u8; X448_KEY_SIZE_BYTES],
        }
    }
}

impl TeeCryptoOps for X448Keypair {
    fn new(key_type: u32, key_size_bits: usize) -> TeeResult<Self> {
        if key_type != TEE_TYPE_X448_KEYPAIR || key_size_bits != 448 {
            return Err(TEE_ERROR_NOT_SUPPORTED);
        }
        Ok(Self::default())
    }

    fn get_attr_by_id(&mut self, attr_id: TeeObjIdType) -> TeeResult<CryptoAttrRef<'_>> {
        match attr_id as u32 {
            TEE_ATTR_X448_PRIVATE_VALUE => Ok(CryptoAttrRef::Key56(&mut self.priv_key)),
            TEE_ATTR_X448_PUBLIC_VALUE => Ok(CryptoAttrRef::Key56(&mut self.pub_key)),
            _ => Err(TEE_ERROR_ITEM_NOT_FOUND),
        }
    }
}

pub(crate) struct Ed25519OperationParams {
    pub variant: Ed25519Variant,
}

pub(crate) fn parse_ed25519_operation_params(
    params: Option<&[KernelAttribute]>,
) -> TeeResult<Ed25519OperationParams> {
    let mut ph_flag = false;
    let mut ctx: Option<alloc::vec::Vec<u8>> = None;

    if let Some(params) = params {
        for attr in params {
            match attr {
                KernelAttribute::Value { id, a, .. } if *id == TEE_ATTR_EDDSA_PREHASH => match *a {
                    0 => ph_flag = false,
                    1 => ph_flag = true,
                    _ => return Err(TEE_ERROR_BAD_PARAMETERS),
                },
                KernelAttribute::Memref { id, data } if *id == TEE_ATTR_EDDSA_CTX => {
                    if ctx.is_some() {
                        return Err(TEE_ERROR_BAD_PARAMETERS);
                    }
                    if data.len() > ED25519_CTX_MAX_LENGTH {
                        return Err(TEE_ERROR_BAD_PARAMETERS);
                    }
                    if data.is_empty() {
                        continue;
                    }
                    ctx = Some(data.to_vec());
                }
                _ => {}
            }
        }
    }

    let variant = Ed25519Variant {
        prehash: ph_flag,
        context: ctx,
    };
    variant.validate().map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
    Ok(Ed25519OperationParams { variant })
}

fn map_ed25519_verify_err(err: tee_crypto::CryptoError) -> u32 {
    match err {
        tee_crypto::CryptoError::VerificationFailed => TEE_ERROR_SIGNATURE_INVALID,
        tee_crypto::CryptoError::UnsupportedAlgorithm => TEE_ERROR_NOT_SUPPORTED,
        _ => TEE_ERROR_BAD_PARAMETERS,
    }
}

pub(crate) fn crypto_acipher_gen_ed25519_key(
    key: &mut Ed25519Keypair,
    key_size_bits: usize,
) -> TeeResult {
    if key_size_bits != 256 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    let mut rng = TeeSoftwareRng::new();
    let (seed, public_key) =
        ed25519_ops::ed25519_generate_keypair(&mut rng).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
    key.priv_key = seed;
    key.pub_key = public_key;
    key.curve = TEE_ECC_CURVE_25519;
    Ok(())
}

pub(crate) fn crypto_acipher_gen_x25519_key(
    key: &mut X25519Keypair,
    key_size_bits: usize,
) -> TeeResult {
    if key_size_bits != 256 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    let mut rng = TeeSoftwareRng::new();
    let (private_key, public_key) =
        x25519_ops::x25519_generate_keypair(&mut rng).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
    key.priv_key = private_key;
    key.pub_key = public_key;
    key.curve = TEE_ECC_CURVE_25519;
    Ok(())
}

pub(crate) fn crypto_acipher_gen_x448_key(
    key: &mut X448Keypair,
    key_size_bits: usize,
) -> TeeResult {
    if key_size_bits != 448 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    let mut rng = TeeSoftwareRng::new();
    let (private_key, public_key) =
        x448_ops::x448_generate_keypair(&mut rng).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
    key.priv_key = private_key;
    key.pub_key = public_key;
    Ok(())
}

pub(crate) fn crypto_acipher_ed25519_sign(
    cs: Arc<Mutex<TeeCrypState>>,
    message: &[u8],
    output: &mut [u8],
    required: &mut usize,
    params: Option<&[KernelAttribute]>,
) -> TeeResult<usize> {
    let cs_guard = cs.lock();
    let key1 = cs_guard.key1.ok_or(TEE_ERROR_BAD_STATE)?;
    drop(cs_guard);

    let obj = tee_obj_get(key1 as _)?;
    let obj_guard = obj.lock();
    let key = match &obj_guard.attr[0] {
        TeeCryptObj::Ed25519Keypair(key) => key,
        _ => return Err(TEE_ERROR_BAD_STATE),
    };

    *required = ED25519_SIGNATURE_SIZE_BYTES;
    if output.len() < ED25519_SIGNATURE_SIZE_BYTES {
        return Err(TEE_ERROR_SHORT_BUFFER);
    }

    let op_params = parse_ed25519_operation_params(params)?;
    let signature = ed25519_ops::ed25519_sign_variant(&key.priv_key, message, &op_params.variant)
        .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
    let sig = signature.as_bytes();
    output[..sig.len()].copy_from_slice(sig);
    Ok(sig.len())
}

pub(crate) fn crypto_acipher_ed25519_verify(
    cs: Arc<Mutex<TeeCrypState>>,
    message: &[u8],
    signature: &[u8],
    params: Option<&[KernelAttribute]>,
) -> TeeResult {
    let cs_guard = cs.lock();
    let key1 = cs_guard.key1.ok_or(TEE_ERROR_BAD_STATE)?;
    drop(cs_guard);

    let obj = tee_obj_get(key1 as _)?;
    let obj_guard = obj.lock();
    let pub_key = match &obj_guard.attr[0] {
        TeeCryptObj::Ed25519PublicKey(key) => &key.pub_key,
        TeeCryptObj::Ed25519Keypair(key) => &key.pub_key,
        _ => return Err(TEE_ERROR_BAD_STATE),
    };

    let op_params = parse_ed25519_operation_params(params)?;
    ed25519_ops::ed25519_verify_variant(pub_key, message, signature, &op_params.variant)
        .map_err(map_ed25519_verify_err)
}

fn tee_alg_get_digest_hash(algo: u32) -> u32 {
    (algo >> 12) & 0xFF
}

fn uses_secret_value_type(type_props: &TeeCrypObjTypeProps) -> bool {
    type_props.type_attrs.as_ptr() == TEE_CRYP_OBJ_SECRET_VALUE_ATTRS.as_ptr()
        && type_props.type_attrs.len() == TEE_CRYP_OBJ_SECRET_VALUE_ATTRS.len()
}

fn obj_as_secret(o: &TeeObj) -> TeeResult<&TeeCrypObjSecretWrapper> {
    match &o.attr[0] {
        TeeCryptObj::ObjSecret(secret) => Ok(secret),
        _ => Err(TEE_ERROR_BAD_STATE),
    }
}

fn obj_as_secret_mut(o: &mut TeeObj) -> TeeResult<&mut TeeCrypObjSecretWrapper> {
    match &mut o.attr[0] {
        TeeCryptObj::ObjSecret(secret) => Ok(secret),
        _ => Err(TEE_ERROR_BAD_STATE),
    }
}

fn finalize_derived_secret(
    so: &mut TeeObj,
    type_props: &TeeCrypObjTypeProps,
    key_material: &[u8],
) -> TeeResult {
    let secret = obj_as_secret_mut(so)?;
    if key_material.len() > secret.secret().alloc_size as usize {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    secret.data_mut()[..key_material.len()].copy_from_slice(key_material);
    if key_material.len() < secret.data().len() {
        secret.data_mut()[key_material.len()..].fill(0);
    }
    secret.secret_mut().key_size = key_material.len() as u32;
    so.info.handleFlags |= TEE_HANDLE_FLAG_INITIALIZED;
    set_attribute(so, type_props, TEE_ATTR_SECRET_VALUE);
    Ok(())
}

struct HkdfParams<'a> {
    salt: &'a [u8],
    info: &'a [u8],
    okm_len: usize,
    hash_id: u32,
}

fn get_hkdf_params(algo: u32, attrs: &[KernelAttribute]) -> TeeResult<HkdfParams<'_>> {
    let mut salt: Option<&[u8]> = None;
    let mut info: Option<&[u8]> = None;
    let mut okm_len: Option<usize> = None;
    let mut hash_id = if algo == TEE_ALG_HKDF {
        TEE_MAIN_ALGO_SHA256
    } else {
        tee_alg_get_digest_hash(algo)
    };

    for attr in attrs {
        match attr.id() {
            __OPTEE_TEE_ATTR_HKDF_SALT | TEE_ATTR_HKDF_SALT if salt.is_none() => {
                salt = Some(attr.memref_data()?);
            }
            __OPTEE_TEE_ATTR_HKDF_INFO | TEE_ATTR_HKDF_INFO if info.is_none() => {
                info = Some(attr.memref_data()?);
            }
            TEE_ATTR_HKDF_OKM_LENGTH | TEE_ATTR_KDF_KEY_SIZE if okm_len.is_none() => {
                let (len, _) = attr.value()?;
                okm_len = Some(len as usize);
            }
            TEE_ATTR_HKDF_HASH_ALGORITHM if algo == TEE_ALG_HKDF => {
                let (id, _) = attr.value()?;
                hash_id = id;
            }
            _ => return Err(TEE_ERROR_BAD_PARAMETERS),
        }
    }

    let okm_len = okm_len.ok_or(TEE_ERROR_BAD_PARAMETERS)?;
    Ok(HkdfParams {
        salt: salt.unwrap_or(&[]),
        info: info.unwrap_or(&[]),
        okm_len,
        hash_id,
    })
}

fn hkdf_derive(
    hash_id: u32,
    ikm: &[u8],
    salt: &[u8],
    info: &[u8],
    okm_len: usize,
) -> TeeResult<Vec<u8>> {
    let okm = match hash_id {
        TEE_MAIN_ALGO_MD5 | TEE_ALG_HKDF_MD5_DERIVE_KEY => {
            hkdf::<HmacMd5>(salt, ikm, info, okm_len).map_err(|_| TEE_ERROR_GENERIC)?
        }
        TEE_MAIN_ALGO_SHA1 | TEE_ALG_HKDF_SHA1_DERIVE_KEY => {
            hkdf::<HmacSha1>(salt, ikm, info, okm_len).map_err(|_| TEE_ERROR_GENERIC)?
        }
        TEE_MAIN_ALGO_SHA224 | TEE_ALG_HKDF_SHA224_DERIVE_KEY => {
            hkdf::<HmacSha224>(salt, ikm, info, okm_len).map_err(|_| TEE_ERROR_GENERIC)?
        }
        TEE_MAIN_ALGO_SHA256 | TEE_ALG_HKDF_SHA256_DERIVE_KEY => {
            hkdf::<HmacSha256>(salt, ikm, info, okm_len).map_err(|_| TEE_ERROR_GENERIC)?
        }
        TEE_MAIN_ALGO_SHA384 | TEE_ALG_HKDF_SHA384_DERIVE_KEY => {
            hkdf::<HmacSha384>(salt, ikm, info, okm_len).map_err(|_| TEE_ERROR_GENERIC)?
        }
        TEE_MAIN_ALGO_SHA512 | TEE_ALG_HKDF_SHA512_DERIVE_KEY => {
            hkdf::<HmacSha512>(salt, ikm, info, okm_len).map_err(|_| TEE_ERROR_GENERIC)?
        }
        TEE_MAIN_ALGO_SM3 => {
            hkdf::<HmacSm3>(salt, ikm, info, okm_len).map_err(|_| TEE_ERROR_GENERIC)?
        }
        _ => return Err(TEE_ERROR_NOT_SUPPORTED),
    };
    Ok(okm)
}

fn derive_hkdf(
    ko: &TeeObj,
    so: &mut TeeObj,
    so_props: &TeeCrypObjTypeProps,
    algo: u32,
    attrs: &[KernelAttribute],
) -> TeeResult {
    let params = get_hkdf_params(algo, attrs)?;
    let ikm = obj_as_secret(ko)?.key();
    if ikm.is_empty() {
        return Err(TEE_ERROR_BAD_STATE);
    }
    if params.okm_len > obj_as_secret(so)?.secret().alloc_size as usize {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    let okm = hkdf_derive(
        params.hash_id,
        ikm,
        params.salt,
        params.info,
        params.okm_len,
    )?;
    finalize_derived_secret(so, so_props, &okm)
}

struct ConcatKdfParams<'a> {
    other_info: &'a [u8],
    dkm_len: usize,
}

fn digest_hash_to_hash_algorithm(hash_id: u32) -> TeeResult<HashAlgorithm> {
    match hash_id {
        TEE_MAIN_ALGO_SHA1 | TEE_ALG_CONCAT_KDF_SHA1_DERIVE_KEY => Ok(HashAlgorithm::Sha1),
        TEE_MAIN_ALGO_SHA224 | TEE_ALG_CONCAT_KDF_SHA224_DERIVE_KEY => Ok(HashAlgorithm::Sha224),
        TEE_MAIN_ALGO_SHA256 | TEE_ALG_CONCAT_KDF_SHA256_DERIVE_KEY => Ok(HashAlgorithm::Sha256),
        TEE_MAIN_ALGO_SHA384 | TEE_ALG_CONCAT_KDF_SHA384_DERIVE_KEY => Ok(HashAlgorithm::Sha384),
        TEE_MAIN_ALGO_SHA512 | TEE_ALG_CONCAT_KDF_SHA512_DERIVE_KEY => Ok(HashAlgorithm::Sha512),
        _ => Err(TEE_ERROR_NOT_SUPPORTED),
    }
}

fn get_concat_kdf_params(attrs: &[KernelAttribute]) -> TeeResult<ConcatKdfParams<'_>> {
    let mut other_info: Option<&[u8]> = None;
    let mut dkm_len: Option<usize> = None;

    for attr in attrs {
        match attr.id() {
            TEE_ATTR_CONCAT_KDF_OTHER_INFO if other_info.is_none() => {
                other_info = Some(attr.memref_data()?);
            }
            TEE_ATTR_CONCAT_KDF_DKM_LENGTH if dkm_len.is_none() => {
                let (len, _) = attr.value()?;
                dkm_len = Some(len as usize);
            }
            _ => return Err(TEE_ERROR_BAD_PARAMETERS),
        }
    }

    let dkm_len = dkm_len.ok_or(TEE_ERROR_BAD_PARAMETERS)?;
    Ok(ConcatKdfParams {
        other_info: other_info.unwrap_or(&[]),
        dkm_len,
    })
}

fn derive_concat_kdf(
    ko: &TeeObj,
    so: &mut TeeObj,
    so_props: &TeeCrypObjTypeProps,
    algo: u32,
    attrs: &[KernelAttribute],
) -> TeeResult {
    let params = get_concat_kdf_params(attrs)?;
    let shared_secret = obj_as_secret(ko)?.key();
    if shared_secret.is_empty() {
        return Err(TEE_ERROR_BAD_STATE);
    }
    if params.dkm_len > obj_as_secret(so)?.secret().alloc_size as usize {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    let hash_algo = digest_hash_to_hash_algorithm(tee_alg_get_digest_hash(algo))?;
    let dkm = concat_kdf(hash_algo, shared_secret, params.other_info, params.dkm_len)
        .map_err(|_| TEE_ERROR_GENERIC)?;
    finalize_derived_secret(so, so_props, &dkm)
}

struct Pbkdf2Params<'a> {
    salt: &'a [u8],
    dkm_len: usize,
    iteration_count: usize,
}

fn get_pbkdf2_params(attrs: &[KernelAttribute]) -> TeeResult<Pbkdf2Params<'_>> {
    let mut salt: Option<&[u8]> = None;
    let mut dkm_len: Option<usize> = None;
    let mut iteration_count: Option<usize> = None;

    for attr in attrs {
        match attr.id() {
            TEE_ATTR_PBKDF2_SALT if salt.is_none() => {
                salt = Some(attr.memref_data()?);
            }
            TEE_ATTR_PBKDF2_DKM_LENGTH if dkm_len.is_none() => {
                let (len, _) = attr.value()?;
                dkm_len = Some(len as usize);
            }
            TEE_ATTR_PBKDF2_ITERATION_COUNT if iteration_count.is_none() => {
                let (count, _) = attr.value()?;
                iteration_count = Some(count as usize);
            }
            _ => return Err(TEE_ERROR_BAD_PARAMETERS),
        }
    }

    let dkm_len = dkm_len.ok_or(TEE_ERROR_BAD_PARAMETERS)?;
    let iteration_count = iteration_count.ok_or(TEE_ERROR_BAD_PARAMETERS)?;
    Ok(Pbkdf2Params {
        salt: salt.unwrap_or(&[]),
        dkm_len,
        iteration_count,
    })
}

fn pbkdf2_derive(
    hash_id: u32,
    password: &[u8],
    salt: &[u8],
    iteration_count: usize,
    dkm_len: usize,
) -> TeeResult<Vec<u8>> {
    match hash_id {
        TEE_MAIN_ALGO_SHA1 | TEE_ALG_PBKDF2_HMAC_SHA1_DERIVE_KEY => {
            pbkdf2_hmac_sha1(password, salt, iteration_count, dkm_len)
                .map_err(|_| TEE_ERROR_GENERIC)
        }
        _ => Err(TEE_ERROR_NOT_SUPPORTED),
    }
}

fn derive_pbkdf2(
    ko: &TeeObj,
    so: &mut TeeObj,
    so_props: &TeeCrypObjTypeProps,
    algo: u32,
    attrs: &[KernelAttribute],
) -> TeeResult {
    let params = get_pbkdf2_params(attrs)?;
    let password = obj_as_secret(ko)?.key();
    if password.is_empty() {
        return Err(TEE_ERROR_BAD_STATE);
    }
    if params.dkm_len > obj_as_secret(so)?.secret().alloc_size as usize {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    let dkm = pbkdf2_derive(
        tee_alg_get_digest_hash(algo),
        password,
        params.salt,
        params.iteration_count,
        params.dkm_len,
    )?;
    finalize_derived_secret(so, so_props, &dkm)
}

fn pad_ecc_field_bytes(bytes: &[u8], field_len: usize) -> Vec<u8> {
    if bytes.len() >= field_len {
        bytes[bytes.len() - field_len..].to_vec()
    } else {
        let mut out = Vec::with_capacity(field_len);
        out.resize(field_len - bytes.len(), 0);
        out.extend_from_slice(bytes);
        out
    }
}

fn derive_ecdh(
    ko: &TeeObj,
    so: &mut TeeObj,
    so_props: &TeeCrypObjTypeProps,
    attrs: &[KernelAttribute],
) -> TeeResult {
    if attrs.len() != 2 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    if attrs[0].id() != TEE_ATTR_ECC_PUBLIC_VALUE_X || attrs[1].id() != TEE_ATTR_ECC_PUBLIC_VALUE_Y
    {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    let peer_x = attrs[0].memref_data()?;
    let peer_y = attrs[1].memref_data()?;

    let ecc_kp = match &ko.attr[0] {
        TeeCryptObj::EccKeypair(kp) => kp,
        _ => return Err(TEE_ERROR_BAD_STATE),
    };
    let curve = tee_curve_to_ecc_curve(ecc_kp.curve)?;
    let field_len = ecc_curve_field_byte_len(curve);
    let local_secret = bn_to_ecc_field_bytes(&ecc_kp.d, field_len);
    let peer_x = pad_ecc_field_bytes(peer_x, field_len);
    let peer_y = pad_ecc_field_bytes(peer_y, field_len);
    let shared = ecc_ops::ecc_shared_secret(curve, &local_secret, &peer_x, &peer_y)
        .map_err(|_| TEE_ERROR_GENERIC)?;
    let shared_bytes = shared.expose_secret();
    if shared_bytes.len() > obj_as_secret(so)?.secret().alloc_size as usize {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    finalize_derived_secret(so, so_props, shared_bytes)
}

fn derive_sm2_kep(
    ko: &TeeObj,
    ko2: &TeeObj,
    so: &mut TeeObj,
    so_props: &TeeCrypObjTypeProps,
    attrs: &[KernelAttribute],
) -> TeeResult {
    let params = get_sm2_kep_params(attrs)?;
    let static_kp = obj_as_sm2_kep_keypair(ko)?;
    let eph_kp = obj_as_sm2_kep_keypair(ko2)?;
    let my_static = sm2_kep_from_ecc_keypair(static_kp)?;
    let my_ephemeral = sm2_kep_from_ecc_keypair(eph_kp)?;

    let key_len = (so.info.maxObjectSize / 8) as usize;
    if key_len == 0 || key_len > obj_as_secret(so)?.secret().alloc_size as usize {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    let compute_conf_out = params.conf_out.is_some();
    if let Some((_, len)) = params.conf_out
        && len < 32
    {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    let derive_params = Sm2KepDeriveParams {
        is_initiator: params.is_initiator,
        initiator_id: params.initiator_id,
        responder_id: params.responder_id,
        peer: Sm2KepPeerKeys {
            static_x: params.peer_static_x,
            static_y: params.peer_static_y,
            ephemeral_x: params.peer_ephemeral_x,
            ephemeral_y: params.peer_ephemeral_y,
        },
        conf_in: params.conf_in,
        confirmation: if compute_conf_out {
            ConfirmationMode::Compute
        } else {
            ConfirmationMode::Skip
        },
    };

    let out = sm2_kep_derive(&my_static, &my_ephemeral, &derive_params, key_len)
        .map_err(map_sm2_kep_err)?;

    finalize_derived_secret(so, so_props, &out.key)?;

    if let Some((user_addr, buf_len)) = params.conf_out {
        let conf = out.conf_out.as_ref().ok_or(TEE_ERROR_GENERIC)?;
        write_user_memref_out(user_addr, buf_len, conf)?;
    }

    Ok(())
}

struct Sm2KepParams<'a> {
    is_initiator: bool,
    initiator_id: &'a [u8],
    responder_id: &'a [u8],
    peer_static_x: [u8; 32],
    peer_static_y: [u8; 32],
    peer_ephemeral_x: [u8; 32],
    peer_ephemeral_y: [u8; 32],
    conf_in: Option<&'a [u8]>,
    conf_out: Option<(usize, usize)>,
}

fn get_sm2_kep_params(attrs: &[KernelAttribute]) -> TeeResult<Sm2KepParams<'_>> {
    let mut is_initiator: Option<bool> = None;
    let mut peer_static_x: Option<[u8; 32]> = None;
    let mut peer_static_y: Option<[u8; 32]> = None;
    let mut peer_ephemeral_x: Option<[u8; 32]> = None;
    let mut peer_ephemeral_y: Option<[u8; 32]> = None;
    let mut initiator_id: Option<&[u8]> = None;
    let mut responder_id: Option<&[u8]> = None;
    let mut conf_in: Option<&[u8]> = None;
    let mut conf_out: Option<(usize, usize)> = None;

    for attr in attrs {
        match attr {
            KernelAttribute::Value { id, a, .. } if *id == TEE_ATTR_SM2_KEP_USER => {
                if is_initiator.is_some() {
                    return Err(TEE_ERROR_BAD_PARAMETERS);
                }
                is_initiator = Some(*a == 0);
            }
            KernelAttribute::MemrefOut {
                id: TEE_ATTR_SM2_KEP_CONFIRMATION_OUT,
                user_addr,
                len,
            } if conf_out.is_none() => {
                conf_out = Some((*user_addr, *len));
            }
            KernelAttribute::Memref { id, data } => match *id {
                TEE_ATTR_ECC_PUBLIC_VALUE_X if peer_static_x.is_none() => {
                    peer_static_x = Some(memref_to_field32(data)?);
                }
                TEE_ATTR_ECC_PUBLIC_VALUE_Y if peer_static_y.is_none() => {
                    peer_static_y = Some(memref_to_field32(data)?);
                }
                TEE_ATTR_ECC_EPHEMERAL_PUBLIC_VALUE_X if peer_ephemeral_x.is_none() => {
                    peer_ephemeral_x = Some(memref_to_field32(data)?);
                }
                TEE_ATTR_ECC_EPHEMERAL_PUBLIC_VALUE_Y if peer_ephemeral_y.is_none() => {
                    peer_ephemeral_y = Some(memref_to_field32(data)?);
                }
                TEE_ATTR_SM2_ID_INITIATOR if initiator_id.is_none() => {
                    initiator_id = Some(data);
                }
                TEE_ATTR_SM2_ID_RESPONDER if responder_id.is_none() => {
                    responder_id = Some(data);
                }
                TEE_ATTR_SM2_KEP_CONFIRMATION_IN if conf_in.is_none() => {
                    conf_in = Some(data);
                }
                _ => return Err(TEE_ERROR_BAD_PARAMETERS),
            },
            _ => return Err(TEE_ERROR_BAD_PARAMETERS),
        }
    }

    Ok(Sm2KepParams {
        is_initiator: is_initiator.ok_or(TEE_ERROR_BAD_PARAMETERS)?,
        initiator_id: initiator_id.ok_or(TEE_ERROR_BAD_PARAMETERS)?,
        responder_id: responder_id.ok_or(TEE_ERROR_BAD_PARAMETERS)?,
        peer_static_x: peer_static_x.ok_or(TEE_ERROR_BAD_PARAMETERS)?,
        peer_static_y: peer_static_y.ok_or(TEE_ERROR_BAD_PARAMETERS)?,
        peer_ephemeral_x: peer_ephemeral_x.ok_or(TEE_ERROR_BAD_PARAMETERS)?,
        peer_ephemeral_y: peer_ephemeral_y.ok_or(TEE_ERROR_BAD_PARAMETERS)?,
        conf_in,
        conf_out,
    })
}

fn obj_as_sm2_kep_keypair(o: &TeeObj) -> TeeResult<&EccKeypair> {
    match &o.attr[0] {
        TeeCryptObj::EccKeypair(kp) if kp.curve == TEE_ECC_CURVE_SM2 => Ok(kp),
        _ => Err(TEE_ERROR_BAD_STATE),
    }
}

fn sm2_kep_from_ecc_keypair(kp: &EccKeypair) -> TeeResult<Sm2KepKeypair> {
    let secret = bn_to_ecc_field_bytes(&kp.d, 32);
    let secret: [u8; 32] = secret.try_into().map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
    Sm2KepKeypair::from_secret_bytes(&secret).map_err(|_| TEE_ERROR_BAD_PARAMETERS)
}

fn memref_to_field32(data: &[u8]) -> TeeResult<[u8; 32]> {
    let padded = pad_ecc_field_bytes(data, 32);
    padded.try_into().map_err(|_| TEE_ERROR_BAD_PARAMETERS)
}

fn map_sm2_kep_err(err: tee_crypto::CryptoError) -> u32 {
    match err {
        tee_crypto::CryptoError::VerificationFailed => TEE_ERROR_BAD_STATE,
        tee_crypto::CryptoError::InvalidInput
        | tee_crypto::CryptoError::InvalidLength
        | tee_crypto::CryptoError::InvalidKey => TEE_ERROR_BAD_PARAMETERS,
        _ => TEE_ERROR_GENERIC,
    }
}

fn derive_dh(
    _ko: &TeeObj,
    _so: &mut TeeObj,
    _so_props: &TeeCrypObjTypeProps,
    _attrs: &[KernelAttribute],
) -> TeeResult {
    Err(TEE_ERROR_NOT_SUPPORTED)
}

fn derive_key_by_algo(
    algo: u32,
    ko: &TeeObj,
    ko2: Option<&TeeObj>,
    so: &mut TeeObj,
    so_props: &TeeCrypObjTypeProps,
    attrs: &[KernelAttribute],
) -> TeeResult {
    let algo = translate_compat_algo(algo);
    match algo {
        TEE_ALG_DH_DERIVE_SHARED_SECRET => derive_dh(ko, so, so_props, attrs),
        TEE_ALG_ECDH_DERIVE_SHARED_SECRET => derive_ecdh(ko, so, so_props, attrs),
        TEE_ALG_SM2_KEP => derive_sm2_kep(ko, ko2.ok_or(TEE_ERROR_BAD_STATE)?, so, so_props, attrs),
        TEE_ALG_X25519 => derive_x25519(ko, so, so_props, attrs),
        TEE_ALG_X448 => derive_x448(ko, so, so_props, attrs),
        _ => match tee_alg_get_main_alg(algo) {
            TEE_MAIN_ALGO_HKDF => derive_hkdf(ko, so, so_props, algo, attrs),
            TEE_MAIN_ALGO_CONCAT_KDF => derive_concat_kdf(ko, so, so_props, algo, attrs),
            TEE_MAIN_ALGO_PBKDF2 => derive_pbkdf2(ko, so, so_props, algo, attrs),
            _ => Err(TEE_ERROR_NOT_SUPPORTED),
        },
    }
}

fn derive_x25519(
    ko: &TeeObj,
    so: &mut TeeObj,
    so_props: &TeeCrypObjTypeProps,
    attrs: &[KernelAttribute],
) -> TeeResult {
    if attrs.len() != 1 || attrs[0].id() != TEE_ATTR_X25519_PUBLIC_VALUE {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    let peer_public = attrs[0].memref_data()?;
    if peer_public.len() != X25519_KEY_SIZE_BYTES {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    let peer_public: &[u8; X25519_KEY_SIZE_BYTES] = peer_public
        .try_into()
        .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;

    let x25519_kp = match &ko.attr[0] {
        TeeCryptObj::X25519Keypair(kp) => kp,
        _ => return Err(TEE_ERROR_BAD_STATE),
    };
    if X25519_KEY_SIZE_BYTES > obj_as_secret(so)?.secret().alloc_size as usize {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    let shared = x25519_ops::x25519_shared_secret(&x25519_kp.priv_key, peer_public).map_err(
        |err| match err {
            tee_crypto::CryptoError::VerificationFailed => TEE_ERROR_SECURITY,
            _ => TEE_ERROR_BAD_PARAMETERS,
        },
    )?;
    finalize_derived_secret(so, so_props, &shared)
}

fn derive_x448(
    ko: &TeeObj,
    so: &mut TeeObj,
    so_props: &TeeCrypObjTypeProps,
    attrs: &[KernelAttribute],
) -> TeeResult {
    if attrs.len() != 1 || attrs[0].id() != TEE_ATTR_X448_PUBLIC_VALUE {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    let peer_public = attrs[0].memref_data()?;
    if peer_public.len() != X448_KEY_SIZE_BYTES {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    let peer_public: &[u8; X448_KEY_SIZE_BYTES] = peer_public
        .try_into()
        .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;

    let x448_kp = match &ko.attr[0] {
        TeeCryptObj::X448Keypair(kp) => kp,
        _ => return Err(TEE_ERROR_BAD_STATE),
    };
    if X448_KEY_SIZE_BYTES > obj_as_secret(so)?.secret().alloc_size as usize {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    let shared =
        x448_ops::x448_shared_secret(&x448_kp.priv_key, peer_public).map_err(|err| match err {
            tee_crypto::CryptoError::VerificationFailed => TEE_ERROR_SECURITY,
            _ => TEE_ERROR_BAD_PARAMETERS,
        })?;
    finalize_derived_secret(so, so_props, &shared)
}

pub(crate) fn tee_cryp_derive_key(
    state: u32,
    attrs: &[KernelAttribute],
    derived_key: TeeObjIdType,
) -> TeeResult {
    let cs = tee_cryp_state_get(state)?;
    let cs_guard = cs.lock();

    if cs_guard.mode != TEE_OperationMode::TEE_MODE_DERIVE {
        return Err(TEE_ERROR_BAD_STATE);
    }
    if tee_alg_get_class(cs_guard.algo) != TEE_OPERATION_KEY_DERIVATION {
        return Err(TEE_ERROR_BAD_STATE);
    }
    let key1 = cs_guard.key1.ok_or(TEE_ERROR_BAD_STATE)?;
    let key2 = cs_guard.key2;
    let algo = cs_guard.algo;
    drop(cs_guard);

    let ko = tee_obj_get(key1 as TeeObjIdType)?;
    let so = tee_obj_get(derived_key)?;
    let ko2 = if translate_compat_algo(algo) == TEE_ALG_SM2_KEP {
        Some(tee_obj_get(
            key2.ok_or(TEE_ERROR_BAD_STATE)? as TeeObjIdType
        )?)
    } else {
        None
    };

    // ksync::Mutex 在同一任务再次获取自己已持有的锁时会断言 panic。派生目标要在
    // 运算密钥保持加锁时写入，所以它必须是另一把锁。
    if Arc::ptr_eq(&ko, &so)
        || ko2
            .as_ref()
            .is_some_and(|obj| Arc::ptr_eq(obj, &ko) || Arc::ptr_eq(obj, &so))
    {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    let ko_guard = ko.lock();
    let ko2_guard = ko2.as_ref().map(|obj| obj.lock());
    let mut so_guard = so.lock();

    if so_guard.info.handleFlags & TEE_HANDLE_FLAG_INITIALIZED != 0 {
        return Err(TEE_ERROR_BAD_STATE);
    }

    let so_props =
        tee_svc_find_type_props(so_guard.info.objectType).ok_or(TEE_ERROR_NOT_SUPPORTED)?;
    if !uses_secret_value_type(so_props) {
        return Err(TEE_ERROR_NOT_SUPPORTED);
    }

    derive_key_by_algo(
        algo,
        &ko_guard,
        ko2_guard.as_deref(),
        &mut so_guard,
        so_props,
        attrs,
    )
}

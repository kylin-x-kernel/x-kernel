// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, format, sync::Arc};
use core::{default::Default, fmt, fmt::Debug};

use ksync::Mutex;
use mbedtls::{
    bignum::Mpi,
    cipher::raw::{Cipher, CipherId, CipherMode, CipherPadding, Operation},
    ecp::EcPoint,
    error::{Error as MbedError, HiError, LoError},
    hash::{Hmac, Md, Type as MdType},
    pk::{
        EcGroup, EcGroupId, Pk, RsaPadding, RsaPrivateComponents, RsaPublicComponents,
        Type as PkType,
    },
};
use mbedtls_sys_auto::{
    ERR_SM2_ALLOC_FAILED, ERR_SM2_BAD_INPUT_DATA, ERR_SM2_BAD_SIGNATURE, mpi_write_binary,
};
use tee_raw_sys::*;

use crate::tee::{
    TEE_ALG_DES3_CMAC, TEE_ALG_RSAES_PKCS1_OAEP_MGF1_MD5, TEE_ALG_RSASSA_PKCS1_PSS_MGF1_MD5,
    TeeResult,
    crypto::{
        aes_xts::{
            TeeCipherXtsCtx, aes_xts_final_buffered, aes_xts_init, aes_xts_update_buffered,
            cipher_uses_aes_xts_kernel,
        },
        authenc_aad::{TeeAuthencAadCtx, cipher_uses_authenc_aad_buffer},
        crypto_impl::{
            EccAlgoKeyPair, EccComKeyPair, EccKeypair, Sm2DsaKeyPair, Sm2KepKeyPair, Sm2PkeKeyPair,
            crypto_ecc_keypair_ops, crypto_ecc_keypair_ops_generate,
        },
    },
    libmbedtls::{
        bignum::{BigNum, crypto_bignum_allocate},
        ecc::{EcdOps, Sm2DsaOps, Sm2KepOps, Sm2PkeOps},
    },
    rng_software::TeeSoftwareRng,
    tee_api_defines_extensions::TEE_ALG_SM4_XTS,
    tee_obj::{tee_obj_get, tee_obj_id_type},
    tee_svc_cryp::{CryptoAttrRef, TeeCryptObj, tee_cryp_obj_secret_wrapper, tee_crypto_ops},
    tee_svc_cryp2::{CipherPaddingMode, CrypCtx, CrypState, TeeCipherCtx, TeeCrypState},
};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ecc_public_key {
    pub x: BigNum,
    pub y: BigNum,
    curve: u32,
    // ops: Box<dyn crypto_ecc_public_ops>,
}

impl tee_crypto_ops for ecc_public_key {
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

        Ok(ecc_public_key {
            x: crypto_bignum_allocate(key_size_bits)?,
            y: crypto_bignum_allocate(key_size_bits)?,
            curve,
        })
    }

    fn get_attr_by_id(&mut self, attr_id: tee_obj_id_type) -> TeeResult<CryptoAttrRef<'_>> {
        match attr_id as u32 {
            TEE_ATTR_ECC_PUBLIC_VALUE_X => Ok(CryptoAttrRef::BigNum(&mut self.x)),
            TEE_ATTR_ECC_PUBLIC_VALUE_Y => Ok(CryptoAttrRef::BigNum(&mut self.y)),
            TEE_ATTR_ECC_CURVE => Ok(CryptoAttrRef::U32(&mut self.curve)),
            _ => Err(TEE_ERROR_ITEM_NOT_FOUND),
        }
    }
}
#[derive(Default)]
pub struct ecc_keypair {
    pub d: BigNum,
    pub x: BigNum,
    pub y: BigNum,
    pub curve: u32,
    // TODO: add ops
    // pub ops: Box<dyn crypto_ecc_keypair_ops>,
}

impl Debug for ecc_keypair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ecc_keypair")
            .field("d", &self.d)
            .field("x", &self.x)
            .field("y", &self.y)
            .field("curve", &format!("{:#010X?}", self.curve))
            .finish()
    }
}

impl tee_crypto_ops for ecc_keypair {
    fn new(key_type: u32, key_size_bits: usize) -> TeeResult<Self> {
        let mut curve = 0;

        let _ops: Box<dyn crypto_ecc_keypair_ops> = match key_type {
            TEE_TYPE_ECDSA_KEYPAIR | TEE_TYPE_ECDH_KEYPAIR => Box::new(EcdOps),
            TEE_TYPE_SM2_DSA_KEYPAIR => {
                curve = TEE_ECC_CURVE_SM2;
                Box::new(Sm2DsaOps)
            }
            TEE_TYPE_SM2_PKE_KEYPAIR => {
                curve = TEE_ECC_CURVE_SM2;
                Box::new(Sm2PkeOps)
            }
            TEE_TYPE_SM2_KEP_KEYPAIR => {
                curve = TEE_ECC_CURVE_SM2;
                Box::new(Sm2KepOps)
            }
            _ => return Err(TEE_ERROR_NOT_IMPLEMENTED),
        };

        Ok(ecc_keypair {
            d: crypto_bignum_allocate(key_size_bits)?,
            x: crypto_bignum_allocate(key_size_bits)?,
            y: crypto_bignum_allocate(key_size_bits)?,
            curve,
            // ops,
        })
    }

    fn get_attr_by_id(&mut self, attr_id: tee_obj_id_type) -> TeeResult<CryptoAttrRef<'_>> {
        match attr_id as u32 {
            TEE_ATTR_ECC_PRIVATE_VALUE => Ok(CryptoAttrRef::BigNum(&mut self.d)),
            TEE_ATTR_ECC_PUBLIC_VALUE_X => Ok(CryptoAttrRef::BigNum(&mut self.x)),
            TEE_ATTR_ECC_PUBLIC_VALUE_Y => Ok(CryptoAttrRef::BigNum(&mut self.y)),
            TEE_ATTR_ECC_CURVE => Ok(CryptoAttrRef::U32(&mut self.curve)),
            _ => Err(TEE_ERROR_ITEM_NOT_FOUND),
        }
    }
}

impl PartialEq for ecc_keypair {
    fn eq(&self, other: &Self) -> bool {
        self.d == other.d && self.x == other.x && self.y == other.y && self.curve == other.curve
    }
}

impl Eq for ecc_keypair {}

pub struct rsa_keypair {
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

impl Debug for rsa_keypair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("rsa_keypair")
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

impl tee_crypto_ops for rsa_keypair {
    fn new(_key_type: u32, key_size_bits: usize) -> TeeResult<Self> {
        Ok(rsa_keypair {
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

    fn get_attr_by_id(&mut self, attr_id: tee_obj_id_type) -> TeeResult<CryptoAttrRef<'_>> {
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

pub struct rsa_public_key {
    pub e: BigNum, // Public exponent
    pub n: BigNum, // Modulus
}

impl Debug for rsa_public_key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("rsa_public_key")
            .field("e", &self.e)
            .field("n", &self.n)
            .finish()
    }
}

impl tee_crypto_ops for rsa_public_key {
    fn new(_key_type: u32, key_size_bits: usize) -> TeeResult<Self> {
        Ok(rsa_public_key {
            e: crypto_bignum_allocate(key_size_bits)?,
            n: crypto_bignum_allocate(key_size_bits)?,
        })
    }

    fn get_attr_by_id(&mut self, attr_id: tee_obj_id_type) -> TeeResult<CryptoAttrRef<'_>> {
        match attr_id as u32 {
            TEE_ATTR_RSA_MODULUS => Ok(CryptoAttrRef::BigNum(&mut self.n)),
            TEE_ATTR_RSA_PUBLIC_EXPONENT => Ok(CryptoAttrRef::BigNum(&mut self.e)),
            _ => Err(TEE_ERROR_ITEM_NOT_FOUND),
        }
    }
}

pub fn crypto_acipher_gen_ecc_key(
    key: &mut ecc_keypair,
    key_size_bits: usize,
    object_type: u32,
) -> TeeResult {
    let mut key: Box<dyn crypto_ecc_keypair_ops_generate> = match object_type {
        TEE_TYPE_ECDSA_KEYPAIR | TEE_TYPE_ECDH_KEYPAIR => {
            Box::new(EccKeypair::<EccComKeyPair>::new(key))
        }
        TEE_TYPE_SM2_PKE_KEYPAIR => Box::new(EccKeypair::<Sm2PkeKeyPair>::new(key)),
        TEE_TYPE_SM2_DSA_KEYPAIR => Box::new(EccKeypair::<Sm2DsaKeyPair>::new(key)),
        TEE_TYPE_SM2_KEP_KEYPAIR => Box::new(EccKeypair::<Sm2KepKeyPair>::new(key)),
        _ => return Err(TEE_ERROR_NOT_IMPLEMENTED),
    };
    key.generate(key_size_bits)
}

// The crypto context used by the crypto_hash_*() functions
pub(crate) struct CryptoHashContext {
    pub ops: Option<&'static CryptoHashOps>,
}

// Constructor for CryptoHashCtx
pub(crate) struct CryptoHashOps {
    pub init: Option<fn(ctx: &mut CryptoHashContext) -> TeeResult>,
    pub update: Option<fn(ctx: &mut CryptoHashContext, data: &[u8]) -> TeeResult>,
    pub final_: Option<fn(ctx: &mut CryptoHashContext, digest: &mut [u8]) -> TeeResult>,
    pub free_ctx: Option<fn(ctx: &mut CryptoHashContext)>,
    pub copy_state: Option<fn(dst_ctx: &mut CryptoHashContext, src_ctx: &CryptoHashContext)>,
}

// defining hash operations for cryptographic hashing
pub(crate) trait CryptoHashCtx {
    // Initialize the hash context
    fn init(&mut self) -> TeeResult;

    // Update the hash context with data
    fn update(&mut self, data: &[u8]) -> TeeResult;

    // Finalize the hash computation and return the digest
    fn r#final(&mut self, digest: &mut [u8]) -> TeeResult;

    // Free the hash context resources
    fn free_ctx(self);

    // Copy the state from one context to another
    fn copy_state(&mut self, ctx: &dyn CryptoHashCtx);
}

// Helper function to get ops from context
fn hash_ops(ctx: &CryptoHashContext) -> &CryptoHashOps {
    ctx.ops.as_ref().expect("CryptoHashCtx ops is None")
}

pub(crate) fn crypto_hash_free_ctx(ctx: impl CryptoHashCtx) {
    ctx.free_ctx();
}

pub(crate) fn crypto_hash_copy_state(ctx: &mut dyn CryptoHashCtx, src_ctx: &dyn CryptoHashCtx) {
    ctx.copy_state(src_ctx);
}

fn hash_md_type_from_algo(algo: u32) -> TeeResult<MdType> {
    match algo {
        TEE_ALG_MD5 => Ok(MdType::Md5),
        TEE_ALG_SHA1 => Ok(MdType::Sha1),
        TEE_ALG_SHA224 => Ok(MdType::Sha224),
        TEE_ALG_SHA256 => Ok(MdType::Sha256),
        TEE_ALG_SHA384 => Ok(MdType::Sha384),
        TEE_ALG_SHA512 => Ok(MdType::Sha512),
        TEE_ALG_SM3 => Ok(MdType::SM3),
        _ => Err(TEE_ERROR_NOT_IMPLEMENTED),
    }
}

/// OP-TEE `crypto_hash_alloc_ctx`: allocate hash context at state alloc time.
pub(crate) fn crypto_hash_alloc_ctx(algo: u32) -> TeeResult<CrypCtx> {
    let md_type = hash_md_type_from_algo(algo)?;
    Md::new(md_type)
        .map(CrypCtx::HashCtx)
        .map_err(|_| TEE_ERROR_NOT_SUPPORTED)
}

pub(crate) fn crypto_hash_init(cs: Arc<Mutex<TeeCrypState>>) -> TeeResult {
    let mut cs_guard = cs.lock();
    let md_type = hash_md_type_from_algo(cs_guard.algo)?;
    let md = Md::new(md_type).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
    cs_guard.ctx = CrypCtx::HashCtx(md);
    cs_guard.state = CrypState::Initialized;
    Ok(())
}

pub(crate) fn crypto_hash_update(cs: Arc<Mutex<TeeCrypState>>, data: &[u8]) -> TeeResult {
    let mut cs_guard = cs.lock();

    match &mut cs_guard.ctx {
        CrypCtx::HashCtx(md) => md.update(data).map_err(|_| TEE_ERROR_BAD_PARAMETERS),
        _ => Err(TEE_ERROR_BAD_PARAMETERS),
    }
}

pub(crate) fn crypto_hash_final(cs: Arc<Mutex<TeeCrypState>>, hash: &mut [u8]) -> TeeResult<usize> {
    let mut cs_guard = cs.lock();

    let ctx = core::mem::replace(&mut cs_guard.ctx, CrypCtx::Others);

    if let CrypCtx::HashCtx(md) = ctx {
        // OP-TEE: keep a clone so TEE_DigestExtract / TEE_CopyOperation can re-finalize.
        let state = md.clone();
        let len = md.finish(hash).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        cs_guard.ctx = CrypCtx::HashCtx(state);
        Ok(len)
    } else {
        Err(TEE_ERROR_BAD_PARAMETERS)
    }
}

// Driver-based hash allocation (stub implementation)
pub(crate) fn drvcrypt_hash_alloc_ctx(_algo: u32) -> TeeResult<Box<dyn CryptoHashCtx>> {
    Err(TEE_ERROR_NOT_IMPLEMENTED)
}

// Default hash algorithm allocation functions (stub implementations)
pub(crate) fn crypto_md5_alloc_ctx() -> TeeResult<Box<dyn CryptoHashCtx>> {
    Err(TEE_ERROR_NOT_IMPLEMENTED)
}

pub(crate) fn crypto_sha1_alloc_ctx() -> TeeResult<Box<dyn CryptoHashCtx>> {
    Err(TEE_ERROR_NOT_IMPLEMENTED)
}

pub(crate) fn crypto_sha224_alloc_ctx() -> TeeResult<Box<dyn CryptoHashCtx>> {
    Err(TEE_ERROR_NOT_IMPLEMENTED)
}

pub(crate) fn crypto_sha256_alloc_ctx() -> TeeResult<Box<dyn CryptoHashCtx>> {
    Err(TEE_ERROR_NOT_IMPLEMENTED)
}

pub(crate) fn crypto_sha384_alloc_ctx() -> TeeResult<Box<dyn CryptoHashCtx>> {
    Err(TEE_ERROR_NOT_IMPLEMENTED)
}

pub(crate) fn crypto_sha512_alloc_ctx() -> TeeResult<Box<dyn CryptoHashCtx>> {
    Err(TEE_ERROR_NOT_IMPLEMENTED)
}

pub(crate) fn crypto_sha3_224_alloc_ctx() -> TeeResult<Box<dyn CryptoHashCtx>> {
    Err(TEE_ERROR_NOT_IMPLEMENTED)
}

pub(crate) fn crypto_sha3_256_alloc_ctx() -> TeeResult<Box<dyn CryptoHashCtx>> {
    Err(TEE_ERROR_NOT_IMPLEMENTED)
}

pub(crate) fn crypto_sha3_384_alloc_ctx() -> TeeResult<Box<dyn CryptoHashCtx>> {
    Err(TEE_ERROR_NOT_IMPLEMENTED)
}

pub(crate) fn crypto_sha3_512_alloc_ctx() -> TeeResult<Box<dyn CryptoHashCtx>> {
    Err(TEE_ERROR_NOT_IMPLEMENTED)
}

pub(crate) fn crypto_shake128_alloc_ctx() -> TeeResult<Box<dyn CryptoHashCtx>> {
    Err(TEE_ERROR_NOT_IMPLEMENTED)
}

pub(crate) fn crypto_shake256_alloc_ctx() -> TeeResult<Box<dyn CryptoHashCtx>> {
    Err(TEE_ERROR_NOT_IMPLEMENTED)
}

pub(crate) fn crypto_sm3_alloc_ctx() -> TeeResult<Box<dyn CryptoHashCtx>> {
    Err(TEE_ERROR_NOT_IMPLEMENTED)
}

// defining mac operations for cryptographic hashing
pub(crate) trait CryptoMacCtx {
    // Initialize the hash context
    fn init(&mut self, key: &[u8]) -> TeeResult;

    // Update the hash context with data
    fn update(&mut self, data: &[u8]) -> TeeResult;

    // Finalize the hash computation and return the digest
    fn r#final(&mut self, digest: &mut [u8]) -> TeeResult;

    // Free the hash context resources
    fn free_ctx(self);

    // Copy the state from one context to another
    fn copy_state(&mut self, ctx: &dyn CryptoMacCtx);
}

fn cmac_cipher_id_from_algo(algo: u32) -> TeeResult<CipherId> {
    match algo {
        TEE_ALG_AES_CMAC => Ok(CipherId::Aes),
        TEE_ALG_DES3_CMAC => Ok(CipherId::Des3),
        TEE_ALG_SM4_CMAC => Ok(CipherId::SM4),
        _ => Err(TEE_ERROR_NOT_SUPPORTED),
    }
}

/// Default key length for `cipher_setup` at alloc (OP-TEE `crypto_cmac_alloc_ctx`).
fn cmac_default_key_bit_len(algo: u32) -> TeeResult<u32> {
    match algo {
        TEE_ALG_AES_CMAC => Ok(128),
        TEE_ALG_DES3_CMAC => Ok(192),
        TEE_ALG_SM4_CMAC => Ok(128),
        _ => Err(TEE_ERROR_NOT_SUPPORTED),
    }
}

/// OP-TEE `crypto_mac_alloc_ctx`: allocate MAC operation context without the secret key.
pub(crate) fn crypto_mac_alloc_ctx(algo: u32) -> TeeResult<CrypCtx> {
    match algo {
        TEE_ALG_HMAC_MD5 | TEE_ALG_HMAC_SHA1 | TEE_ALG_HMAC_SHA224 | TEE_ALG_HMAC_SHA256
        | TEE_ALG_HMAC_SHA384 | TEE_ALG_HMAC_SHA512 | TEE_ALG_HMAC_SM3 => {
            let md_type = match algo {
                TEE_ALG_HMAC_MD5 => MdType::Md5,
                TEE_ALG_HMAC_SHA1 => MdType::Sha1,
                TEE_ALG_HMAC_SHA224 => MdType::Sha224,
                TEE_ALG_HMAC_SHA256 => MdType::Sha256,
                TEE_ALG_HMAC_SHA384 => MdType::Sha384,
                TEE_ALG_HMAC_SHA512 => MdType::Sha512,
                TEE_ALG_HMAC_SM3 => MdType::SM3,
                _ => return Err(TEE_ERROR_NOT_SUPPORTED),
            };
            Hmac::setup(md_type)
                .map(CrypCtx::HmacCtx)
                .map_err(|_| TEE_ERROR_NOT_SUPPORTED)
        }
        TEE_ALG_AES_CMAC | TEE_ALG_DES3_CMAC | TEE_ALG_SM4_CMAC => {
            let cipher_id = cmac_cipher_id_from_algo(algo)?;
            let key_bit_len = cmac_default_key_bit_len(algo)?;
            Cipher::setup_for_cmac(cipher_id, key_bit_len)
                .map(CrypCtx::CmacCtx)
                .map_err(|_| TEE_ERROR_NOT_SUPPORTED)
        }
        _ => Err(TEE_ERROR_NOT_SUPPORTED),
    }
}

pub(crate) fn crypto_mac_init(cs: Arc<Mutex<TeeCrypState>>, key: &[u8]) -> TeeResult {
    let mut cs_guard = cs.lock();
    let algo = cs_guard.algo;
    match &mut cs_guard.ctx {
        CrypCtx::HmacCtx(hmac) => hmac.starts(key).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?,
        CrypCtx::CmacCtx(cipher) => {
            let cipher_id = cmac_cipher_id_from_algo(algo)?;
            cipher
                .cmac_init(cipher_id, key)
                .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        }
        _ => return Err(TEE_ERROR_BAD_STATE),
    }
    cs_guard.state = CrypState::Initialized;
    Ok(())
}

// Crypto MAC update
pub(crate) fn crypto_mac_update(cs: Arc<Mutex<TeeCrypState>>, data: &[u8]) -> TeeResult {
    let mut guard = cs.lock();

    match &mut guard.ctx {
        CrypCtx::HmacCtx(hmac) => hmac.update(data).map_err(|_| TEE_ERROR_BAD_PARAMETERS),
        CrypCtx::CmacCtx(cipher) => cipher
            .cmac_update(data)
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS),
        _ => Err(TEE_ERROR_BAD_PARAMETERS),
    }
}

// Crypto MAC finalization
pub(crate) fn crypto_mac_final(cs: Arc<Mutex<TeeCrypState>>, hash: &mut [u8]) -> TeeResult<usize> {
    let mut cs_guard = cs.lock();

    let ctx = core::mem::replace(&mut cs_guard.ctx, CrypCtx::Others);

    match ctx {
        CrypCtx::HmacCtx(hmac) => {
            let state = hmac.clone();
            let len = hmac.finish(hash).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
            cs_guard.ctx = CrypCtx::HmacCtx(state);
            Ok(len)
        }
        CrypCtx::CmacCtx(mut cipher) => {
            let block_size = cipher.block_size();
            cipher
                .cmac_finish(hash)
                .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
            cs_guard.ctx = CrypCtx::CmacCtx(cipher);
            Ok(block_size)
        }
        _ => Err(TEE_ERROR_BAD_PARAMETERS),
    }
}

// Crypto MAC free
pub(crate) fn crypto_mac_free(ctx: impl CryptoMacCtx) {
    // Err(TEE_ERROR_NOT_IMPLEMENTED)
    ctx.free_ctx();
}

//
pub(crate) fn crypto_mac_copy_state(ctx: &mut dyn CryptoMacCtx, src_ctx: &dyn CryptoMacCtx) {
    // Err(TEE_ERROR_NOT_IMPLEMENTED)
    ctx.copy_state(src_ctx)
}

const DES3_KEY_LEN_2KEY: usize = 16;
const DES3_KEY_LEN_3KEY: usize = 24;

/// OP-TEE `mbedtls_des3_set2key_*` / `set3key_*`: 2-key 用 DES-EDE（`cipher_info` base id 为 DES），
/// 3-key 用 DES-EDE3（base id 为 3DES）。`cipher_info_from_values(3DES, 128, …)` 选不到 2-key。
fn cipher_setup_for_key(
    cipher_id: CipherId,
    cipher_mode: CipherMode,
    key: &[u8],
) -> Result<Cipher, u32> {
    if cipher_id == CipherId::Des3 {
        match key.len() {
            DES3_KEY_LEN_2KEY => {
                Cipher::setup(CipherId::Des, cipher_mode, 128).map_err(|_| TEE_ERROR_BAD_PARAMETERS)
            }
            DES3_KEY_LEN_3KEY => Cipher::setup(CipherId::Des3, cipher_mode, 192)
                .map_err(|_| TEE_ERROR_BAD_PARAMETERS),
            _ => Err(TEE_ERROR_BAD_PARAMETERS),
        }
    } else {
        Cipher::setup(cipher_id, cipher_mode, (key.len() * 8) as u32)
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)
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

    let mut cipher_id = CipherId::None;
    let mut cipher_mode = CipherMode::None;
    let mut cipher_op = Operation::None;

    let cipher_padding = match padding_mode {
        CipherPaddingMode::None => CipherPadding::None,
        CipherPaddingMode::Pkcs7 => CipherPadding::Pkcs7,
        CipherPaddingMode::Zeros => CipherPadding::Zeros,
        CipherPaddingMode::AnsiX923 => CipherPadding::AnsiX923,
        CipherPaddingMode::IsoIec78164 => CipherPadding::IsoIec78164,
    };

    match mode {
        TEE_OperationMode::TEE_MODE_ENCRYPT => cipher_op = Operation::Encrypt,
        TEE_OperationMode::TEE_MODE_DECRYPT => cipher_op = Operation::Decrypt,
        _ => return Err(TEE_ERROR_BAD_PARAMETERS),
    }

    match algo {
        TEE_ALG_AES_ECB_NOPAD => {
            cipher_id = CipherId::Aes;
            cipher_mode = CipherMode::ECB;
        }
        TEE_ALG_AES_CBC_NOPAD => {
            cipher_id = CipherId::Aes;
            cipher_mode = CipherMode::CBC;
        }
        TEE_ALG_AES_CTR => {
            cipher_id = CipherId::Aes;
            cipher_mode = CipherMode::CTR;
        }
        TEE_ALG_AES_XTS => {
            cipher_id = CipherId::Aes;
            cipher_mode = CipherMode::XTS;
        }
        TEE_ALG_DES_ECB_NOPAD => {
            cipher_id = CipherId::Des;
            cipher_mode = CipherMode::ECB;
        }
        TEE_ALG_DES3_ECB_NOPAD => {
            cipher_id = CipherId::Des3;
            cipher_mode = CipherMode::ECB;
            if key.len() != DES3_KEY_LEN_2KEY && key.len() != DES3_KEY_LEN_3KEY {
                return Err(TEE_ERROR_BAD_PARAMETERS);
            }
        }
        TEE_ALG_DES_CBC_NOPAD => {
            cipher_id = CipherId::Des;
            cipher_mode = CipherMode::CBC;
        }
        TEE_ALG_DES3_CBC_NOPAD => {
            cipher_id = CipherId::Des3;
            cipher_mode = CipherMode::CBC;
            if key.len() != DES3_KEY_LEN_2KEY && key.len() != DES3_KEY_LEN_3KEY {
                return Err(TEE_ERROR_BAD_PARAMETERS);
            }
        }
        TEE_ALG_SM4_ECB_NOPAD => {
            cipher_id = CipherId::SM4;
            cipher_mode = CipherMode::ECB;
        }
        TEE_ALG_SM4_CBC_NOPAD => {
            cipher_id = CipherId::SM4;
            cipher_mode = CipherMode::CBC;
        }
        TEE_ALG_SM4_CTR => {
            cipher_id = CipherId::SM4;
            cipher_mode = CipherMode::CTR;
        }
        TEE_ALG_SM4_XTS => {
            cipher_id = CipherId::SM4;
            cipher_mode = CipherMode::XTS;
        }
        _ => return Err(TEE_ERROR_NOT_IMPLEMENTED),
    }

    let mut cipher = cipher_setup_for_key(cipher_id, cipher_mode, key)?;
    cipher
        .set_key(cipher_op, key)
        .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
    if let Some(iv) = iv {
        cipher.set_iv(iv).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
    }
    // Padding mode may be unsupported for some algorithms; OP-TEE ignores the return value.
    let _ = cipher.set_padding(cipher_padding);
    cipher.reset().map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;

    let xts = if cipher_uses_aes_xts_kernel(algo) {
        let decrypt = matches!(mode, TEE_OperationMode::TEE_MODE_DECRYPT);
        Some(TeeCipherXtsCtx::new(aes_xts_init(key, iv, decrypt)?))
    } else {
        None
    };

    cs_guard.state = CrypState::Initialized;
    cs_guard.ctx = CrypCtx::CipherCtx(Box::new(TeeCipherCtx {
        cipher,
        pending: [0; TeeCipherCtx::PENDING_MAX],
        pending_len: 0,
        xts,
        authenc_aad: None,
    }));
    Ok(())
}

fn tee_cipher_ctx_flush_authenc_aad(op: &mut TeeCipherCtx) -> TeeResult {
    let Some(aad) = &mut op.authenc_aad else {
        return Ok(());
    };
    aad.enter_payload_phase(&mut op.cipher)
}

fn cipher_uses_ecb_pending(algo: u32) -> bool {
    matches!(
        algo,
        TEE_ALG_AES_ECB_NOPAD
            | TEE_ALG_DES_ECB_NOPAD
            | TEE_ALG_DES3_ECB_NOPAD
            | TEE_ALG_SM4_ECB_NOPAD
    )
}

pub(crate) fn crypto_cipher_max_output_len(
    cs: Arc<Mutex<TeeCrypState>>,
    input_len: usize,
) -> TeeResult<usize> {
    let cs_guard = cs.lock();
    let CrypCtx::CipherCtx(op) = &cs_guard.ctx else {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    };
    let block_size = op.cipher.block_size();
    let block_buffered = cipher_uses_ecb_pending(cs_guard.algo) || op.xts.is_some();
    let max_out = if block_buffered {
        (op.pending_len + input_len) / block_size * block_size
    } else {
        input_len + block_size
    };
    Ok(max_out)
}

fn cipher_ecb_buffered_update(
    cipher: &mut Cipher,
    pending: &mut [u8; TeeCipherCtx::PENDING_MAX],
    pending_len: &mut usize,
    block_size: usize,
    input: &[u8],
    output: &mut [u8],
) -> TeeResult<usize> {
    if block_size == 0 || block_size > TeeCipherCtx::PENDING_MAX {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    let mut written = 0usize;
    let mut in_pos = 0usize;

    loop {
        if *pending_len > 0 {
            if in_pos >= input.len() {
                break;
            }
            let need = block_size - *pending_len;
            let take = core::cmp::min(need, input.len() - in_pos);
            pending[*pending_len..*pending_len + take]
                .copy_from_slice(&input[in_pos..in_pos + take]);
            *pending_len += take;
            in_pos += take;

            if *pending_len == block_size {
                if output.len() < written + block_size {
                    return Err(TEE_ERROR_SHORT_BUFFER);
                }
                written += cipher
                    .update(
                        &pending[..block_size],
                        &mut output[written..written + block_size],
                    )
                    .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
                *pending_len = 0;
            }
            continue;
        }

        if in_pos + block_size <= input.len() {
            if output.len() < written + block_size {
                return Err(TEE_ERROR_SHORT_BUFFER);
            }
            written += cipher
                .update(
                    &input[in_pos..in_pos + block_size],
                    &mut output[written..written + block_size],
                )
                .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
            in_pos += block_size;
        } else {
            break;
        }
    }

    let remainder = input.len() - in_pos;
    if remainder > 0 {
        pending[..remainder].copy_from_slice(&input[in_pos..]);
        *pending_len = remainder;
    }

    Ok(written)
}

pub(crate) fn crypto_cipher_update(
    cs: Arc<Mutex<TeeCrypState>>,
    input: &[u8],
    output: &mut [u8],
) -> TeeResult<usize> {
    let mut cs_guard = cs.lock();
    let algo = cs_guard.algo;
    if let CrypCtx::CipherCtx(op) = &mut cs_guard.ctx {
        if cipher_uses_authenc_aad_buffer(algo) {
            tee_cipher_ctx_flush_authenc_aad(op)?;
        }
        if let Some(xts) = &mut op.xts {
            let stream = xts.stream();
            let n = aes_xts_update_buffered(
                &mut xts.state,
                &mut op.pending,
                &mut op.pending_len,
                input,
                output,
                &stream,
            )?;
            xts.after_update(input.len(), n);
            Ok(n)
        } else if cipher_uses_ecb_pending(algo) {
            let block_size = op.cipher.block_size();
            cipher_ecb_buffered_update(
                &mut op.cipher,
                &mut op.pending,
                &mut op.pending_len,
                block_size,
                input,
                output,
            )
        } else {
            op.cipher
                .update(input, output)
                .map_err(|_| TEE_ERROR_BAD_PARAMETERS)
        }
    } else {
        Err(TEE_ERROR_BAD_PARAMETERS)
    }
}

pub(crate) fn crypto_cipher_final(
    cs: Arc<Mutex<TeeCrypState>>,
    output: &mut [u8],
) -> TeeResult<usize> {
    let mut cs_guard = cs.lock();
    let algo = cs_guard.algo;
    if let CrypCtx::CipherCtx(op) = &mut cs_guard.ctx {
        if let Some(xts) = &mut op.xts {
            let stream = xts.final_stream();
            let (n, patch) = aes_xts_final_buffered(
                &mut xts.state,
                &mut op.pending,
                &mut op.pending_len,
                &[],
                output,
                &stream,
            )?;
            if let Some(pb) = patch {
                xts.record_patch_from_final(pb, output);
            }
            xts.emitted_bytes += n;
            Ok(n)
        } else {
            if cipher_uses_ecb_pending(algo) && op.pending_len != 0 {
                return Err(TEE_ERROR_BAD_PARAMETERS);
            }
            op.cipher
                .finish(output)
                .map_err(|_| TEE_ERROR_BAD_PARAMETERS)
        }
    } else {
        Err(TEE_ERROR_BAD_PARAMETERS)
    }
}

pub(crate) fn crypto_authenc_init(
    cs: Arc<Mutex<TeeCrypState>>,
    key: &[u8],
    nonce: &[u8],
    aad_len: Option<usize>,
    tag_len: Option<usize>,
    payload_len: Option<usize>,
) -> TeeResult {
    let mut cs_guard = cs.lock();
    let algo = cs_guard.algo;
    let mode = cs_guard.mode;

    let mut cipher_id = CipherId::None;
    let mut cipher_mode = CipherMode::None;
    let mut cipher_op = Operation::None;

    match mode {
        TEE_OperationMode::TEE_MODE_ENCRYPT => cipher_op = Operation::Encrypt,
        TEE_OperationMode::TEE_MODE_DECRYPT => cipher_op = Operation::Decrypt,
        _ => return Err(TEE_ERROR_BAD_PARAMETERS),
    }

    match algo {
        TEE_ALG_AES_GCM => {
            cipher_id = CipherId::Aes;
            cipher_mode = CipherMode::GCM;
        }
        TEE_ALG_SM4_GCM => {
            cipher_id = CipherId::SM4;
            cipher_mode = CipherMode::GCM;
        }
        TEE_ALG_AES_CCM => {
            cipher_id = CipherId::Aes;
            cipher_mode = CipherMode::CCM;
        }
        TEE_ALG_SM4_CCM => {
            cipher_id = CipherId::SM4;
            cipher_mode = CipherMode::CCM;
        }
        _ => return Err(TEE_ERROR_NOT_IMPLEMENTED),
    }

    if let Ok(mut cipher) = Cipher::setup(cipher_id, cipher_mode, (key.len() * 8) as _) {
        cipher
            .set_key(cipher_op, key)
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        cipher.set_iv(nonce).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        cipher.reset().map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        if cipher_mode == CipherMode::CCM {
            let payload_len = payload_len.ok_or(TEE_ERROR_BAD_PARAMETERS)?;
            let aad_len = aad_len.ok_or(TEE_ERROR_BAD_PARAMETERS)?;
            let tag_len = tag_len.ok_or(TEE_ERROR_BAD_PARAMETERS)?;
            cipher
                .starts_ccm(payload_len, aad_len, tag_len)
                .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        }
        let authenc_aad = if cipher_uses_authenc_aad_buffer(algo) {
            Some(TeeAuthencAadCtx::new(algo, aad_len))
        } else {
            None
        };
        cs_guard.state = CrypState::Initialized;
        cs_guard.ctx = CrypCtx::CipherCtx(Box::new(TeeCipherCtx {
            cipher,
            pending: [0; TeeCipherCtx::PENDING_MAX],
            pending_len: 0,
            xts: None,
            authenc_aad,
        }));
        Ok(())
    } else {
        Err(TEE_ERROR_BAD_PARAMETERS)
    }
}

pub(crate) fn crypto_authenc_update_aad(cs: Arc<Mutex<TeeCrypState>>, aad: &[u8]) -> TeeResult {
    let mut cs_guard = cs.lock();
    if let CrypCtx::CipherCtx(op) = &mut cs_guard.ctx {
        let Some(aad_ctx) = &mut op.authenc_aad else {
            return Err(TEE_ERROR_BAD_PARAMETERS);
        };
        aad_ctx.append_aad(aad)
    } else {
        Err(TEE_ERROR_BAD_PARAMETERS)
    }
}

pub(crate) fn crypto_authenc_enc_final(
    cs: Arc<Mutex<TeeCrypState>>,
    input: Option<&[u8]>,
    output: &mut [u8],
    tag: &mut [u8],
) -> TeeResult<usize> {
    let mut cs_guard = cs.lock();
    let mut res: usize = 0;
    if let CrypCtx::CipherCtx(op) = &mut cs_guard.ctx {
        tee_cipher_ctx_flush_authenc_aad(op)?;
        if let Some(input) = input {
            res = op
                .cipher
                .update(input, output)
                .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        }
        op.cipher
            .write_tag(tag)
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        Ok(res)
    } else {
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
    let mut res: usize = 0;
    if let CrypCtx::CipherCtx(op) = &mut cs_guard.ctx {
        tee_cipher_ctx_flush_authenc_aad(op)?;
        if let Some(input) = input {
            res = op
                .cipher
                .update(input, output)
                .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        }
        op.cipher
            .check_tag(tag)
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        Ok(res)
    } else {
        Err(TEE_ERROR_BAD_PARAMETERS)
    }
}

pub(crate) fn crypto_rsa_init(
    cs: Arc<Mutex<TeeCrypState>>,
    padding_mode: RsaPadding,
    mode: TEE_OperationMode,
) -> TeeResult {
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
                if let TeeCryptObj::rsa_public_key(rsa_key) = &obj_key1_guard.attr[0] {
                    let rsa = RsaPublicComponents {
                        n: rsa_key.n.as_mpi(),
                        e: rsa_key.e.as_mpi(),
                    };
                    let mut pk = Pk::public_from_rsa_components(rsa)
                        .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
                    pk.set_options(mbedtls::pk::Options::Rsa {
                        padding: padding_mode,
                    });
                    cs_guard.ctx = CrypCtx::AsyCtx(pk);
                } else {
                    return Err(TEE_ERROR_BAD_STATE);
                }
            }
            TEE_OperationMode::TEE_MODE_DECRYPT | TEE_OperationMode::TEE_MODE_SIGN => {
                if let TeeCryptObj::rsa_keypair(rsa_key) = &obj_key1_guard.attr[0] {
                    let rsa = RsaPrivateComponents::WithPrimes {
                        p: rsa_key.p.as_mpi(),
                        q: rsa_key.q.as_mpi(),
                        e: rsa_key.e.as_mpi(),
                    };
                    let mut pk = Pk::private_from_rsa_components(rsa)
                        .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
                    pk.set_options(mbedtls::pk::Options::Rsa {
                        padding: padding_mode,
                    });
                    cs_guard.ctx = CrypCtx::AsyCtx(pk);
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
) -> TeeResult<usize> {
    let mut cs_guard = cs.lock();
    if let CrypCtx::AsyCtx(pk) = &mut cs_guard.ctx {
        let mut rng = TeeSoftwareRng::new();
        pk.encrypt_extend(input, output, &mut rng, None)
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)
    } else {
        Err(TEE_ERROR_BAD_PARAMETERS)
    }
}

pub(crate) fn crypto_acipher_rsanopad_decrypt(
    cs: Arc<Mutex<TeeCrypState>>,
    input: &[u8],
    output: &mut [u8],
) -> TeeResult<usize> {
    let mut cs_guard = cs.lock();
    if let CrypCtx::AsyCtx(pk) = &mut cs_guard.ctx {
        let mut rng = TeeSoftwareRng::new();
        pk.decrypt_extend(input, output, &mut rng, None)
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)
    } else {
        Err(TEE_ERROR_BAD_PARAMETERS)
    }
}

fn get_curve_id(curve: u32) -> TeeResult<EcGroupId> {
    match curve {
        TEE_CRYPTO_ELEMENT_NONE => Ok(EcGroupId::None),
        TEE_ECC_CURVE_NIST_P192 => Ok(EcGroupId::SecP192R1),
        TEE_ECC_CURVE_NIST_P224 => Ok(EcGroupId::SecP224R1),
        TEE_ECC_CURVE_NIST_P256 => Ok(EcGroupId::SecP256R1),
        TEE_ECC_CURVE_NIST_P384 => Ok(EcGroupId::SecP384R1),
        TEE_ECC_CURVE_NIST_P521 => Ok(EcGroupId::SecP521R1),
        TEE_ECC_CURVE_25519 => Ok(EcGroupId::Curve25519),
        TEE_ECC_CURVE_SM2 => Ok(EcGroupId::SM2P256R1),
        _ => Err(TEE_ERROR_NOT_SUPPORTED),
    }
}

pub(crate) fn crypto_ecc_init(cs: Arc<Mutex<TeeCrypState>>, pk_type: PkType) -> TeeResult {
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
                if let TeeCryptObj::ecc_public_key(ecc_key) = &obj_key1_guard.attr[0] {
                    let public_point = EcPoint::from_components(
                        ecc_key.x.clone().into_mpi(),
                        ecc_key.y.clone().into_mpi(),
                    )
                    .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
                    let curve_id = get_curve_id(ecc_key.curve)?;
                    let ec_group = EcGroup::new(curve_id).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
                    let mut pk = Pk::public_from_ec_components_extend(
                        ec_group,
                        public_point,
                        pk_type.into(),
                    )
                    .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
                    cs_guard.ctx = CrypCtx::AsyCtx(pk);
                } else {
                    return Err(TEE_ERROR_BAD_STATE);
                }
            }
            TEE_OperationMode::TEE_MODE_DECRYPT | TEE_OperationMode::TEE_MODE_SIGN => {
                if let TeeCryptObj::ecc_keypair(ecc_key) = &obj_key1_guard.attr[0] {
                    let curve_id = get_curve_id(ecc_key.curve)?;
                    let mut ec_group =
                        EcGroup::new(curve_id).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
                    let mut pk = Pk::private_from_ec_components_extend(
                        ec_group,
                        ecc_key.d.clone().into_mpi(),
                        pk_type.into(),
                    )
                    .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
                    cs_guard.ctx = CrypCtx::AsyCtx(pk);
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
    let mut cs_guard = cs.lock();
    if let CrypCtx::AsyCtx(pk) = &mut cs_guard.ctx {
        let mut rng = TeeSoftwareRng::new();
        pk.encrypt_extend(input, output, &mut rng, Some(MdType::SM3 as _))
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)
    } else {
        Err(TEE_ERROR_BAD_PARAMETERS)
    }
}

pub(crate) fn crypto_acipher_sm2_pke_decrypt(
    cs: Arc<Mutex<TeeCrypState>>,
    input: &[u8],
    output: &mut [u8],
) -> TeeResult<usize> {
    let mut cs_guard = cs.lock();
    if let CrypCtx::AsyCtx(pk) = &mut cs_guard.ctx {
        let mut rng = TeeSoftwareRng::new();
        pk.decrypt_extend(input, output, &mut rng, Some(MdType::SM3 as _))
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)
    } else {
        Err(TEE_ERROR_BAD_PARAMETERS)
    }
}

pub(crate) fn crypto_acipher_rsaes_encrypt(
    cs: Arc<Mutex<TeeCrypState>>,
    input: &[u8],
    output: &mut [u8],
    label: &[u8],
) -> TeeResult<usize> {
    let cs_guard = cs.lock();
    let algo = cs_guard.algo;
    drop(cs_guard);

    let mut cs_guard = cs.lock();
    if let CrypCtx::AsyCtx(pk) = &mut cs_guard.ctx {
        let mut rng = TeeSoftwareRng::new();

        match algo {
            TEE_ALG_RSAES_PKCS1_V1_5 => pk
                .encrypt_extend(input, output, &mut rng, None)
                .map_err(|_| TEE_ERROR_BAD_PARAMETERS),
            TEE_ALG_RSAES_PKCS1_OAEP_MGF1_MD5
            | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA1
            | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA224
            | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA256
            | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA384
            | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA512 => pk
                .encrypt_with_label(input, output, &mut rng, label)
                .map_err(|_| TEE_ERROR_BAD_PARAMETERS),
            _ => Err(TEE_ERROR_BAD_PARAMETERS),
        }
    } else {
        Err(TEE_ERROR_BAD_PARAMETERS)
    }
}

pub(crate) fn crypto_acipher_rsaes_decrypt(
    cs: Arc<Mutex<TeeCrypState>>,
    input: &[u8],
    output: &mut [u8],
    label: &[u8],
) -> TeeResult<usize> {
    let cs_guard = cs.lock();
    let algo = cs_guard.algo;
    drop(cs_guard);

    let mut cs_guard = cs.lock();
    if let CrypCtx::AsyCtx(pk) = &mut cs_guard.ctx {
        let mut rng = TeeSoftwareRng::new();

        match algo {
            TEE_ALG_RSAES_PKCS1_V1_5 => pk
                .decrypt_extend(input, output, &mut rng, None)
                .map_err(|_| TEE_ERROR_BAD_PARAMETERS),
            TEE_ALG_RSAES_PKCS1_OAEP_MGF1_MD5
            | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA1
            | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA224
            | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA256
            | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA384
            | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA512 => pk
                .decrypt_with_label(input, output, &mut rng, label)
                .map_err(|_| TEE_ERROR_BAD_PARAMETERS),
            _ => Err(TEE_ERROR_BAD_PARAMETERS),
        }
    } else {
        Err(TEE_ERROR_BAD_PARAMETERS)
    }
}

pub(crate) fn crypto_acipher_rsassa_sign(
    cs: Arc<Mutex<TeeCrypState>>,
    input: &[u8],
    output: &mut [u8],
) -> TeeResult<usize> {
    let cs_guard = cs.lock();
    let algo = cs_guard.algo;
    drop(cs_guard);

    let md_type = match algo {
        TEE_ALG_RSASSA_PKCS1_V1_5_MD5 | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_MD5 => MdType::Md5,
        TEE_ALG_RSASSA_PKCS1_V1_5_SHA1 | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA1 => MdType::Sha1,
        TEE_ALG_RSASSA_PKCS1_V1_5_SHA224 | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA224 => MdType::Sha224,
        TEE_ALG_RSASSA_PKCS1_V1_5_SHA256 | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA256 => MdType::Sha256,
        TEE_ALG_RSASSA_PKCS1_V1_5_SHA384 | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA384 => MdType::Sha384,
        TEE_ALG_RSASSA_PKCS1_V1_5_SHA512 | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA512 => MdType::Sha512,
        _ => MdType::None,
    };

    let mut cs_guard = cs.lock();

    if let CrypCtx::AsyCtx(pk) = &mut cs_guard.ctx {
        let mut rng = TeeSoftwareRng::new();
        pk.sign(md_type, input, output, &mut rng)
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)
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
    drop(cs_guard);

    let md_type = match algo {
        TEE_ALG_RSASSA_PKCS1_V1_5_MD5 | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_MD5 => MdType::Md5,
        TEE_ALG_RSASSA_PKCS1_V1_5_SHA1 | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA1 => MdType::Sha1,
        TEE_ALG_RSASSA_PKCS1_V1_5_SHA224 | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA224 => MdType::Sha224,
        TEE_ALG_RSASSA_PKCS1_V1_5_SHA256 | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA256 => MdType::Sha256,
        TEE_ALG_RSASSA_PKCS1_V1_5_SHA384 | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA384 => MdType::Sha384,
        TEE_ALG_RSASSA_PKCS1_V1_5_SHA512 | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA512 => MdType::Sha512,
        _ => MdType::None,
    };

    let mut cs_guard = cs.lock();

    if let CrypCtx::AsyCtx(pk) = &mut cs_guard.ctx {
        pk.verify(md_type, hash, signature)
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)
    } else {
        Err(TEE_ERROR_BAD_PARAMETERS)
    }
}

/// 使用ECDSA时，传入的数据是hash值
/// 使用SM2时，传入的数据是原始数据
pub(crate) fn crypto_acipher_ecc_sign(
    cs: Arc<Mutex<TeeCrypState>>,
    input: &[u8],
    output: &mut [u8],
) -> TeeResult<usize> {
    let cs_guard = cs.lock();
    let algo = cs_guard.algo;
    drop(cs_guard);

    let md_type = match algo {
        TEE_ALG_ECDSA_SHA1 => MdType::Sha1,
        TEE_ALG_ECDSA_SHA224 => MdType::Sha224,
        TEE_ALG_ECDSA_SHA256 => MdType::Sha256,
        TEE_ALG_ECDSA_SHA384 => MdType::Sha384,
        TEE_ALG_ECDSA_SHA512 => MdType::Sha512,
        TEE_ALG_SM2_DSA_SM3 => MdType::SM3,
        _ => MdType::None,
    };

    let mut cs_guard = cs.lock();

    if let CrypCtx::AsyCtx(pk) = &mut cs_guard.ctx {
        let mut rng = TeeSoftwareRng::new();
        match algo {
            TEE_ALG_SM2_DSA_SM3 => pk
                .sm2_sign(md_type, input, output, &mut rng)
                .map_err(|_| TEE_ERROR_BAD_PARAMETERS),
            _ => pk
                .sign(md_type, input, output, &mut rng)
                .map_err(|_| TEE_ERROR_BAD_PARAMETERS),
        }
    } else {
        Err(TEE_ERROR_BAD_PARAMETERS)
    }
}

/// Map high-level mbedtls verify errors to GP TEE codes.
fn map_verify_hi(hi: HiError) -> Option<u32> {
    match hi {
        HiError::EcpVerifyFailed
        | HiError::RsaVerifyFailed
        | HiError::PkSigLenMismatch
        | HiError::EcpSigLenMismatch => Some(TEE_ERROR_SIGNATURE_INVALID),
        HiError::PkBadInputData
        | HiError::EcpBadInputData
        | HiError::PkInvalidPubkey
        | HiError::PkInvalidAlg
        | HiError::PkTypeMismatch
        | HiError::PkKeyInvalidFormat
        | HiError::EcpInvalidKey => Some(TEE_ERROR_BAD_PARAMETERS),
        HiError::PkAllocFailed | HiError::EcpAllocFailed | HiError::MdAllocFailed => {
            Some(TEE_ERROR_OUT_OF_MEMORY)
        }
        HiError::Unknown(_) => None,
        _ => None,
    }
}

/// Map low-level mbedtls verify errors to GP TEE codes.
fn map_verify_lo(lo: LoError) -> Option<u32> {
    match lo {
        LoError::Asn1InvalidData
        | LoError::Asn1LengthMismatch
        | LoError::Asn1InvalidLength
        | LoError::Asn1UnexpectedTag
        | LoError::Asn1OutOfData => Some(TEE_ERROR_SIGNATURE_INVALID),
        LoError::MpiAllocFailed | LoError::Asn1AllocFailed => Some(TEE_ERROR_OUT_OF_MEMORY),
        LoError::Unknown(_) => None,
        _ => None,
    }
}

/// Map a raw mbedtls error code for verify paths (SM2 codes not in `HiError`, etc.).
fn map_verify_mbedtls_code(code: i32) -> Option<u32> {
    // `mbedtls_sm2_verify_internal` returns ERR_SM2_BAD_SIGNATURE with -1..-3 offsets.
    if (ERR_SM2_BAD_SIGNATURE - 3..=ERR_SM2_BAD_SIGNATURE).contains(&code) {
        return Some(TEE_ERROR_SIGNATURE_INVALID);
    }

    match code {
        ERR_SM2_BAD_INPUT_DATA => Some(TEE_ERROR_BAD_PARAMETERS),
        ERR_SM2_ALLOC_FAILED => Some(TEE_ERROR_OUT_OF_MEMORY),
        _ => map_verify_hi(HiError::from(code)).or_else(|| map_verify_lo(LoError::from(code))),
    }
}

/// Map high-level unknown codes (`HiError::Unknown` stores the positive high bits).
fn map_verify_hi_unknown(hi_code: i32) -> Option<u32> {
    map_verify_mbedtls_code(-hi_code)
}

/// Maps mbedtls asymmetric verify failures to GP TEE error codes.
///
/// Follows OP-TEE `convert_ltc_verify_status()` in `lib/libtomcrypt/acipher_helpers.h`.
/// SM2 uses `ERR_SM2_BAD_SIGNATURE` instead of `ERR_ECP_VERIFY_FAILED`.
fn mbedtls_verify_error_to_tee(err: MbedError) -> u32 {
    map_verify_mbedtls_code(err.to_int())
        .or_else(|| {
            err.high_level().and_then(|hi| {
                map_verify_hi(hi).or_else(|| match hi {
                    HiError::Unknown(hi_code) => map_verify_hi_unknown(hi_code),
                    _ => None,
                })
            })
        })
        .or_else(|| {
            err.low_level().and_then(|lo| {
                map_verify_lo(lo).or_else(|| match lo {
                    LoError::Unknown(lo_code) => map_verify_mbedtls_code(-lo_code),
                    _ => None,
                })
            })
        })
        .unwrap_or(TEE_ERROR_GENERIC)
}

/// 使用ECDSA时，传入的数据是hash值
/// 使用SM2时，传入的数据是原始数据
pub(crate) fn crypto_acipher_ecc_verify(
    cs: Arc<Mutex<TeeCrypState>>,
    hash: &[u8],
    signature: &[u8],
) -> TeeResult {
    let cs_guard = cs.lock();
    let algo = cs_guard.algo;
    drop(cs_guard);

    if hash.is_empty() || signature.is_empty() {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    let md_type = match algo {
        TEE_ALG_ECDSA_SHA1 => MdType::Sha1,
        TEE_ALG_ECDSA_SHA224 => MdType::Sha224,
        TEE_ALG_ECDSA_SHA256 => MdType::Sha256,
        TEE_ALG_ECDSA_SHA384 => MdType::Sha384,
        TEE_ALG_ECDSA_SHA512 => MdType::Sha512,
        TEE_ALG_SM2_DSA_SM3 => MdType::SM3,
        _ => return Err(TEE_ERROR_BAD_PARAMETERS),
    };

    let mut cs_guard = cs.lock();

    let CrypCtx::AsyCtx(pk) = &mut cs_guard.ctx else {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    };

    let result = match algo {
        TEE_ALG_SM2_DSA_SM3 => pk.sm2_verify(md_type, hash, signature),
        _ => pk.verify(md_type, hash, signature),
    };

    result.map_err(mbedtls_verify_error_to_tee)
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    alloc::{alloc, dealloc},
    boxed::Box,
    string::String,
    sync::Arc,
    vec,
    vec::Vec,
};
use core::slice::{from_raw_parts, from_raw_parts_mut};
use core::{
    alloc::Layout,
    any::Any,
    ffi::{c_char, c_uint, c_ulong, c_void},
    // from,
    mem::size_of,
    ops::{Deref, DerefMut},
    ptr::NonNull,
    slice,
    time::Duration,
};

use kerrno::{KError, KResult};
use ksync::Mutex;
use lazy_static::lazy_static;
use mbedtls::{
    cipher::raw::Cipher,
    hash::{Hmac, Md, Type as MdType},
    pk::{Pk, RsaPadding, Type as PkType},
};
use tee_raw_sys::{libc_compat::size_t, *};

use super::{
    TeeResult,
    config::CFG_COMPAT_GP10_DES,
    crypto::crypto::{
        CryptoHashCtx, CryptoMacCtx, crypto_hash_alloc_ctx, crypto_hash_final, crypto_hash_init,
        crypto_hash_update, crypto_mac_alloc_ctx, crypto_mac_final, crypto_mac_init,
        crypto_mac_update, ecc_keypair, ecc_public_key, rsa_keypair,
    },
    crypto::{sm3_hash::SM3HashCtx, sm3_hmac::SM3HmacCtx},
    libmbedtls::bignum::{
        crypto_bignum_bin2bn, crypto_bignum_bn2bin, crypto_bignum_copy, crypto_bignum_num_bits,
        crypto_bignum_num_bytes,
    },
    libutee::{
        tee_api_objects::TEE_USAGE_DEFAULT,
        utee_defines::{
            TEE_CHAIN_MODE_XTS, tee_alg_get_chain_mode, tee_alg_get_class, tee_alg_get_main_alg,
            tee_u32_to_big_endian,
        },
    },
    memtag::memtag_strip_tag_vaddr,
    tee_obj::{tee_obj, tee_obj_add, tee_obj_close, tee_obj_get, tee_obj_id_type},
    tee_pobj::with_pobj_usage_lock,
    user_access::{
        bb_alloc, bb_free, copy_from_user, copy_from_user_struct, copy_from_user_u64, copy_to_user,
        copy_to_user_struct, copy_to_user_u64,
    },
    // ts_manager:: {
    //     TsSession,
    //     ts_get_current_session, ts_get_current_session_may_fail, ts_push_current_session, ts_pop_current_session, ts_get_calling_session,
    // }
    user_access::{enter_user_access, exit_user_access},
    user_ta::{
        user_ta_ctx, // to_user_ta_ctx
    },
    utils::{bit, bit32},
    vm::vm_check_access_rights,
};
// use core::ffi::c_void;
// use core::ptr::NonNull;
use super::{
    tee_svc_cryp::{
        TeeCryptObj, copy_in_attrs, get_user_u64_as_size_t, tee_cryp_obj_secret,
        tee_cryp_obj_secret_wrapper, tee_cryp_obj_type_props, tee_obj_attr_clear,
    },
    types_ext::vaddr_t,
};
use crate::{
    mm::vm_load_string,
    tee::{
        self, TEE_ALG_DES3_CMAC, TEE_ALG_RSAES_PKCS1_OAEP_MGF1_MD5,
        TEE_ALG_RSASSA_PKCS1_PSS_MGF1_MD5, TEE_ALG_SHA3_224, TEE_ALG_SHA3_256, TEE_ALG_SHA3_384,
        TEE_ALG_SHA3_512, TEE_ALG_SHAKE128, TEE_ALG_SHAKE256, TEE_TYPE_CONCAT_KDF_Z,
        TEE_TYPE_HKDF_IKM, TEE_TYPE_PBKDF2_PASSWORD,
        crypto::{
            self,
            crypto::{
                crypto_acipher_ecc_sign, crypto_acipher_ecc_verify, crypto_acipher_rsaes_decrypt,
                crypto_acipher_rsaes_encrypt, crypto_acipher_rsanopad_decrypt,
                crypto_acipher_rsanopad_encrypt, crypto_acipher_rsassa_sign,
                crypto_acipher_rsassa_verify, crypto_acipher_sm2_pke_decrypt,
                crypto_acipher_sm2_pke_encrypt, crypto_authenc_dec_final, crypto_authenc_enc_final,
                crypto_authenc_init, crypto_authenc_update_aad, crypto_cipher_ctx_copy_from,
                crypto_cipher_final, crypto_cipher_init, crypto_cipher_max_output_len,
                crypto_cipher_update, crypto_ecc_init, crypto_rsa_init,
            },
        },
        libmbedtls::bignum::BigNum,
        memtag::{memtag_strip_tag, memtag_strip_tag_const},
        rng_software::crypto_rng_read,
        tee_session::{with_tee_session_ctx, with_tee_session_ctx_mut},
        tee_svc_cryp::{CryptoAttrRef, TeeCryptObjAttrOps, tee_crypto_ops},
        user_access::bb_memdup_user,
        utee_defines::{
            TEE_AES_BLOCK_SIZE, TEE_DES_BLOCK_SIZE, TEE_MD5_HASH_SIZE, TEE_SHA1_HASH_SIZE,
            TEE_SHA224_HASH_SIZE, TEE_SHA256_HASH_SIZE, TEE_SHA384_HASH_SIZE, TEE_SHA512_HASH_SIZE,
            TEE_SM3_HASH_SIZE,
        },
    },
};

pub const TEE_TYPE_ATTR_OPTIONAL: u32 = bit(0);
pub const TEE_TYPE_ATTR_REQUIRED: u32 = bit(1);
pub const TEE_TYPE_ATTR_OPTIONAL_GROUP: u32 = bit(2);
pub const TEE_TYPE_ATTR_SIZE_INDICATOR: u32 = bit(3);
pub const TEE_TYPE_ATTR_GEN_KEY_OPT: u32 = bit(4);
pub const TEE_TYPE_ATTR_GEN_KEY_REQ: u32 = bit(5);
pub const TEE_TYPE_ATTR_BIGNUM_MAXBITS: u32 = bit(6);

// Handle storing of generic secret keys of varying lengths
pub const ATTR_OPS_INDEX_SECRET: u32 = 0;
// Convert to/from big-endian byte array and provider-specific bignum
pub const ATTR_OPS_INDEX_BIGNUM: u32 = 1;
// Convert to/from value attribute depending on direction
// Convert to/from big-endian byte array and provider-specific bignum
pub const ATTR_OPS_INDEX_VALUE: u32 = 2;
// Convert to/from curve25519 attribute depending on direction
// Convert to/from big-endian byte array and provider-specific bignum
pub const ATTR_OPS_INDEX_25519: u32 = 3;
// Convert to/from big-endian byte array and provider-specific bignum
pub const ATTR_OPS_INDEX_448: u32 = 4;

/// Represents the state of a cryptographic operation
///
/// This enum indicates whether a cryptographic operation has been initialized or not.
///
/// # Variants
/// * `Initialized` - The cryptographic operation has been properly initialized and is ready for use
/// * `Uninitialized` - The cryptographic operation has not been initialized yet
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum CrypState {
    Initialized = 0,
    Uninitialized,
}

/// Function pointer type for finalization
///
/// This type defines the signature for functions that are responsible for cleaning up
/// or finalizing cryptographic contexts when they are no longer needed.
///
/// # Parameters
/// * `*mut c_void` - A pointer to the context that needs to be finalized
type TeeCrypCtxFinalizeFunc = unsafe extern "Rust" fn(*mut c_void);

/// Rust equivalent of the tee_cryp_state struct
///
/// This structure represents the state of a cryptographic operation in the TEE environment.
/// It contains all the necessary information to manage an active cryptographic operation,
/// including the algorithm, keys, and context-specific data.
///
/// # Fields
/// * `algo` - The cryptographic algorithm identifier (e.g., TEE_ALG_AES_ECB_NOPAD)
/// * `mode` - The operation mode (e.g., encrypt, decrypt, sign, verify)
/// * `key1` - Virtual address of the first key used in the operation (vaddr_t is typically usize in Rust)
/// * `key2` - Virtual address of the second key used in the operation (for algorithms requiring multiple keys)
/// * `ctx` - A trait object containing the specific context data for the algorithm
/// * `ctx_finalize` - Optional function pointer for finalizing the context when the operation ends
/// * `state` - Current state of the operation (initialized or uninitialized)
/// * `id` - Unique identifier for this cryptographic state instance
#[repr(C)]
pub(crate) struct TeeCrypState {
    // Since TAILQ_ENTRY is a linked list node, we use Option<NonNull> for safe pointer handling
    // pub link: Option<NonNull<TeeCrypState<'a>>>,
    pub algo: u32,
    pub mode: TEE_OperationMode,
    pub key1: Option<u32>,
    pub key2: Option<u32>,
    pub ctx: CrypCtx,
    pub ctx_finalize: Option<TeeCrypCtxFinalizeFunc>,
    pub state: CrypState,
    pub id: u32,
}

/// Cipher operation context; ECB NOPAD partial input is buffered here (OP-TEE semantics).
pub(crate) struct TeeCipherCtx {
    pub cipher: Cipher,
    pub pending: [u8; Self::PENDING_MAX],
    pub pending_len: usize,
    /// Stateful AES-XTS (mbedtls `cipher_update` does not continue tweak across calls).
    pub xts: Option<crate::tee::crypto::aes_xts::TeeCipherXtsCtx>,
    /// Buffered AAD and payload for CCM/GCM (mbedtls multipart AEAD is unreliable).
    pub authenc: Option<crate::tee::crypto::authenc::TeeAuthencCtx>,
}

impl TeeCipherCtx {
    pub(crate) const PENDING_MAX: usize = 32;
}

impl Clone for TeeCipherCtx {
    fn clone(&self) -> Self {
        Self {
            cipher: self.cipher.clone(),
            pending: self.pending,
            pending_len: self.pending_len,
            xts: self.xts.clone(),
            authenc: self.authenc.clone(),
        }
    }
}

pub(crate) enum CrypCtx {
    CipherCtx(Box<TeeCipherCtx>),
    HashCtx(Md),
    AsyCtx(Pk),
    HmacCtx(Hmac),
    CmacCtx(Cipher),
    Others,
}

impl Default for TeeCrypState {
    fn default() -> Self {
        Self {
            algo: 0,
            mode: TEE_OperationMode::TEE_MODE_DECRYPT,
            key1: None,
            key2: None,
            ctx: CrypCtx::Others,
            ctx_finalize: None,
            state: CrypState::Uninitialized,
            id: 0,
        }
    }
}

pub(crate) enum CipherPaddingMode {
    Pkcs7,
    IsoIec78164,
    AnsiX923,
    Zeros,
    None,
}

// Rust equivalent of the tee_cryp_obj_secret struct
#[repr(C)]
struct TeeCrypObjSecret {
    key_size: u32,
    alloc_size: u32,
    // The actual data would follow this struct in memory
    // In Rust, we would typically handle this differently using Vec<u8> or similar
}

// If you need to work with the data following the struct, you might want to use:
impl TeeCrypObjSecret {
    // Get a slice of the secret data
    fn data(&self) -> &[u8] {
        // This is unsafe as we're creating a slice from raw memory
        // The caller must ensure the memory is valid
        unsafe {
            from_raw_parts(
                (self as *const Self).add(1) as *const u8,
                self.alloc_size as usize,
            )
        }
    }

    // Get a mutable slice of the secret data
    fn data_mut(&mut self) -> &mut [u8] {
        // This is unsafe as we're creating a slice from raw memory
        unsafe {
            from_raw_parts_mut(
                (self as *mut Self).add(1) as *mut u8,
                self.alloc_size as usize,
            )
        }
    }
}

/// Check if algorithm is an XOF (Extendable Output Function)
///
/// XOF algorithms like SHAKE128 and SHAKE256 can produce
/// output of arbitrary length, unlike regular hash functions
/// that have fixed output size.
///
/// # Arguments
/// * `algo` - Algorithm identifier
///
/// # Returns
/// * `true` if the algorithm is an XOF (SHAKE128 or SHAKE256)
/// * `false` otherwise
#[inline]
pub fn is_xof_algo(algo: u32) -> bool {
    algo == TEE_ALG_SHAKE128 || algo == TEE_ALG_SHAKE256
}

/// Get the digest (hash) output size for the specified algorithm
///
/// # Arguments
/// * `algo` - Algorithm identifier, defined in TEE_ALG_* constants
/// * `size` - Mutable reference to store the calculated digest size
///
/// # Returns
/// * `TeeResult` - Operation result:
///   - `TEE_SUCCESS`: Successfully obtained digest size
///   - `TEE_ERROR_NOT_SUPPORTED`: Unsupported algorithm
///   - `TEE_ERROR_BAD_PARAMETERS`: Invalid parameters
///
/// # Note
/// This function only returns the standard-defined digest size for the algorithm,
/// without considering any padding or special processing/// Get digest size for algorithm
fn tee_alg_get_digest_size(algo: u32, size: &mut usize) -> TeeResult {
    match algo {
        TEE_ALG_MD5 | TEE_ALG_HMAC_MD5 => {
            *size = TEE_MD5_HASH_SIZE;
        }
        TEE_ALG_SHA1 | TEE_ALG_HMAC_SHA1 | TEE_ALG_DSA_SHA1 | TEE_ALG_ECDSA_SHA1 => {
            *size = TEE_SHA1_HASH_SIZE;
        }
        TEE_ALG_SHA224
        | TEE_ALG_SHA3_224
        | TEE_ALG_HMAC_SHA224
        | TEE_ALG_HMAC_SHA3_224
        | TEE_ALG_DSA_SHA224
        | TEE_ALG_ECDSA_SHA224 => {
            *size = TEE_SHA224_HASH_SIZE;
        }
        TEE_ALG_SHA256
        | TEE_ALG_SHA3_256
        | TEE_ALG_HMAC_SHA256
        | TEE_ALG_HMAC_SHA3_256
        | TEE_ALG_DSA_SHA256
        | TEE_ALG_ECDSA_SHA256 => {
            *size = TEE_SHA256_HASH_SIZE;
        }
        TEE_ALG_SHA384
        | TEE_ALG_SHA3_384
        | TEE_ALG_HMAC_SHA384
        | TEE_ALG_HMAC_SHA3_384
        | TEE_ALG_ECDSA_SHA384 => {
            *size = TEE_SHA384_HASH_SIZE;
        }
        TEE_ALG_SHA512
        | TEE_ALG_SHA3_512
        | TEE_ALG_HMAC_SHA512
        | TEE_ALG_HMAC_SHA3_512
        | TEE_ALG_ECDSA_SHA512 => {
            *size = TEE_SHA512_HASH_SIZE;
        }
        TEE_ALG_SM3 | TEE_ALG_HMAC_SM3 => {
            *size = TEE_SM3_HASH_SIZE;
        }
        TEE_ALG_AES_CBC_MAC_NOPAD | TEE_ALG_AES_CBC_MAC_PKCS5 | TEE_ALG_AES_CMAC => {
            *size = TEE_AES_BLOCK_SIZE;
        }
        TEE_ALG_DES_CBC_MAC_NOPAD
        | TEE_ALG_DES_CBC_MAC_PKCS5
        | TEE_ALG_DES3_CBC_MAC_NOPAD
        | TEE_ALG_DES3_CBC_MAC_PKCS5
        | TEE_ALG_DES3_CMAC => {
            *size = TEE_DES_BLOCK_SIZE;
        }
        _ => {
            return Err(TEE_ERROR_NOT_SUPPORTED);
        }
    }
    Ok(())
}

/// Safely writes a u64 value to a user-space pointer
///
/// This function performs the following operations:
/// 1. Checks if the u64 value exceeds the usize range (on 32-bit systems)
/// 2. Copies the value to user space in a secure manner
///
/// # Arguments
/// * `dst` - Target user-space pointer (usize address)
/// * `src` - Reference to source u64 value
///
/// # Returns
/// * `TeeResult` - Operation result:
///   - Returns `Ok(())` on success
///   - Returns `TEE_ERROR_OVERFLOW` on overflow
///   - Returns appropriate error code on copy failure
///
/// # Safety
/// - Caller must ensure `dst` is a valid user-space address
/// - Performs user-space memory write operations, must ensure target memory is writable
fn put_user_u64(dst: &mut usize, src: &u64) -> TeeResult {
    let mut d: u64 = 0;

    // check overflow: 32bit，usize = u32，not hold u64
    if *src > usize::MAX as u64 {
        return Err(TEE_ERROR_OVERFLOW);
    }

    // copy_to_user: set
    copy_to_user_u64(&mut d, src)?;

    *dst = d as usize;

    Ok(())
}

fn translate_compat_algo(algo: u32) -> u32 {
    match algo {
        TEE_ALG_ECDSA_P192 => TEE_ALG_ECDSA_SHA1,
        TEE_ALG_ECDSA_P224 => TEE_ALG_ECDSA_SHA224,
        TEE_ALG_ECDSA_P256 => TEE_ALG_ECDSA_SHA256,
        TEE_ALG_ECDSA_P384 => TEE_ALG_ECDSA_SHA384,
        TEE_ALG_ECDSA_P521 => TEE_ALG_ECDSA_SHA512,
        TEE_ALG_ECDH_P192 | TEE_ALG_ECDH_P224 | TEE_ALG_ECDH_P256 | TEE_ALG_ECDH_P384
        | TEE_ALG_ECDH_P521 => TEE_ALG_ECDH_DERIVE_SHARED_SECRET,
        _ => algo,
    }
}

fn tee_svc_cryp_check_key_type(o: &tee_obj, algo: u32, mode: TEE_OperationMode) -> TeeResult {
    let req_key_type;
    let mut req_key_type2: u32 = 0;
    match tee_alg_get_main_alg(algo) {
        TEE_MAIN_ALGO_MD5 => {
            req_key_type = TEE_TYPE_HMAC_MD5;
        }
        TEE_MAIN_ALGO_SHA1 => {
            req_key_type = TEE_TYPE_HMAC_SHA1;
        }
        TEE_MAIN_ALGO_SHA224 => {
            req_key_type = TEE_TYPE_HMAC_SHA224;
        }
        TEE_MAIN_ALGO_SHA256 => {
            req_key_type = TEE_TYPE_HMAC_SHA256;
        }
        TEE_MAIN_ALGO_SHA384 => {
            req_key_type = TEE_TYPE_HMAC_SHA384;
        }
        TEE_MAIN_ALGO_SHA512 => {
            req_key_type = TEE_TYPE_HMAC_SHA512;
        }
        TEE_MAIN_ALGO_SHA3_224 => {
            req_key_type = TEE_TYPE_HMAC_SHA3_224;
        }
        TEE_MAIN_ALGO_SHA3_256 => {
            req_key_type = TEE_TYPE_HMAC_SHA3_256;
        }
        TEE_MAIN_ALGO_SHA3_384 => {
            req_key_type = TEE_TYPE_HMAC_SHA3_384;
        }
        TEE_MAIN_ALGO_SHA3_512 => {
            req_key_type = TEE_TYPE_HMAC_SHA3_512;
        }
        TEE_MAIN_ALGO_SM3 => {
            req_key_type = TEE_TYPE_HMAC_SM3;
        }
        TEE_MAIN_ALGO_AES => {
            req_key_type = TEE_TYPE_AES;
        }
        TEE_MAIN_ALGO_DES => {
            req_key_type = TEE_TYPE_DES;
        }
        TEE_MAIN_ALGO_DES3 => {
            req_key_type = TEE_TYPE_DES3;
        }
        TEE_MAIN_ALGO_SM4 => {
            req_key_type = TEE_TYPE_SM4;
        }
        TEE_MAIN_ALGO_RSA => {
            req_key_type = TEE_TYPE_RSA_KEYPAIR;
            if mode == TEE_OperationMode::TEE_MODE_ENCRYPT
                || mode == TEE_OperationMode::TEE_MODE_VERIFY
            {
                req_key_type2 = TEE_TYPE_RSA_PUBLIC_KEY;
            }
        }
        TEE_MAIN_ALGO_DSA => {
            req_key_type = TEE_TYPE_DSA_KEYPAIR;
            if mode == TEE_OperationMode::TEE_MODE_ENCRYPT
                || mode == TEE_OperationMode::TEE_MODE_VERIFY
            {
                req_key_type2 = TEE_TYPE_DSA_PUBLIC_KEY;
            }
        }
        TEE_MAIN_ALGO_DH => {
            req_key_type = TEE_TYPE_DH_KEYPAIR;
        }
        TEE_MAIN_ALGO_ECDSA => {
            req_key_type = TEE_TYPE_ECDSA_KEYPAIR;
            if mode == TEE_OperationMode::TEE_MODE_VERIFY {
                req_key_type2 = TEE_TYPE_ECDSA_PUBLIC_KEY;
            }
        }
        TEE_MAIN_ALGO_ECDH => {
            req_key_type = TEE_TYPE_ECDH_KEYPAIR;
        }
        TEE_MAIN_ALGO_ED25519 => {
            req_key_type = TEE_TYPE_ED25519_KEYPAIR;
            if mode == TEE_OperationMode::TEE_MODE_VERIFY {
                req_key_type2 = TEE_TYPE_ED25519_PUBLIC_KEY;
            }
        }
        TEE_MAIN_ALGO_SM2_PKE => {
            if mode == TEE_OperationMode::TEE_MODE_ENCRYPT {
                req_key_type = TEE_TYPE_SM2_PKE_PUBLIC_KEY;
            } else {
                req_key_type = TEE_TYPE_SM2_PKE_KEYPAIR;
            }
        }
        TEE_MAIN_ALGO_SM2_DSA_SM3 => {
            if mode == TEE_OperationMode::TEE_MODE_VERIFY {
                req_key_type = TEE_TYPE_SM2_DSA_PUBLIC_KEY;
            } else {
                req_key_type = TEE_TYPE_SM2_DSA_KEYPAIR;
            }
        }
        TEE_MAIN_ALGO_SM2_KEP => {
            req_key_type = TEE_TYPE_SM2_KEP_KEYPAIR;
            req_key_type2 = TEE_TYPE_SM2_KEP_PUBLIC_KEY;
        }
        TEE_MAIN_ALGO_HKDF => {
            req_key_type = TEE_TYPE_HKDF_IKM;
        }
        TEE_MAIN_ALGO_CONCAT_KDF => {
            req_key_type = TEE_TYPE_CONCAT_KDF_Z;
        }
        TEE_MAIN_ALGO_PBKDF2 => {
            req_key_type = TEE_TYPE_PBKDF2_PASSWORD;
        }
        TEE_MAIN_ALGO_X25519 => {
            req_key_type = TEE_TYPE_X25519_KEYPAIR;
        }
        TEE_MAIN_ALGO_X448 => {
            req_key_type = TEE_TYPE_X448_KEYPAIR;
        }
        _ => return Err(TEE_ERROR_BAD_PARAMETERS),
    }

    if req_key_type != o.info.objectType && req_key_type2 != o.info.objectType {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    Ok(())
}

fn get_rsaes_padding_mode(algo: u32) -> RsaPadding {
    match algo {
        TEE_ALG_RSAES_PKCS1_V1_5 => RsaPadding::Pkcs1V15,
        TEE_ALG_RSAES_PKCS1_OAEP_MGF1_MD5 => RsaPadding::Pkcs1V21 { mgf: MdType::Md5 },
        TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA1 => RsaPadding::Pkcs1V21 { mgf: MdType::Sha1 },
        TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA224 => RsaPadding::Pkcs1V21 {
            mgf: MdType::Sha224,
        },
        TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA256 => RsaPadding::Pkcs1V21 {
            mgf: MdType::Sha256,
        },
        TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA384 => RsaPadding::Pkcs1V21 {
            mgf: MdType::Sha384,
        },
        TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA512 => RsaPadding::Pkcs1V21 {
            mgf: MdType::Sha512,
        },
        _ => RsaPadding::None,
    }
}

fn get_rsassa_padding_mode(algo: u32) -> RsaPadding {
    match algo {
        TEE_ALG_RSASSA_PKCS1_V1_5_MD5
        | TEE_ALG_RSASSA_PKCS1_V1_5_SHA1
        | TEE_ALG_RSASSA_PKCS1_V1_5_SHA224
        | TEE_ALG_RSASSA_PKCS1_V1_5_SHA256
        | TEE_ALG_RSASSA_PKCS1_V1_5_SHA384
        | TEE_ALG_RSASSA_PKCS1_V1_5_SHA512 => RsaPadding::Pkcs1V15,
        TEE_ALG_RSASSA_PKCS1_PSS_MGF1_MD5 => RsaPadding::Pkcs1V21 { mgf: MdType::Md5 },
        TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA1 => RsaPadding::Pkcs1V21 { mgf: MdType::Sha1 },
        TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA224 => RsaPadding::Pkcs1V21 {
            mgf: MdType::Sha224,
        },
        TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA256 => RsaPadding::Pkcs1V21 {
            mgf: MdType::Sha256,
        },
        TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA384 => RsaPadding::Pkcs1V21 {
            mgf: MdType::Sha384,
        },
        TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA512 => RsaPadding::Pkcs1V21 {
            mgf: MdType::Sha512,
        },
        _ => RsaPadding::None,
    }
}

fn get_dsa_type(algo: u32) -> PkType {
    match algo {
        TEE_ALG_ECDSA_SHA1 | TEE_ALG_ECDSA_SHA224 | TEE_ALG_ECDSA_SHA256 | TEE_ALG_ECDSA_SHA384
        | TEE_ALG_ECDSA_SHA512 => PkType::Eckey,
        TEE_ALG_SM2_DSA_SM3 => PkType::SM2,
        _ => PkType::None,
    }
}

fn tee_cryp_asymm_init(
    cs: Arc<Mutex<TeeCrypState>>,
    algo: u32,
    mode: TEE_OperationMode,
) -> TeeResult {
    match algo {
        TEE_ALG_RSA_NOPAD => match mode {
            TEE_OperationMode::TEE_MODE_ENCRYPT | TEE_OperationMode::TEE_MODE_DECRYPT => {
                crypto_rsa_init(cs.clone(), RsaPadding::None, mode)
            }
            _ => Err(TEE_ERROR_GENERIC),
        },
        TEE_ALG_SM2_PKE => match mode {
            TEE_OperationMode::TEE_MODE_ENCRYPT | TEE_OperationMode::TEE_MODE_DECRYPT => {
                crypto_ecc_init(cs.clone(), PkType::SM2)
            }
            _ => Err(TEE_ERROR_GENERIC),
        },
        TEE_ALG_RSAES_PKCS1_V1_5
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_MD5
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA1
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA224
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA256
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA384
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA512 => match mode {
            TEE_OperationMode::TEE_MODE_ENCRYPT | TEE_OperationMode::TEE_MODE_DECRYPT => {
                crypto_rsa_init(cs.clone(), get_rsaes_padding_mode(algo), mode)
            }
            _ => Err(TEE_ERROR_GENERIC),
        },
        TEE_ALG_RSASSA_PKCS1_V1_5_MD5
        | TEE_ALG_RSASSA_PKCS1_V1_5_SHA1
        | TEE_ALG_RSASSA_PKCS1_V1_5_SHA224
        | TEE_ALG_RSASSA_PKCS1_V1_5_SHA256
        | TEE_ALG_RSASSA_PKCS1_V1_5_SHA384
        | TEE_ALG_RSASSA_PKCS1_V1_5_SHA512
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_MD5
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA1
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA224
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA256
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA384
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA512 => match mode {
            TEE_OperationMode::TEE_MODE_SIGN | TEE_OperationMode::TEE_MODE_VERIFY => {
                crypto_rsa_init(cs.clone(), get_rsassa_padding_mode(algo), mode)
            }
            _ => Err(TEE_ERROR_GENERIC),
        },
        TEE_ALG_ECDSA_SHA1 | TEE_ALG_ECDSA_SHA224 | TEE_ALG_ECDSA_SHA256 | TEE_ALG_ECDSA_SHA384
        | TEE_ALG_ECDSA_SHA512 | TEE_ALG_SM2_DSA_SM3 => {
            crypto_ecc_init(cs.clone(), get_dsa_type(algo))
        }
        _ => Ok(()),
    }
}

// 创建一个TeeCrypState
pub fn tee_cryp_state_alloc(
    algo: u32,
    mode: TEE_OperationMode,
    key1: Option<u32>,
    key2: Option<u32>,
    state: &mut u32,
) -> TeeResult {
    let algo = translate_compat_algo(algo);
    let mut cs = TeeCrypState::default();
    let mut cs_id: u32 = 0;
    let mut res: TeeResult = Ok(());

    // 判断密钥对象是否存在，并取出密钥对象
    let mut o1_ok = false;
    let mut o2_ok = false;
    if let Some(key1) = key1
        && let Ok(obj1_arc) = tee_obj_get(key1 as tee_obj_id_type)
    {
        o1_ok = true;
        let mut o1 = obj1_arc.lock();
        if o1.busy {
            return Err(TEE_ERROR_BUSY);
        }
        o1.busy = true;
        cs.key1 = Some(o1.info.objectId);
        tee_svc_cryp_check_key_type(&o1, algo, mode)?;
    }

    if let Some(key2) = key2
        && let Ok(obj2_arc) = tee_obj_get(key2 as tee_obj_id_type)
    {
        o2_ok = true;
        let mut o2 = obj2_arc.lock();
        if o2.busy {
            return Err(TEE_ERROR_BUSY);
        }
        o2.busy = true;
        cs.key2 = Some(o2.info.objectId);
        tee_svc_cryp_check_key_type(&o2, algo, mode)?;
    }

    // 判断密钥是否符合算法要求
    match tee_alg_get_class(algo) {
        TEE_OPERATION_CIPHER => {
            if (tee_alg_get_chain_mode(algo) == TEE_CHAIN_MODE_XTS && (!o1_ok || !o2_ok))
                || (tee_alg_get_chain_mode(algo) != TEE_CHAIN_MODE_XTS && (!o1_ok || o2_ok))
            {
                return Err(TEE_ERROR_BAD_PARAMETERS);
            }
        }
        TEE_OPERATION_AE => {
            if !o1_ok || o2_ok {
                return Err(TEE_ERROR_BAD_PARAMETERS);
            }
        }
        TEE_OPERATION_MAC => {
            if !o1_ok || o2_ok {
                return Err(TEE_ERROR_BAD_PARAMETERS);
            }
        }
        TEE_OPERATION_DIGEST => {
            if o1_ok || o2_ok {
                return Err(TEE_ERROR_BAD_PARAMETERS);
            }
        }
        TEE_OPERATION_ASYMMETRIC_CIPHER | TEE_OPERATION_ASYMMETRIC_SIGNATURE => {
            if !o1_ok || o2_ok {
                return Err(TEE_ERROR_BAD_PARAMETERS);
            }
        }
        TEE_OPERATION_KEY_DERIVATION => {
            if algo == TEE_ALG_SM2_KEP {
                if !o1_ok || !o2_ok {
                    return Err(TEE_ERROR_BAD_PARAMETERS);
                }
            } else if !o1_ok || o2_ok {
                return Err(TEE_ERROR_BAD_PARAMETERS);
            }
        }
        _ => {
            return Err(TEE_ERROR_NOT_SUPPORTED);
        }
    }

    // OP-TEE: crypto_mac_alloc_ctx / crypto_hash_alloc_ctx at state alloc time.
    cs.ctx = match tee_alg_get_class(algo) {
        TEE_OPERATION_MAC => crypto_mac_alloc_ctx(algo)?,
        TEE_OPERATION_DIGEST => crypto_hash_alloc_ctx(algo)?,
        _ => CrypCtx::Others,
    };

    with_tee_session_ctx_mut(|ctx| {
        let vacant = ctx.cryp_state.vacant_entry();
        let id = vacant.key();
        let cs_id = id as u32;
        cs.id = cs_id;
        cs.algo = algo;
        cs.mode = mode;
        *state = cs_id;

        // 插入TeeCrypState
        let arc_cs = Arc::new(Mutex::new(cs));
        let _ = vacant.insert(arc_cs);
        Ok(())
    })
}

pub fn syscall_cryp_state_alloc(
    arg0: usize,
    arg1: usize,
    arg2: usize,
    arg3: usize,
    arg4: usize,
) -> TeeResult {
    let mut state_ptr = arg4 as *mut u32;
    let mut state = 0u32;
    unsafe { copy_from_user_struct(&mut state, &*state_ptr)? };
    let mode = match arg1 {
        0 => TEE_OperationMode::TEE_MODE_ENCRYPT,
        1 => TEE_OperationMode::TEE_MODE_DECRYPT,
        2 => TEE_OperationMode::TEE_MODE_SIGN,
        3 => TEE_OperationMode::TEE_MODE_VERIFY,
        4 => TEE_OperationMode::TEE_MODE_MAC,
        5 => TEE_OperationMode::TEE_MODE_DIGEST,
        6 => TEE_OperationMode::TEE_MODE_DERIVE,
        _ => return Err(TEE_ERROR_BAD_PARAMETERS),
    };
    let key1 = if arg2 == 0 { None } else { Some(arg2 as u32) };
    let key2 = if arg3 == 0 { None } else { Some(arg3 as u32) };

    tee_cryp_state_alloc(arg0 as _, mode, key1, key2, &mut state)?;
    unsafe { copy_to_user_struct(&mut *state_ptr, &state)? };
    Ok(())
}

// 复制一个TeeCrypState
pub fn tee_cryp_state_copy(dst_id: u32, src_id: u32) -> TeeResult {
    let cs_dst = tee_cryp_state_get(dst_id)?;
    let cs_src = tee_cryp_state_get(src_id)?;

    if dst_id == src_id {
        return Ok(());
    }

    let mut dst_guard = cs_dst.lock();
    let src_guard = cs_src.lock();

    if dst_guard.algo != src_guard.algo || dst_guard.mode != src_guard.mode {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    match tee_alg_get_class(src_guard.algo) {
        TEE_OPERATION_CIPHER | TEE_OPERATION_AE => {
            let CrypCtx::CipherCtx(src_cipher) = &src_guard.ctx else {
                return Err(TEE_ERROR_BAD_STATE);
            };
            let copied = crypto_cipher_ctx_copy_from(&src_guard, src_cipher)?;
            match &mut dst_guard.ctx {
                CrypCtx::CipherCtx(dst_cipher) => **dst_cipher = copied,
                _ => dst_guard.ctx = CrypCtx::CipherCtx(Box::new(copied)),
            }
        }
        TEE_OPERATION_DIGEST => {
            let CrypCtx::HashCtx(src_md) = &src_guard.ctx else {
                return Err(TEE_ERROR_BAD_STATE);
            };
            let CrypCtx::HashCtx(dst_md) = &mut dst_guard.ctx else {
                return Err(TEE_ERROR_BAD_STATE);
            };
            // OP-TEE: crypto_hash_copy_state() → mbedtls_md_clone / copy hash_state
            *dst_md = src_md.clone();
        }
        TEE_OPERATION_MAC => match &src_guard.ctx {
            CrypCtx::HmacCtx(src_hmac) => {
                let CrypCtx::HmacCtx(dst_hmac) = &mut dst_guard.ctx else {
                    return Err(TEE_ERROR_BAD_STATE);
                };
                *dst_hmac = src_hmac.clone();
            }
            CrypCtx::CmacCtx(src_cipher) => {
                let CrypCtx::CmacCtx(dst_cipher) = &mut dst_guard.ctx else {
                    return Err(TEE_ERROR_BAD_STATE);
                };
                // OP-TEE mbed_cmac_copy_state: mbedtls_cipher_clone in place.
                dst_cipher
                    .copy_state_from(src_cipher)
                    .map_err(|_| TEE_ERROR_BAD_STATE)?;
            }
            _ => return Err(TEE_ERROR_BAD_STATE),
        },
        _ => return Err(TEE_ERROR_BAD_STATE),
    }
    dst_guard.state = src_guard.state;
    dst_guard.ctx_finalize = src_guard.ctx_finalize;

    Ok(())
}

pub fn syscall_cryp_state_copy(arg0: usize, arg1: usize) -> TeeResult {
    tee_cryp_state_copy(arg0 as _, arg1 as _)
}

// 删除一个TeeCrypState：先丢弃 mbedTLS 运算上下文（内含轮密钥等副本），再移除状态节点，最后清零并关闭密钥对象
pub fn tee_cryp_state_free(id: u32) -> TeeResult {
    let cs = tee_cryp_state_get(id)?;
    let (key1, key2) = {
        let mut g = cs.lock();
        let key1 = g.key1;
        let key2 = g.key2;
        // `crypto_cipher_init` 等会把密钥物化进 `Cipher`/`Hmac`/`Pk`；仅对 tee_obj 做 attr_clear 无法清理这些副本
        let _ = core::mem::replace(&mut g.ctx, CrypCtx::Others);
        g.ctx_finalize = None;
        (key1, key2)
    };

    cryp_state_free(id)?;

    if let Some(key1) = key1 {
        let o = tee_obj_get(key1 as _)?;
        let mut o_guard = o.lock();
        o_guard.busy = false;
        tee_obj_attr_clear(&mut o_guard)?;
        drop(o_guard);
        tee_obj_close(key1)?;
    }

    if let Some(key2) = key2 {
        let o = tee_obj_get(key2 as _)?;
        let mut o_guard = o.lock();
        o_guard.busy = false;
        tee_obj_attr_clear(&mut o_guard)?;
        drop(o_guard);
        tee_obj_close(key2)?;
    }

    Ok(())
}

pub fn syscall_cryp_state_free(arg0: usize) -> TeeResult {
    tee_cryp_state_free(arg0 as _)
}

// 根据id获取一个TeeCrypState
pub fn tee_cryp_state_get(id: u32) -> TeeResult<Arc<Mutex<TeeCrypState>>> {
    with_tee_session_ctx(|ctx| match ctx.cryp_state.get(id as _) {
        Some(cs) => Ok(Arc::clone(cs)),
        None => Err(TEE_ERROR_ITEM_NOT_FOUND),
    })
}

// 根据id删除一个TeeCrypState
fn cryp_state_free(id: u32) -> TeeResult {
    with_tee_session_ctx_mut(|ctx| {
        if let Some(cs) = ctx.cryp_state.try_remove(id as usize) {
            tee_debug!("Remove cryp state {}", id);
            Ok(())
        } else {
            tee_debug!("Remove cryp state failed");
            Err(TEE_ERROR_BAD_STATE)
        }
    })?;
    Ok(())
}

pub fn tee_cryp_hash_init(id: u32) -> TeeResult {
    let mut cs = tee_cryp_state_get(id)?;
    let cs_guard = cs.lock();
    let algo = cs_guard.algo;
    let key1 = cs_guard.key1;
    drop(cs_guard);

    match tee_alg_get_class(algo) {
        TEE_OPERATION_DIGEST => crypto_hash_init(cs.clone()),
        TEE_OPERATION_MAC => {
            let key1 = key1.ok_or(TEE_ERROR_BAD_PARAMETERS)?;
            let o = tee_obj_get(key1 as tee_obj_id_type)?;
            let mut o_guard = o.lock();
            if o_guard.attr.is_empty() {
                return Err(TEE_ERROR_BAD_STATE);
            }

            // 从tee_obj中读取密钥
            if let TeeCryptObj::obj_secret(k) = &o_guard.attr[0] {
                let mut key = k.key();
                crypto_mac_init(cs.clone(), key)
            } else {
                Err(TEE_ERROR_BAD_STATE)
            }
        }
        _ => Err(TEE_ERROR_BAD_PARAMETERS),
    }
}

pub fn syscall_hash_init(arg0: usize) -> TeeResult {
    tee_cryp_hash_init(arg0 as _)
}

pub fn tee_cryp_hash_update(id: u32, chunk: &[u8]) -> TeeResult {
    memtag_strip_tag_const()?;
    vm_check_access_rights(0, 0, 0)?;

    let mut cs = tee_cryp_state_get(id)?;
    let cs_guard = cs.lock();
    let algo = cs_guard.algo;

    if cs_guard.state != CrypState::Initialized {
        return Err(TEE_ERROR_BAD_STATE);
    }
    drop(cs_guard);

    match tee_alg_get_class(algo) {
        TEE_OPERATION_DIGEST => crypto_hash_update(cs.clone(), chunk),
        TEE_OPERATION_MAC => crypto_mac_update(cs.clone(), chunk),
        _ => Err(TEE_ERROR_BAD_PARAMETERS),
    }
}

pub fn syscall_hash_update(arg0: usize, arg1: usize, arg2: usize) -> TeeResult {
    let chunk_ptr = arg1 as *const u8;
    let chunk_len = arg2;

    let chunk_slice: &[u8] = if chunk_ptr.is_null() || chunk_len == 0 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    } else {
        unsafe { core::slice::from_raw_parts(chunk_ptr, chunk_len) }
    };
    let chunk = bb_memdup_user(chunk_slice)?;

    tee_cryp_hash_update(arg0 as _, &chunk)
}

pub fn tee_cryp_hash_final(id: u32, chunk: &[u8], hash: &mut [u8]) -> TeeResult<usize> {
    memtag_strip_tag_const()?;
    memtag_strip_tag()?;
    vm_check_access_rights(0, 0, 0)?;

    let mut cs = tee_cryp_state_get(id)?;
    let cs_guard = cs.lock();
    let algo = cs_guard.algo;

    if cs_guard.state != CrypState::Initialized {
        return Err(TEE_ERROR_BAD_STATE);
    }
    drop(cs_guard);

    let mut hash_size = 0;
    match tee_alg_get_class(algo) {
        TEE_OPERATION_DIGEST => {
            tee_alg_get_digest_size(algo, &mut hash_size)?;
            if hash.len() < hash_size {
                return Err(TEE_ERROR_SHORT_BUFFER);
            }

            if !chunk.is_empty() {
                crypto_hash_update(cs.clone(), chunk)?;
            }
            crypto_hash_final(cs.clone(), hash)
        }
        TEE_OPERATION_MAC => {
            tee_alg_get_digest_size(algo, &mut hash_size)?;
            if hash.len() < hash_size {
                return Err(TEE_ERROR_SHORT_BUFFER);
            }

            if !chunk.is_empty() {
                crypto_mac_update(cs.clone(), chunk)?;
            }
            crypto_mac_final(cs.clone(), hash)
        }
        _ => Err(TEE_ERROR_BAD_PARAMETERS),
    }
}

pub fn syscall_hash_final(
    arg0: usize,
    arg1: usize,
    arg2: usize,
    arg3: usize,
    arg4: usize,
) -> TeeResult {
    let chunk_ptr = arg1 as *const u8;
    let chunk_len = arg2;

    // 输入的hash_len长度应该为缓冲区长度，最后函数返回值为实际长度
    let hash_ptr = arg3 as *mut u8;
    let mut hash_len_ptr = arg4 as *mut usize;
    let mut hash_len: usize = 0;
    unsafe { copy_from_user_struct(&mut hash_len, &*hash_len_ptr)? };

    let chunk: Box<[u8]> = if chunk_ptr.is_null() || chunk_len == 0 {
        Box::new([])
    } else {
        bb_memdup_user(unsafe { core::slice::from_raw_parts(chunk_ptr, chunk_len) })?
    };

    if hash_ptr.is_null() || hash_len == 0 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    let hash_slice = unsafe { core::slice::from_raw_parts_mut(hash_ptr, hash_len) };
    let mut hash = bb_memdup_user(hash_slice)?;

    hash_len = tee_cryp_hash_final(arg0 as _, &chunk, &mut hash)?;

    // Copy hash to user
    unsafe { copy_to_user_struct(&mut *hash_len_ptr, &hash_len)? };
    unsafe { copy_to_user(hash_slice, &hash, hash_len * size_of::<u8>())? };
    Ok(())
}

/// optee中只支持NoPad，此处实现了Padding模式的拓展
/// 实际使用中，若要保持ALG类型一致，请使用CipherPaddingMode::None作为参数
pub fn tee_cryp_cipher_init(
    id: u32,
    iv: Option<&[u8]>,
    padding_mode: CipherPaddingMode,
) -> TeeResult {
    let mut cs = tee_cryp_state_get(id)?;
    let cs_guard = cs.lock();
    let algo = cs_guard.algo;
    let key1 = cs_guard.key1;
    let key2 = cs_guard.key2;

    // 当key1和key2都有效时，将key1和key2密钥拼接
    // 在XTS模式下key1和key2都有效
    let mut key: Vec<u8> = Vec::new();

    // 获取key1密钥
    if let Some(k) = key1 {
        let obj_key1 = tee_obj_get(k as _)?;
        let obj_key1_guard = obj_key1.lock();

        if obj_key1_guard.attr.is_empty() {
            return Err(TEE_ERROR_BAD_STATE);
        }

        // 从tee_obj中读取密钥
        if let TeeCryptObj::obj_secret(k) = &obj_key1_guard.attr[0] {
            key.extend_from_slice(k.key());
        } else {
            return Err(TEE_ERROR_BAD_STATE);
        }
    } else {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    };

    // 如果key2存在，则获取key2密钥
    if let Some(k) = key2 {
        let obj_key2 = tee_obj_get(k as _)?;
        let obj_key2_guard = obj_key2.lock();
        if obj_key2_guard.attr.is_empty() {
            return Err(TEE_ERROR_BAD_STATE);
        }

        // 从tee_obj中读取密钥
        if let TeeCryptObj::obj_secret(k) = &obj_key2_guard.attr[0] {
            key.extend_from_slice(k.key());
        } else {
            return Err(TEE_ERROR_BAD_STATE);
        }
    }

    drop(cs_guard);
    crypto_cipher_init(cs.clone(), key.as_slice(), iv, padding_mode)
}

pub fn syscall_cipher_init(arg0: usize, arg1: usize, arg2: usize) -> TeeResult {
    let iv_ptr = arg1 as *const u8;
    let iv_len = arg2;

    // 转换IV
    let iv: Option<Box<[u8]>> = if iv_ptr.is_null() || iv_len == 0 {
        None
    } else {
        let iv_slice = unsafe { core::slice::from_raw_parts(iv_ptr, iv_len) };
        let iv_option = bb_memdup_user(iv_slice)?;
        Some(iv_option)
    };

    match iv {
        Some(iv) => tee_cryp_cipher_init(arg0 as _, Some(&iv), CipherPaddingMode::None),
        None => tee_cryp_cipher_init(arg0 as _, None, CipherPaddingMode::None),
    }
}

/// 不足一块的输入会缓存在 `TeeCipherCtx::pending`（OP-TEE 语义）；
/// `output` 须能容纳本次可能写出的密文字节数（可能为 0）。
pub fn tee_cryp_cipher_update(id: u32, input: &[u8], output: &mut [u8]) -> TeeResult<usize> {
    memtag_strip_tag_const()?;
    memtag_strip_tag()?;
    vm_check_access_rights(0, 0, 0)?;

    let mut cs = tee_cryp_state_get(id)?;
    let cs_guard = cs.lock();

    if cs_guard.state != CrypState::Initialized {
        return Err(TEE_ERROR_BAD_STATE);
    }

    drop(cs_guard);

    let max_out = crypto_cipher_max_output_len(cs.clone(), input.len())?;
    if output.len() < max_out {
        return Err(TEE_ERROR_SHORT_BUFFER);
    }

    crypto_cipher_update(cs.clone(), input, output)
}

pub fn syscall_cipher_update(
    arg0: usize,
    arg1: usize,
    arg2: usize,
    arg3: usize,
    arg4: usize,
) -> TeeResult {
    let src_ptr = arg1 as *const u8;
    let src_len = arg2;

    // 输入的dst_len长度应该为缓冲区长度，最后函数返回值为实际长度
    let dst_ptr = arg3 as *mut u8;

    let mut dst_len_ptr = arg4 as *mut usize;
    let mut dst_len: usize = 0;
    unsafe { copy_from_user_struct(&mut dst_len, &*dst_len_ptr)? };

    let src = if src_ptr.is_null() || src_len == 0 {
        Box::new([])
    } else {
        let src_slice = unsafe { core::slice::from_raw_parts(src_ptr, src_len) };
        bb_memdup_user(src_slice)?
    };

    if dst_ptr.is_null() || dst_len == 0 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    let dst_slice = unsafe { core::slice::from_raw_parts_mut(dst_ptr, dst_len) };
    let mut dst = bb_memdup_user(dst_slice)?;

    let id = arg0 as u32;
    {
        let cs = tee_cryp_state_get(id)?;
        let mut guard = cs.lock();
        if let CrypCtx::CipherCtx(op) = &mut guard.ctx
            && let Some(xts) = &mut op.xts
        {
            xts.record_user_base_if_unset(dst_ptr as usize);
        }
    }

    dst_len = tee_cryp_cipher_update(id, &src, &mut dst)?;

    // Copy dst to user
    unsafe { copy_to_user_struct(&mut *dst_len_ptr, &dst_len)? };
    unsafe { copy_to_user(dst_slice, &dst, dst_len * size_of::<u8>())? };
    Ok(())
}

/// 用于处理最后一个数据块的填充和加密
pub fn tee_cryp_cipher_final(id: u32, input: &[u8], output: &mut [u8]) -> TeeResult<usize> {
    memtag_strip_tag_const()?;
    memtag_strip_tag()?;
    vm_check_access_rights(0, 0, 0)?;

    let mut cs = tee_cryp_state_get(id)?;
    let cs_guard = cs.lock();

    if cs_guard.state != CrypState::Initialized {
        return Err(TEE_ERROR_BAD_STATE);
    }

    drop(cs_guard);

    {
        let cs = tee_cryp_state_get(id)?;
        let mut guard = cs.lock();
        if let CrypCtx::CipherCtx(op) = &mut guard.ctx
            && let Some(xts) = &mut op.xts
        {
            xts.in_final_syscall = true;
        }
    }

    // 本次 syscall 的 output 仅覆盖当前 memref，始终从 output[0] 写入。
    // TA 全局偏移 `xts.emitted_bytes` 只用于计算 patch 的虚拟地址，不能用作 output 下标。
    let mut len = 0;
    if !input.is_empty() {
        len = tee_cryp_cipher_update(id, input, output)?;
    }

    let tail = crypto_cipher_final(cs.clone(), &mut output[len..])?;

    {
        let cs = tee_cryp_state_get(id)?;
        let mut guard = cs.lock();
        if let CrypCtx::CipherCtx(op) = &mut guard.ctx
            && let Some(xts) = &mut op.xts
        {
            xts.in_final_syscall = false;
        }
    }

    Ok(len + tail)
}

pub fn syscall_cipher_final(
    arg0: usize,
    arg1: usize,
    arg2: usize,
    arg3: usize,
    arg4: usize,
) -> TeeResult {
    let src_ptr = arg1 as *const u8;
    let src_len = arg2;

    // 输入的dst_len长度应该为缓冲区长度，最后函数返回值为实际长度
    let dst_ptr = arg3 as *mut u8;
    let mut dst_len_ptr = arg4 as *mut usize;
    let mut dst_len: usize = 0;
    unsafe { copy_from_user_struct(&mut dst_len, &*dst_len_ptr)? };

    let src = if src_ptr.is_null() || src_len == 0 {
        Box::new([])
    } else {
        let src_slice = unsafe { core::slice::from_raw_parts(src_ptr, src_len) };
        bb_memdup_user(src_slice)?
    };

    if dst_ptr.is_null() || dst_len == 0 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    let dst_slice = unsafe { core::slice::from_raw_parts_mut(dst_ptr, dst_len) };
    let mut dst = bb_memdup_user(dst_slice)?;

    let id = arg0 as u32;
    {
        let cs = tee_cryp_state_get(id)?;
        let mut guard = cs.lock();
        if let CrypCtx::CipherCtx(op) = &mut guard.ctx
            && let Some(xts) = &mut op.xts
        {
            xts.record_user_base_if_unset(dst_ptr as usize);
        }
    }

    dst_len = tee_cryp_cipher_final(id, &src, &mut dst)?;

    xts_merge_patch_into_dst(id, dst_ptr as usize, &mut dst)?;

    // Copy dst to user
    unsafe { copy_to_user_struct(&mut *dst_len_ptr, &dst_len)? };
    unsafe { copy_to_user(dst_slice, &dst, dst_len * size_of::<u8>())? };

    xts_apply_patch_to_user(id, dst_ptr as usize, dst_len)?;

    Ok(())
}

/// If the patched block lies in this syscall's memref, update the kernel copy before `copy_to_user`.
fn xts_merge_patch_into_dst(id: u32, dst_ptr: usize, dst: &mut [u8]) -> TeeResult {
    let cs = tee_cryp_state_get(id)?;
    let mut guard = cs.lock();
    let CrypCtx::CipherCtx(op) = &mut guard.ctx else {
        return Ok(());
    };
    if let Some(xts) = &op.xts {
        xts.merge_patch_into_slice(dst_ptr, dst);
    }
    Ok(())
}

/// Patch the previous full XTS block in TA memory when it is outside this syscall's memref.
fn xts_apply_patch_to_user(id: u32, dst_ptr: usize, dst_len: usize) -> TeeResult {
    let cs = tee_cryp_state_get(id)?;
    let mut guard = cs.lock();
    let CrypCtx::CipherCtx(op) = &mut guard.ctx else {
        return Ok(());
    };
    let Some(xts) = &mut op.xts else {
        return Ok(());
    };
    let Some(patch) = xts.patch_block.take() else {
        return Ok(());
    };
    let Some(patch_va) = xts.patch_user_off else {
        return Ok(());
    };
    if patch_va >= dst_ptr && patch_va + patch.len() <= dst_ptr + dst_len {
        return Ok(());
    }
    let patch_slice = unsafe { core::slice::from_raw_parts_mut(patch_va as *mut u8, patch.len()) };
    unsafe { copy_to_user(patch_slice, &patch, patch.len() * size_of::<u8>())? };
    Ok(())
}

pub fn syscall_cryp_random_number_generate(arg0: usize, arg1: usize) -> TeeResult {
    let buf_ptr = arg0 as *mut u8;
    let buf_len = arg1;

    if buf_ptr.is_null() || buf_len == 0 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    let buf_slice = unsafe { core::slice::from_raw_parts_mut(buf_ptr, buf_len) };
    let mut buf = bb_memdup_user(buf_slice)?;
    crypto_rng_read(&mut buf)?;

    unsafe { copy_to_user(buf_slice, &buf, buf_len * size_of::<u8>())? };
    Ok(())
}

pub fn tee_cryp_authenc_init(
    id: u32,
    nonce: &[u8],
    aad_len: Option<usize>,
    tag_len: Option<usize>,
    payload_len: Option<usize>,
) -> TeeResult {
    let mut cs = tee_cryp_state_get(id)?;
    let cs_guard = cs.lock();
    let algo = cs_guard.algo;
    let key1 = cs_guard.key1;

    let mut key: Vec<u8> = Vec::new();

    // 获取key1密钥
    if let Some(k) = key1 {
        let obj_key1 = tee_obj_get(k as _)?;
        let obj_key1_guard = obj_key1.lock();

        if obj_key1_guard.attr.is_empty() {
            return Err(TEE_ERROR_BAD_STATE);
        }

        // 从tee_obj中读取密钥
        if let TeeCryptObj::obj_secret(k) = &obj_key1_guard.attr[0] {
            key.extend_from_slice(k.key());
        } else {
            return Err(TEE_ERROR_BAD_STATE);
        }
    } else {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    };

    drop(cs_guard);
    crypto_authenc_init(
        cs.clone(),
        key.as_slice(),
        nonce,
        aad_len,
        tag_len,
        payload_len,
    )
}

pub fn syscall_authenc_init(
    arg0: usize,
    arg1: usize,
    arg2: usize,
    arg3: usize,
    arg4: usize,
    arg5: usize,
) -> TeeResult {
    let nonce_ptr = arg1 as *const u8;
    let nonce_len = arg2;

    let nonce_slice = unsafe { core::slice::from_raw_parts(nonce_ptr, nonce_len) };
    let nonce = bb_memdup_user(nonce_slice)?;

    let aad_len = if arg4 == 0 { None } else { Some(arg4) };
    let tag_len = if arg3 == 0 { None } else { Some(arg3) };
    let payload_len = if arg5 == 0 { None } else { Some(arg5) };

    tee_cryp_authenc_init(arg0 as _, &nonce, aad_len, tag_len, payload_len)
}

pub fn tee_cryp_authenc_update_aad(id: u32, aad: &[u8]) -> TeeResult {
    memtag_strip_tag()?;
    vm_check_access_rights(0, 0, 0)?;

    let mut cs = tee_cryp_state_get(id)?;
    let cs_guard = cs.lock();
    let algo = cs_guard.algo;

    if cs_guard.state != CrypState::Initialized {
        return Err(TEE_ERROR_BAD_STATE);
    }
    if tee_alg_get_class(algo) != TEE_OPERATION_AE {
        return Err(TEE_ERROR_BAD_STATE);
    }

    drop(cs_guard);
    crypto_authenc_update_aad(cs.clone(), aad)
}

pub fn syscall_authenc_update_aad(arg0: usize, arg1: usize, arg2: usize) -> TeeResult {
    let aad_ptr = arg1 as *const u8;
    let aad_len = arg2;

    let aad_slice = unsafe { core::slice::from_raw_parts(aad_ptr, aad_len) };
    let aad = bb_memdup_user(aad_slice)?;

    tee_cryp_authenc_update_aad(arg0 as _, &aad)
}

pub fn tee_cryp_authenc_update_payload(
    id: u32,
    input: &[u8],
    output: &mut [u8],
) -> TeeResult<usize> {
    tee_cryp_cipher_update(id, input, output)
}

pub fn syscall_authenc_update_payload(
    arg0: usize,
    arg1: usize,
    arg2: usize,
    arg3: usize,
    arg4: usize,
) -> TeeResult {
    syscall_cipher_update(arg0, arg1, arg2, arg3, arg4)
}

pub fn tee_cryp_authenc_enc_final(
    id: u32,
    input: Option<&[u8]>,
    output: &mut [u8],
    tag: &mut [u8],
) -> TeeResult<usize> {
    memtag_strip_tag_const()?;
    memtag_strip_tag()?;
    vm_check_access_rights(0, 0, 0)?;

    let mut cs = tee_cryp_state_get(id)?;
    let cs_guard = cs.lock();

    if cs_guard.state != CrypState::Initialized {
        return Err(TEE_ERROR_BAD_STATE);
    }

    drop(cs_guard);
    crypto_authenc_enc_final(cs.clone(), input, output, tag)
}

pub fn syscall_authenc_enc_final(
    arg0: usize,
    arg1: usize,
    arg2: usize,
    arg3: usize,
    arg4: usize,
    arg5: usize,
    arg6: usize,
) -> TeeResult {
    let src_ptr = arg1 as *const u8;
    let src_len = arg2;

    // 输入的dst_len长度应该为缓冲区长度，最后函数返回值为实际长度
    let dst_ptr = arg3 as *mut u8;
    let mut dst_len_ptr = arg4 as *mut usize;
    let mut dst_len: usize = 0;
    unsafe { copy_from_user_struct(&mut dst_len, &*dst_len_ptr)? };

    let src = if src_ptr.is_null() || src_len == 0 {
        None
    } else {
        let src_slice = unsafe { core::slice::from_raw_parts(src_ptr, src_len) };
        let src_option = bb_memdup_user(src_slice)?;
        Some(src_option)
    };

    if dst_ptr.is_null() || dst_len == 0 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    let dst_slice = unsafe { core::slice::from_raw_parts_mut(dst_ptr, dst_len) };
    let mut dst = bb_memdup_user(dst_slice)?;

    let tag_ptr = arg5 as *mut u8;
    let mut tag_len_ptr = arg6 as *mut usize;
    let mut tag_len: usize = 0;
    unsafe { copy_from_user_struct(&mut tag_len, &*tag_len_ptr)? };

    if tag_ptr.is_null() || tag_len == 0 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    let tag_slice = unsafe { core::slice::from_raw_parts_mut(tag_ptr, tag_len) };
    let mut tag = bb_memdup_user(tag_slice)?;

    match src {
        Some(src) => {
            dst_len = tee_cryp_authenc_enc_final(arg0 as _, Some(&src), &mut dst, &mut tag)?;
        }
        None => {
            dst_len = tee_cryp_authenc_enc_final(arg0 as _, None, &mut dst, &mut tag)?;
        }
    };

    // Copy to user
    unsafe { copy_to_user_struct(&mut *dst_len_ptr, &dst_len)? };
    unsafe { copy_to_user(dst_slice, &dst, dst_len * size_of::<u8>())? };
    unsafe { copy_to_user(tag_slice, &tag, tag_len * size_of::<u8>())? };
    Ok(())
}

pub fn tee_cryp_authenc_dec_final(
    id: u32,
    input: Option<&[u8]>,
    output: &mut [u8],
    tag: &[u8],
) -> TeeResult<usize> {
    memtag_strip_tag_const()?;
    memtag_strip_tag()?;
    vm_check_access_rights(0, 0, 0)?;

    let mut cs = tee_cryp_state_get(id)?;
    let cs_guard = cs.lock();

    if cs_guard.state != CrypState::Initialized {
        return Err(TEE_ERROR_BAD_STATE);
    }

    drop(cs_guard);
    crypto_authenc_dec_final(cs.clone(), input, output, tag)
}

pub fn syscall_authenc_dec_final(
    arg0: usize,
    arg1: usize,
    arg2: usize,
    arg3: usize,
    arg4: usize,
    arg5: usize,
    arg6: usize,
) -> TeeResult {
    let src_ptr = arg1 as *const u8;
    let src_len = arg2;

    // 输入的dst_len长度应该为缓冲区长度，最后函数返回值为实际长度
    let dst_ptr = arg3 as *mut u8;
    let mut dst_len_ptr = arg4 as *mut usize;
    let mut dst_len: usize = 0;
    unsafe { copy_from_user_struct(&mut dst_len, &*dst_len_ptr)? };

    let src = if src_ptr.is_null() || src_len == 0 {
        None
    } else {
        let src_slice = unsafe { core::slice::from_raw_parts(src_ptr, src_len) };
        let src_option = bb_memdup_user(src_slice)?;
        Some(src_option)
    };

    if dst_ptr.is_null() || dst_len == 0 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    let dst_slice = unsafe { core::slice::from_raw_parts_mut(dst_ptr, dst_len) };
    let mut dst = bb_memdup_user(dst_slice)?;

    let tag_ptr = arg5 as *const u8;
    let mut tag_len = arg6;

    if tag_ptr.is_null() || tag_len == 0 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    let tag_slice = unsafe { core::slice::from_raw_parts(tag_ptr, tag_len) };
    let tag = bb_memdup_user(tag_slice)?;

    match src {
        Some(src) => {
            dst_len = tee_cryp_authenc_dec_final(arg0 as _, Some(&src), &mut dst, &tag)?;
        }
        None => {
            dst_len = tee_cryp_authenc_dec_final(arg0 as _, None, &mut dst, &tag)?;
        }
    };

    // Copy to user
    unsafe { copy_to_user_struct(&mut *dst_len_ptr, &dst_len)? };
    unsafe { copy_to_user(dst_slice, &dst, dst_len * size_of::<u8>())? };
    Ok(())
}

pub fn tee_cryp_asymm_operate(
    id: u32,
    input: &[u8],
    output: &mut [u8],
    label: Option<&[u8]>,
) -> TeeResult<usize> {
    let mut required = 0usize;
    tee_cryp_asymm_operate_with_required(id, input, output, label, None, &mut required)
}

pub fn tee_cryp_asymm_operate_with_required(
    id: u32,
    input: &[u8],
    output: &mut [u8],
    label: Option<&[u8]>,
    mgf_algo: Option<u32>,
    required: &mut usize,
) -> TeeResult<usize> {
    *required = 0;
    memtag_strip_tag_const()?;
    memtag_strip_tag()?;
    vm_check_access_rights(0, 0, 0)?;

    let mut cs = tee_cryp_state_get(id)?;
    let cs_guard = cs.lock();
    let algo = cs_guard.algo;
    let mode = cs_guard.mode;
    let cryp_state = cs_guard.state;
    let label = match label {
        Some(label) => label,
        None => &[],
    };
    drop(cs_guard);

    if cryp_state != CrypState::Initialized {
        tee_cryp_asymm_init(cs.clone(), algo, mode)?;
        let mut cs_guard = cs.lock();
        cs_guard.state = CrypState::Initialized;
        drop(cs_guard);
    }

    match algo {
        TEE_ALG_RSA_NOPAD => match mode {
            TEE_OperationMode::TEE_MODE_ENCRYPT => {
                crypto_acipher_rsanopad_encrypt(cs.clone(), input, output, required)
            }
            TEE_OperationMode::TEE_MODE_DECRYPT => {
                crypto_acipher_rsanopad_decrypt(cs.clone(), input, output, required)
            }
            _ => Err(TEE_ERROR_GENERIC),
        },
        TEE_ALG_SM2_PKE => match mode {
            TEE_OperationMode::TEE_MODE_ENCRYPT => {
                crypto_acipher_sm2_pke_encrypt(cs.clone(), input, output)
            }
            TEE_OperationMode::TEE_MODE_DECRYPT => {
                crypto_acipher_sm2_pke_decrypt(cs.clone(), input, output)
            }
            _ => Err(TEE_ERROR_GENERIC),
        },
        TEE_ALG_RSAES_PKCS1_V1_5
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_MD5
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA1
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA224
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA256
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA384
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA512 => match mode {
            TEE_OperationMode::TEE_MODE_ENCRYPT => {
                crypto_acipher_rsaes_encrypt(cs.clone(), input, output, label, mgf_algo, required)
            }
            TEE_OperationMode::TEE_MODE_DECRYPT => {
                crypto_acipher_rsaes_decrypt(cs.clone(), input, output, label, mgf_algo, required)
            }
            _ => Err(TEE_ERROR_GENERIC),
        },
        TEE_ALG_RSASSA_PKCS1_V1_5_MD5
        | TEE_ALG_RSASSA_PKCS1_V1_5_SHA1
        | TEE_ALG_RSASSA_PKCS1_V1_5_SHA224
        | TEE_ALG_RSASSA_PKCS1_V1_5_SHA256
        | TEE_ALG_RSASSA_PKCS1_V1_5_SHA384
        | TEE_ALG_RSASSA_PKCS1_V1_5_SHA512
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_MD5
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA1
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA224
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA256
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA384
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA512 => {
            crypto_acipher_rsassa_sign(cs.clone(), input, output, required)
        }
        TEE_ALG_DSA_SHA1 | TEE_ALG_DSA_SHA224 | TEE_ALG_DSA_SHA256 => Err(TEE_ERROR_NOT_SUPPORTED), /* mbedtls no support for DSA */
        TEE_ALG_ED25519 => Err(TEE_ERROR_NOT_SUPPORTED), // mbedtls no support for EdDSA
        TEE_ALG_ECDSA_SHA1 | TEE_ALG_ECDSA_SHA224 | TEE_ALG_ECDSA_SHA256 | TEE_ALG_ECDSA_SHA384
        | TEE_ALG_ECDSA_SHA512 | TEE_ALG_SM2_DSA_SM3 => {
            crypto_acipher_ecc_sign(cs.clone(), input, output, required)
        }
        _ => Err(TEE_ERROR_NOT_SUPPORTED),
    }
}

/// 与 OP-TEE `syscall_asymm_operate` 一致：`arg1` 为 `utee_attribute` 数组指针，`arg2` 为元素个数。
/// 用于 RSA-OAEP 的 `TEE_ATTR_RSA_OAEP_LABEL` 等 memref 参数（从用户态拷入内核后再参与运算）。
pub fn syscall_asymm_operate(
    arg0: usize,
    arg1: usize,
    arg2: usize,
    arg3: usize,
    arg4: usize,
    arg5: usize,
    arg6: usize,
) -> TeeResult {
    let usr_params_ptr = arg1 as *const utee_attribute;
    let num_params = arg2;

    if num_params != 0 && usr_params_ptr.is_null() {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    let params_attrs: Option<Box<[TEE_Attribute]>> = if num_params == 0 {
        None
    } else {
        let attr_null = TEE_Attribute::default();
        let mut attrs = vec![attr_null; num_params].into_boxed_slice();
        let usr_attrs_slice = unsafe { core::slice::from_raw_parts(usr_params_ptr, num_params) };
        copy_in_attrs(&mut user_ta_ctx::default(), usr_attrs_slice, &mut attrs)?;
        Some(attrs)
    };

    let mut label_buf: Option<Box<[u8]>> = None;
    let mut mgf_algo: Option<u32> = None;
    if let Some(ref attrs) = params_attrs {
        for attr in attrs.iter() {
            match attr.attributeID {
                TEE_ATTR_RSA_OAEP_LABEL => {
                    if attr.attributeID & TEE_ATTR_FLAG_VALUE != 0 {
                        return Err(TEE_ERROR_BAD_PARAMETERS);
                    }
                    let (buf, len) = unsafe {
                        (
                            attr.content.memref.buffer as *const u8,
                            attr.content.memref.size,
                        )
                    };
                    if len != 0 && buf.is_null() {
                        return Err(TEE_ERROR_BAD_PARAMETERS);
                    }
                    let usr_label = unsafe { core::slice::from_raw_parts(buf, len) };
                    label_buf = Some(bb_memdup_user(usr_label)?);
                }
                TEE_ATTR_RSA_OAEP_MGF_HASH => {
                    let (buf, len) = unsafe {
                        (
                            attr.content.memref.buffer as *const u8,
                            attr.content.memref.size,
                        )
                    };
                    if len != core::mem::size_of::<u32>() || buf.is_null() {
                        return Err(TEE_ERROR_BAD_PARAMETERS);
                    }
                    let mut val = 0u32;
                    unsafe { copy_from_user_struct(&mut val, &*(buf as *const u32))? };
                    mgf_algo = Some(val);
                }
                _ => {}
            }
        }
    }

    let src_ptr = arg3 as *const u8;
    let src_len = arg4;

    // 输入的dst_len长度应该为缓冲区长度，最后函数返回值为实际长度
    let dst_ptr = arg5 as *mut u8;
    let mut dst_len_ptr = arg6 as *mut usize;
    let mut dst_len: usize = 0;
    unsafe { copy_from_user_struct(&mut dst_len, &*dst_len_ptr)? };

    let src = if src_ptr.is_null() || src_len == 0 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    } else {
        let src_slice = unsafe { core::slice::from_raw_parts(src_ptr, src_len) };
        bb_memdup_user(src_slice)?
    };

    if dst_ptr.is_null() && dst_len != 0 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    if dst_len != 0 && dst_ptr.is_null() {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    let mut dst_buf: Box<[u8]>;
    let dst_user_slice = if dst_len == 0 {
        dst_buf = Box::new([]);
        None
    } else {
        let dst_slice = unsafe { core::slice::from_raw_parts_mut(dst_ptr, dst_len) };
        dst_buf = bb_memdup_user(dst_slice)?;
        Some(dst_slice)
    };

    let mut required = 0usize;
    let label_ref = label_buf.as_deref();
    match tee_cryp_asymm_operate_with_required(
        arg0 as _,
        &src,
        &mut dst_buf,
        label_ref,
        mgf_algo,
        &mut required,
    ) {
        Ok(actual) => {
            dst_len = actual;
            unsafe { copy_to_user_struct(&mut *dst_len_ptr, &dst_len)? };
            if let Some(dst_slice) = dst_user_slice {
                unsafe { copy_to_user(dst_slice, &dst_buf, dst_len * size_of::<u8>())? };
            }
            Ok(())
        }
        Err(TEE_ERROR_SHORT_BUFFER) => {
            if required == 0 {
                return Err(TEE_ERROR_SHORT_BUFFER);
            }
            unsafe { copy_to_user_struct(&mut *dst_len_ptr, &required)? };
            Err(TEE_ERROR_SHORT_BUFFER)
        }
        Err(e) => Err(e),
    }
}

pub fn tee_cryp_asymm_verify(id: u32, hash: &[u8], signature: &[u8]) -> TeeResult {
    memtag_strip_tag()?;
    vm_check_access_rights(0, 0, 0)?;

    let mut cs = tee_cryp_state_get(id)?;
    let cs_guard = cs.lock();
    let algo = cs_guard.algo;
    let mode = cs_guard.mode;
    let cryp_state = cs_guard.state;
    drop(cs_guard);

    if cryp_state != CrypState::Initialized {
        tee_cryp_asymm_init(cs.clone(), algo, mode)?;
        let mut cs_guard = cs.lock();
        cs_guard.state = CrypState::Initialized;
        drop(cs_guard);
    }

    if mode != TEE_OperationMode::TEE_MODE_VERIFY {
        return Err(TEE_ERROR_BAD_STATE);
    }

    match algo {
        TEE_ALG_RSASSA_PKCS1_V1_5_MD5
        | TEE_ALG_RSASSA_PKCS1_V1_5_SHA1
        | TEE_ALG_RSASSA_PKCS1_V1_5_SHA224
        | TEE_ALG_RSASSA_PKCS1_V1_5_SHA256
        | TEE_ALG_RSASSA_PKCS1_V1_5_SHA384
        | TEE_ALG_RSASSA_PKCS1_V1_5_SHA512
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_MD5
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA1
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA224
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA256
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA384
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA512 => {
            crypto_acipher_rsassa_verify(cs.clone(), hash, signature)
        }
        TEE_ALG_DSA_SHA1 | TEE_ALG_DSA_SHA224 | TEE_ALG_DSA_SHA256 => Err(TEE_ERROR_NOT_SUPPORTED), /* mbedtls no support for DSA */
        TEE_ALG_ED25519 => Err(TEE_ERROR_NOT_SUPPORTED), // mbedtls no support for EdDSA
        TEE_ALG_ECDSA_SHA1 | TEE_ALG_ECDSA_SHA224 | TEE_ALG_ECDSA_SHA256 | TEE_ALG_ECDSA_SHA384
        | TEE_ALG_ECDSA_SHA512 | TEE_ALG_SM2_DSA_SM3 => {
            crypto_acipher_ecc_verify(cs.clone(), hash, signature)
        }
        _ => Err(TEE_ERROR_NOT_SUPPORTED),
    }
}

/// arg1与arg2参数与RSASSA有关
/// 暂未进行处理，目前只支持ECC和SM2
pub fn syscall_asymm_verify(
    arg0: usize,
    _arg1: usize,
    _arg2: usize,
    arg3: usize,
    arg4: usize,
    arg5: usize,
    arg6: usize,
) -> TeeResult {
    let data_ptr = arg3 as *const u8;
    let data_len = arg4;

    let sig_ptr = arg5 as *mut u8;
    let mut sig_len = arg6;

    let data = if data_ptr.is_null() || data_len == 0 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    } else {
        let data_slice = unsafe { core::slice::from_raw_parts(data_ptr, data_len) };
        bb_memdup_user(data_slice)?
    };

    if sig_ptr.is_null() || sig_len == 0 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    let sig_slice = unsafe { core::slice::from_raw_parts_mut(sig_ptr, sig_len) };
    let sig = bb_memdup_user(sig_slice)?;
    tee_cryp_asymm_verify(arg0 as _, &data, &sig)?;
    Ok(())
}

#[unittest::mod_test]
pub mod tests_cryp {
    use mbedtls::{bignum::Mpi, pk::Pk};
    use unittest::{TestResult, assert, assert_eq, assert_ne};

    use super::*;
    use crate::{
        TestUserValue,
        tee::{
            libmbedtls::bignum::BigNum,
            tee_svc_cryp::{
                syscall_cryp_obj_alloc, syscall_cryp_obj_close, syscall_cryp_obj_copy,
                syscall_cryp_obj_populate, syscall_obj_generate_key, tee_init_ref_attribute,
                tee_obj_set_type,
            },
        },
    };

    #[unittest::def_test(custom)]
    fn test_cryp_state() {
        let mut state1: u32 = 0;
        let mut state2: u32 = 0;
        // 须为合法 AES 对象：`tee_cryp_state_free` 会对关联 key 调用 `tee_obj_attr_clear`，
        // 默认 `tee_obj` 无类型/无 attr 会导致 `TEE_ERROR_BAD_STATE`。
        let mut test_obj = tee_obj::default();
        assert!(tee_obj_set_type(&mut test_obj, TEE_TYPE_AES, 256).is_ok());

        let res = tee_obj_add(test_obj);
        assert!(res.is_ok());
        let id = res.unwrap() as u32;

        let res = tee_cryp_state_alloc(
            TEE_ALG_SM3,
            TEE_OperationMode::TEE_MODE_DIGEST,
            None,
            None,
            &mut state1,
        );
        assert!(res.is_ok());

        let res = tee_cryp_state_alloc(
            TEE_ALG_AES_ECB_NOPAD,
            TEE_OperationMode::TEE_MODE_DECRYPT,
            Some(id),
            None,
            &mut state2,
        );
        assert!(res.is_ok());

        let res = tee_cryp_state_get(state1);
        assert!(res.is_ok());
        let cs1 = res.unwrap();

        let guard1 = cs1.lock();
        assert_eq!(guard1.id, state1);
        assert_eq!(guard1.algo, TEE_ALG_SM3);
        assert!(guard1.mode == TEE_OperationMode::TEE_MODE_DIGEST);
        drop(guard1);

        let res = tee_cryp_state_get(state2);
        assert!(res.is_ok());
        let cs2 = res.unwrap();

        let guard2 = cs2.lock();
        assert_eq!(guard2.id, state2);
        assert_eq!(guard2.algo, TEE_ALG_AES_ECB_NOPAD);
        assert!(guard2.mode == TEE_OperationMode::TEE_MODE_DECRYPT);
        drop(guard2);

        let res = tee_cryp_state_free(state1);
        assert!(res.is_ok());

        let res = tee_cryp_state_free(state2);
        assert!(res.is_ok());

        match tee_cryp_state_get(state1) {
            Err(e) => assert_eq!(e, TEE_ERROR_ITEM_NOT_FOUND),
            Ok(_) => panic!("Expected error, but got Ok"),
        }
        match tee_cryp_state_get(state2) {
            Err(e) => assert_eq!(e, TEE_ERROR_ITEM_NOT_FOUND),
            Ok(_) => panic!("Expected error, but got Ok"),
        }
    }

    /// `tee_cryp_state_free` 会 `tee_obj_attr_clear` 并 `tee_obj_close`；此处额外 `Arc::clone` 住内核对象，
    /// 以便在会话表已删除句柄后仍能检查密钥尾随区是否已被写 0。
    #[unittest::def_test(custom)]
    fn test_cryp_state_free_zeroes_key_material() {
        const KEY_LEN: usize = 16;
        let pattern = [0x5Cu8; KEY_LEN];

        let mut obj = tee_obj::default();
        assert!(tee_obj_set_type(&mut obj, TEE_TYPE_AES, 256).is_ok());
        if let TeeCryptObj::obj_secret(w) = &mut obj.attr[0] {
            assert!(w.set_secret_data(&pattern).is_ok());
        } else {
            panic!("expected obj_secret");
        }

        let kid = tee_obj_add(obj).expect("tee_obj_add") as u32;
        let kept = tee_obj_get(kid as tee_obj_id_type).expect("tee_obj_get");

        {
            let g = kept.lock();
            if let TeeCryptObj::obj_secret(w) = &g.attr[0] {
                assert_eq!(w.secret().key_size as usize, KEY_LEN);
                assert_eq!(w.key(), pattern.as_slice());
            } else {
                panic!("expected obj_secret");
            }
        }

        let mut state: u32 = 0;
        assert!(
            tee_cryp_state_alloc(
                TEE_ALG_AES_ECB_NOPAD,
                TEE_OperationMode::TEE_MODE_DECRYPT,
                Some(kid),
                None,
                &mut state,
            )
            .is_ok()
        );

        assert!(tee_cryp_state_free(state).is_ok());

        assert_eq!(
            tee_obj_get(kid as tee_obj_id_type).err(),
            Some(TEE_ERROR_ITEM_NOT_FOUND)
        );

        let g = kept.lock();
        if let TeeCryptObj::obj_secret(w) = &g.attr[0] {
            assert_eq!(w.secret().key_size, 0, "key_size should be cleared");
            assert!(
                w.data().iter().all(|&b| b == 0),
                "secret allocation should be zeroed after cryp_state_free"
            );
        } else {
            panic!("expected obj_secret");
        }
    }

    #[unittest::def_test]
    fn test_translate_compat_algo_maps_legacy_values() {
        assert_eq!(
            translate_compat_algo(TEE_ALG_ECDSA_P192),
            TEE_ALG_ECDSA_SHA1
        );
        assert_eq!(
            translate_compat_algo(TEE_ALG_ECDSA_P256),
            TEE_ALG_ECDSA_SHA256
        );
        assert_eq!(
            translate_compat_algo(TEE_ALG_ECDH_P384),
            TEE_ALG_ECDH_DERIVE_SHARED_SECRET
        );
        assert_eq!(translate_compat_algo(TEE_ALG_SM3), TEE_ALG_SM3);
    }

    #[unittest::def_test(custom)]
    fn test_cryp_legacy_ecdsa_p256_sign_verify_with_keypair() {
        let mut usr_params = crate::user_vec![utee_attribute::default(); 1];
        usr_params[0].attribute_id = TEE_ATTR_ECC_CURVE;
        usr_params[0].a = TEE_ECC_CURVE_NIST_P256 as u64;
        usr_params[0].b = 0;

        let mut obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        syscall_cryp_obj_alloc(TEE_TYPE_ECDSA_KEYPAIR as _, 256, obj_id.as_user_ref()).unwrap();
        let obj_id = obj_id.read();
        syscall_obj_generate_key(obj_id as c_ulong, 256, usr_params.as_ptr(), 1).unwrap();

        let mut obj_id2 = TestUserValue::<c_uint>::from_value(0).unwrap();
        syscall_cryp_obj_alloc(TEE_TYPE_ECDSA_KEYPAIR as _, 256, obj_id2.as_user_ref()).unwrap();
        let obj_id2 = obj_id2.read();
        syscall_cryp_obj_copy(obj_id2 as _, obj_id as _).unwrap();

        let mut st_sign: u32 = 0;
        tee_cryp_state_alloc(
            TEE_ALG_ECDSA_P256,
            TEE_OperationMode::TEE_MODE_SIGN,
            Some(obj_id),
            None,
            &mut st_sign,
        )
        .unwrap();
        let cs = tee_cryp_state_get(st_sign).unwrap();
        assert_eq!(cs.lock().algo, TEE_ALG_ECDSA_SHA256);

        let mut st_verify: u32 = 0;
        tee_cryp_state_alloc(
            TEE_ALG_ECDSA_P256,
            TEE_OperationMode::TEE_MODE_VERIFY,
            Some(obj_id2),
            None,
            &mut st_verify,
        )
        .unwrap();

        let digest = [0x42u8; 32];
        let mut sig = [0u8; 256];
        let sig_len = tee_cryp_asymm_operate(st_sign, &digest, &mut sig, None).unwrap();
        tee_cryp_asymm_verify(st_verify, &digest, &sig[..sig_len]).unwrap();
    }

    #[unittest::def_test(custom)]
    fn test_syscall_cryp_generate_key_ecc_p256_keypair() {
        // ECDSA P-256：必须在 generate_key 时通过 TEE_ATTR_ECC_CURVE 指定曲线
        let mut usr_params = crate::user_vec![utee_attribute::default(); 1];
        usr_params[0].attribute_id = TEE_ATTR_ECC_CURVE;
        usr_params[0].a = TEE_ECC_CURVE_NIST_P256 as u64;
        usr_params[0].b = 0;

        let mut obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        let result = syscall_cryp_obj_alloc(TEE_TYPE_ECDSA_KEYPAIR as _, 256, obj_id.as_user_ref());
        assert!(result.is_ok());
        let obj_id = obj_id.read();

        let result = syscall_obj_generate_key(obj_id as c_ulong, 256, usr_params.as_ptr(), 1);
        assert!(result.is_ok());

        let obj_arc = tee_obj_get(obj_id as tee_obj_id_type);
        assert!(obj_arc.is_ok());
        let obj_arc = obj_arc.unwrap();
        let obj = obj_arc.lock();
        assert_eq!(obj.info.objectType, TEE_TYPE_ECDSA_KEYPAIR);
        assert_eq!(obj.info.maxObjectSize, 256);
        assert_eq!(obj.info.objectUsage, TEE_USAGE_DEFAULT);
        assert_eq!(obj.attr.len(), 1);
        assert!(matches!(obj.attr[0], TeeCryptObj::ecc_keypair(_)));

        let ecc_keypair = match &obj.attr[0] {
            TeeCryptObj::ecc_keypair(ecc_keypair) => ecc_keypair,
            _ => panic!("ecc_keypair not found"),
        };
        assert_eq!(ecc_keypair.curve, TEE_ECC_CURVE_NIST_P256);
        assert!(ecc_keypair.d.bit_length().unwrap() <= 256);
        assert!(ecc_keypair.x.bit_length().unwrap() <= 256);
        assert!(ecc_keypair.y.bit_length().unwrap() <= 256);
        assert!(ecc_keypair.d.bit_length().unwrap() > 0);
        assert!(ecc_keypair.x.bit_length().unwrap() > 0);
        assert!(ecc_keypair.y.bit_length().unwrap() > 0);
    }

    #[unittest::def_test(custom)]
    fn test_cryp_state_alloc_rejects_busy_key() {
        let mut obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        let result = syscall_cryp_obj_alloc(TEE_TYPE_SM4 as _, 128, obj_id.as_user_ref());
        assert!(result.is_ok());
        let obj_id = obj_id.read();

        let result = syscall_obj_generate_key(obj_id as c_ulong, 128, core::ptr::null(), 0);
        assert!(result.is_ok());

        let mut state1: u32 = 0;
        let result = tee_cryp_state_alloc(
            TEE_ALG_SM4_ECB_NOPAD,
            TEE_OperationMode::TEE_MODE_ENCRYPT,
            Some(obj_id),
            None,
            &mut state1,
        );
        assert!(result.is_ok());

        let mut state2: u32 = 0;
        let result = tee_cryp_state_alloc(
            TEE_ALG_SM4_ECB_NOPAD,
            TEE_OperationMode::TEE_MODE_ENCRYPT,
            Some(obj_id),
            None,
            &mut state2,
        );
        assert_eq!(result.err(), Some(TEE_ERROR_BUSY));
    }

    #[unittest::def_test(custom)]
    fn test_cryp_hash_sm3() {
        let mut state: u32 = 0;
        let res = tee_cryp_state_alloc(
            TEE_ALG_SM3,
            TEE_OperationMode::TEE_MODE_DIGEST,
            None,
            None,
            &mut state,
        );
        assert!(res.is_ok());

        let res = tee_cryp_hash_init(state);
        assert!(res.is_ok());

        let data = b"abc";

        let res = tee_cryp_hash_update(state, &data[..]);
        assert!(res.is_ok());

        let mut hash: [u8; 32] = [0; 32];
        let res = tee_cryp_hash_final(state, &[], &mut hash);
        assert!(res.is_ok());
        let hash_size = res.unwrap();

        assert_eq!(hash_size, 32);
        assert_eq!(
            hash,
            [
                0x66, 0xc7, 0xf0, 0xf4, 0x62, 0xee, 0xed, 0xd9, 0xd1, 0xf2, 0xd4, 0x6b, 0xdc, 0x10,
                0xe4, 0xe2, 0x41, 0x67, 0xc4, 0x87, 0x5c, 0xf2, 0xf7, 0xa2, 0x29, 0x7d, 0xa0, 0x2b,
                0x8f, 0x4b, 0xa8, 0xe0
            ]
        );
    }

    #[unittest::def_test(custom)]
    fn test_cryp_hash_requires_init_and_enough_output() {
        let mut state: u32 = 0;
        let res = tee_cryp_state_alloc(
            TEE_ALG_SM3,
            TEE_OperationMode::TEE_MODE_DIGEST,
            None,
            None,
            &mut state,
        );
        assert!(res.is_ok());

        let result = tee_cryp_hash_update(state, b"abc");
        assert_eq!(result.err(), Some(TEE_ERROR_BAD_STATE));

        let result = tee_cryp_hash_init(state);
        assert!(result.is_ok());

        let mut hash = [0u8; 8];
        let result = tee_cryp_hash_final(state, &[], &mut hash);
        assert_eq!(result.err(), Some(TEE_ERROR_SHORT_BUFFER));
    }

    #[unittest::def_test(custom)]
    fn test_cryp_hmac_sm3() {
        let mut state: u32 = 0;
        let mut obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        let result = syscall_cryp_obj_alloc(TEE_TYPE_HMAC_SM3 as _, 128, obj_id.as_user_ref());
        assert!(result.is_ok());
        let obj_id = obj_id.read();

        // 随机生成密钥
        let result = syscall_obj_generate_key(obj_id as c_ulong, 128, core::ptr::null(), 0);
        assert!(result.is_ok());

        let obj_arc = tee_obj_get(obj_id as tee_obj_id_type);
        assert!(obj_arc.is_ok());
        let obj_arc = obj_arc.unwrap();
        let mut obj = obj_arc.lock();

        assert_eq!(obj.info.objectType, TEE_TYPE_HMAC_SM3);
        assert_eq!(obj.info.maxObjectSize, 128);
        assert_eq!(obj.info.objectUsage, TEE_USAGE_DEFAULT);
        assert_eq!(obj.attr.len(), 1);
        assert!(matches!(obj.attr[0], TeeCryptObj::obj_secret(_)));

        let key = b"abcdefghabcdefgh";
        let mut secret = tee_cryp_obj_secret_wrapper::new(32);
        secret.set_secret_data(key as &[u8]);
        assert_eq!(secret.key(), key);

        // 赋值固定的key用于验证结果
        let _ = core::mem::replace(&mut obj.attr[0], TeeCryptObj::obj_secret(secret));
        drop(obj);

        let res = tee_cryp_state_alloc(
            TEE_ALG_HMAC_SM3,
            TEE_OperationMode::TEE_MODE_MAC,
            Some(obj_id as _),
            None,
            &mut state,
        );
        assert!(res.is_ok());

        let res = tee_cryp_hash_init(state);
        assert!(res.is_ok());

        let data = b"abc";

        let res = tee_cryp_hash_update(state, &data[..]);
        assert!(res.is_ok());

        let mut hash: [u8; 32] = [0; 32];
        let res = tee_cryp_hash_final(state, &[], &mut hash);
        assert!(res.is_ok());
        let hash_size = res.unwrap();

        assert_eq!(hash_size, 32);
        assert_eq!(
            hash,
            [
                0x99, 0x67, 0xaf, 0x42, 0x68, 0xd7, 0xf6, 0x96, 0x40, 0xca, 0xb9, 0x99, 0x35, 0x18,
                0x0f, 0xb3, 0xc6, 0x9b, 0xc5, 0x82, 0xa2, 0xb9, 0x7f, 0xa7, 0x53, 0xb2, 0x6c, 0x58,
                0x10, 0xaa, 0xa0, 0x37
            ]
        );
    }

    #[unittest::def_test(custom)]
    fn test_cryp_state_copy_sm3_hash() {
        let mut src_state: u32 = 0;
        let mut dst_state: u32 = 0;

        let res = tee_cryp_state_alloc(
            TEE_ALG_SM3,
            TEE_OperationMode::TEE_MODE_DIGEST,
            None,
            None,
            &mut src_state,
        );
        assert!(res.is_ok());
        let res = tee_cryp_state_alloc(
            TEE_ALG_SM3,
            TEE_OperationMode::TEE_MODE_DIGEST,
            None,
            None,
            &mut dst_state,
        );
        assert!(res.is_ok());

        let res = tee_cryp_hash_init(src_state);
        assert!(res.is_ok());
        let res = tee_cryp_hash_update(src_state, b"abc");
        assert!(res.is_ok());

        let res = tee_cryp_state_copy(dst_state, src_state);
        assert!(res.is_ok());

        let mut src_hash = [0u8; 32];
        let mut dst_hash = [0u8; 32];
        let src_len = tee_cryp_hash_final(src_state, &[], &mut src_hash);
        let dst_len = tee_cryp_hash_final(dst_state, &[], &mut dst_hash);
        assert!(src_len.is_ok());
        assert!(dst_len.is_ok());
        assert_eq!(src_len.unwrap(), 32);
        assert_eq!(dst_len.unwrap(), 32);

        assert_eq!(
            src_hash,
            [
                0x66, 0xc7, 0xf0, 0xf4, 0x62, 0xee, 0xed, 0xd9, 0xd1, 0xf2, 0xd4, 0x6b, 0xdc, 0x10,
                0xe4, 0xe2, 0x41, 0x67, 0xc4, 0x87, 0x5c, 0xf2, 0xf7, 0xa2, 0x29, 0x7d, 0xa0, 0x2b,
                0x8f, 0x4b, 0xa8, 0xe0
            ]
        );
        assert_eq!(dst_hash, src_hash);
    }

    /// OP-TEE `regression_4001`: `TEE_DigestExtract` calls `hash_final` into the
    /// operation buffer, then `TEE_CopyOperation` must still succeed.
    #[unittest::def_test(custom)]
    fn test_cryp_hash_copy_after_digest_extract_style_final() {
        const HASH_DATA_MD5_IN1: [u8; 11] = [
            0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x6b, 0x6c, 0x6d,
        ];
        const HASH_DATA_MD5_OUT1: [u8; 16] = [
            0x61, 0x12, 0x71, 0x83, 0x70, 0x8d, 0x3a, 0xc7, 0xf1, 0x9b, 0x66, 0x06, 0xfc, 0xae,
            0x7d, 0xf6,
        ];

        let mut src_state: u32 = 0;
        let mut dst_state: u32 = 0;

        assert!(
            tee_cryp_state_alloc(
                TEE_ALG_MD5,
                TEE_OperationMode::TEE_MODE_DIGEST,
                None,
                None,
                &mut src_state,
            )
            .is_ok()
        );
        assert!(
            tee_cryp_state_alloc(
                TEE_ALG_MD5,
                TEE_OperationMode::TEE_MODE_DIGEST,
                None,
                None,
                &mut dst_state,
            )
            .is_ok()
        );
        assert!(tee_cryp_hash_init(src_state).is_ok());
        assert!(tee_cryp_hash_update(src_state, &HASH_DATA_MD5_IN1).is_ok());

        let mut digest_buf = [0u8; 16];
        let len = tee_cryp_hash_final(src_state, &[], &mut digest_buf).unwrap();
        assert_eq!(len, 16);
        assert_eq!(digest_buf, HASH_DATA_MD5_OUT1);

        assert!(tee_cryp_state_copy(dst_state, src_state).is_ok());

        let mut dst_digest = [0u8; 16];
        let dst_len = tee_cryp_hash_final(dst_state, &[], &mut dst_digest).unwrap();
        assert_eq!(dst_len, 16);
        assert_eq!(dst_digest, HASH_DATA_MD5_OUT1);
    }

    #[unittest::def_test(custom)]
    fn test_cryp_state_copy_hmac_sm3() {
        let mut src_obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        let mut dst_obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        let mut src_state: u32 = 0;
        let mut dst_state: u32 = 0;
        let key = b"abcdefghabcdefgh";

        let res = syscall_cryp_obj_alloc(TEE_TYPE_HMAC_SM3 as _, 128, src_obj_id.as_user_ref());
        assert!(res.is_ok());
        let src_obj_id = src_obj_id.read();

        let res = syscall_cryp_obj_alloc(TEE_TYPE_HMAC_SM3 as _, 128, dst_obj_id.as_user_ref());
        assert!(res.is_ok());
        let dst_obj_id = dst_obj_id.read();

        let src_obj = tee_obj_get(src_obj_id as tee_obj_id_type);
        assert!(src_obj.is_ok());
        let src_obj = src_obj.unwrap();
        let mut src_obj_guard = src_obj.lock();
        let mut src_secret = tee_cryp_obj_secret_wrapper::new(32);
        src_secret.set_secret_data(key);
        let _ = core::mem::replace(
            &mut src_obj_guard.attr[0],
            TeeCryptObj::obj_secret(src_secret),
        );
        drop(src_obj_guard);

        let dst_obj = tee_obj_get(dst_obj_id as tee_obj_id_type);
        assert!(dst_obj.is_ok());
        let dst_obj = dst_obj.unwrap();
        let mut dst_obj_guard = dst_obj.lock();
        let mut dst_secret = tee_cryp_obj_secret_wrapper::new(32);
        dst_secret.set_secret_data(key);
        let _ = core::mem::replace(
            &mut dst_obj_guard.attr[0],
            TeeCryptObj::obj_secret(dst_secret),
        );
        drop(dst_obj_guard);

        let res = tee_cryp_state_alloc(
            TEE_ALG_HMAC_SM3,
            TEE_OperationMode::TEE_MODE_MAC,
            Some(src_obj_id as _),
            None,
            &mut src_state,
        );
        assert!(res.is_ok());
        let res = tee_cryp_state_alloc(
            TEE_ALG_HMAC_SM3,
            TEE_OperationMode::TEE_MODE_MAC,
            Some(dst_obj_id as _),
            None,
            &mut dst_state,
        );
        assert!(res.is_ok());

        let res = tee_cryp_hash_init(src_state);
        assert!(res.is_ok());
        let res = tee_cryp_hash_update(src_state, b"abc");
        assert!(res.is_ok());

        let res = tee_cryp_state_copy(dst_state, src_state);
        assert!(res.is_ok());

        let mut src_hash = [0u8; 32];
        let mut dst_hash = [0u8; 32];
        let src_len = tee_cryp_hash_final(src_state, &[], &mut src_hash);
        let dst_len = tee_cryp_hash_final(dst_state, &[], &mut dst_hash);
        assert!(src_len.is_ok());
        assert!(dst_len.is_ok());
        assert_eq!(src_len.unwrap(), 32);
        assert_eq!(dst_len.unwrap(), 32);
        assert_eq!(dst_hash, src_hash);
    }

    #[unittest::def_test(custom)]
    fn test_cryp_state_copy_sm4_cbc_ctx() {
        let mut src_obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        let mut dst_obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        let mut src_state: u32 = 0;
        let mut dst_state: u32 = 0;
        let key = b"abcdefghabcdefgh";
        let iv = b"1234qwerasdfzxcv";
        let data1 = b"abcdefghabcdefgh";
        let data2 = b"1234567890987654";

        let res = syscall_cryp_obj_alloc(TEE_TYPE_SM4 as _, 128, src_obj_id.as_user_ref());
        assert!(res.is_ok());
        let src_obj_id = src_obj_id.read();
        let res = syscall_cryp_obj_alloc(TEE_TYPE_SM4 as _, 128, dst_obj_id.as_user_ref());
        assert!(res.is_ok());
        let dst_obj_id = dst_obj_id.read();

        let src_obj = tee_obj_get(src_obj_id as tee_obj_id_type);
        assert!(src_obj.is_ok());
        let src_obj = src_obj.unwrap();
        let mut src_obj_guard = src_obj.lock();
        let mut src_secret = tee_cryp_obj_secret_wrapper::new(32);
        src_secret.set_secret_data(key);
        let _ = core::mem::replace(
            &mut src_obj_guard.attr[0],
            TeeCryptObj::obj_secret(src_secret),
        );
        drop(src_obj_guard);

        let dst_obj = tee_obj_get(dst_obj_id as tee_obj_id_type);
        assert!(dst_obj.is_ok());
        let dst_obj = dst_obj.unwrap();
        let mut dst_obj_guard = dst_obj.lock();
        let mut dst_secret = tee_cryp_obj_secret_wrapper::new(32);
        dst_secret.set_secret_data(key);
        let _ = core::mem::replace(
            &mut dst_obj_guard.attr[0],
            TeeCryptObj::obj_secret(dst_secret),
        );
        drop(dst_obj_guard);

        let res = tee_cryp_state_alloc(
            TEE_ALG_SM4_CBC_NOPAD,
            TEE_OperationMode::TEE_MODE_ENCRYPT,
            Some(src_obj_id as _),
            None,
            &mut src_state,
        );
        assert!(res.is_ok());
        let res = tee_cryp_state_alloc(
            TEE_ALG_SM4_CBC_NOPAD,
            TEE_OperationMode::TEE_MODE_ENCRYPT,
            Some(dst_obj_id as _),
            None,
            &mut dst_state,
        );
        assert!(res.is_ok());

        let res = tee_cryp_cipher_init(src_state, Some(iv), CipherPaddingMode::None);
        assert!(res.is_ok());

        // 参考现有 CBC 用例，update 输出缓冲区留出额外 block 空间
        let mut out1 = [0u8; 32];
        let mut out2_src = [0u8; 32];
        let mut out2_dst = [0u8; 32];

        let res = tee_cryp_cipher_update(src_state, data1, &mut out1);
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), 16);

        let res = tee_cryp_state_copy(dst_state, src_state);
        assert!(res.is_ok());

        let res = tee_cryp_cipher_update(src_state, data2, &mut out2_src);
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), 16);
        let res = tee_cryp_cipher_update(dst_state, data2, &mut out2_dst);
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), 16);

        assert_eq!(&out2_dst[..16], &out2_src[..16]);
    }

    /// regression_4005 case 0: partial AE update, then `TEE_CopyOperation` / `ENC_FINAL` on both ops.
    #[unittest::def_test(custom)]
    fn test_cryp_state_copy_aes_ccm_after_partial_ae() {
        let mut src_state: u32 = 0;
        let mut dst_state: u32 = 0;
        let mut src_obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        let mut dst_obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();

        syscall_cryp_obj_alloc(TEE_TYPE_AES as _, 128, src_obj_id.as_user_ref()).unwrap();
        let src_obj_id = src_obj_id.read();
        syscall_cryp_obj_alloc(TEE_TYPE_AES as _, 128, dst_obj_id.as_user_ref()).unwrap();
        let dst_obj_id = dst_obj_id.read();

        let key = [
            0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x4b, 0x4c, 0x4d,
            0x4e, 0x4f,
        ];
        let nonce = [0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16];
        let aad = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07];
        let plain = [0x20, 0x21, 0x22, 0x23];
        let cipher_expect = [0x71, 0x62, 0x01, 0x5b];
        let tag_expect = [0x4d, 0xac, 0x25, 0x5d];

        for obj_id in [src_obj_id, dst_obj_id] {
            let obj = tee_obj_get(obj_id as tee_obj_id_type).unwrap();
            let mut guard = obj.lock();
            let mut secret = tee_cryp_obj_secret_wrapper::new(32);
            secret.set_secret_data(&key).unwrap();
            let _ = core::mem::replace(&mut guard.attr[0], TeeCryptObj::obj_secret(secret));
        }

        tee_cryp_state_alloc(
            TEE_ALG_AES_CCM,
            TEE_OperationMode::TEE_MODE_ENCRYPT,
            Some(src_obj_id as _),
            None,
            &mut src_state,
        )
        .unwrap();
        tee_cryp_state_alloc(
            TEE_ALG_AES_CCM,
            TEE_OperationMode::TEE_MODE_ENCRYPT,
            Some(dst_obj_id as _),
            None,
            &mut dst_state,
        )
        .unwrap();

        tee_cryp_authenc_init(
            src_state,
            &nonce,
            Some(aad.len()),
            Some(tag_expect.len()),
            Some(plain.len()),
        )
        .unwrap();
        tee_cryp_authenc_update_aad(src_state, &aad[..4]).unwrap();
        tee_cryp_authenc_update_aad(src_state, &aad[4..]).unwrap();

        let mut out_src = [0u8; 32];
        let n = tee_cryp_authenc_update_payload(src_state, &plain[..2], &mut out_src).unwrap();
        assert_eq!(n, 2);

        tee_cryp_state_copy(dst_state, src_state).unwrap();

        let mut out_src_tail = [0u8; 32];
        let mut out_dst_tail = [0u8; 32];
        let mut tag_src = [0u8; 4];
        let mut tag_dst = [0u8; 4];

        let tail_len = tee_cryp_authenc_enc_final(
            src_state,
            Some(&plain[2..]),
            &mut out_src_tail,
            &mut tag_src,
        )
        .unwrap();
        let tail_len2 = tee_cryp_authenc_enc_final(
            dst_state,
            Some(&plain[2..]),
            &mut out_dst_tail,
            &mut tag_dst,
        )
        .unwrap();

        assert_eq!(tail_len, tail_len2);
        assert_eq!(&out_src[..2], &cipher_expect[..2]);
        assert_eq!(&out_src_tail[..tail_len], &cipher_expect[2..]);
        assert_eq!(&out_dst_tail[..tail_len2], &cipher_expect[2..]);
        assert_eq!(tag_src, tag_expect);
        assert_eq!(tag_dst, tag_expect);

        // regression_4005 case 0: dual FREE_OPERATION — must not corrupt heap via CCM clone.
        tee_cryp_state_free(src_state).unwrap();
        tee_cryp_state_free(dst_state).unwrap();
    }

    #[unittest::def_test(custom)]
    fn test_cryp_cmac_aes() {
        let mut state: u32 = 0;
        let mut obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        let result = syscall_cryp_obj_alloc(TEE_TYPE_AES as _, 128, obj_id.as_user_ref());
        assert!(result.is_ok());
        let obj_id = obj_id.read();

        let result = syscall_obj_generate_key(obj_id as c_ulong, 128, core::ptr::null(), 0);
        assert!(result.is_ok());

        let obj_arc = tee_obj_get(obj_id as tee_obj_id_type);
        assert!(obj_arc.is_ok());
        let obj_arc = obj_arc.unwrap();
        let mut obj = obj_arc.lock();

        assert_eq!(obj.info.objectType, TEE_TYPE_AES);
        assert_eq!(obj.info.maxObjectSize, 128);
        assert_eq!(obj.info.objectUsage, TEE_USAGE_DEFAULT);
        assert_eq!(obj.attr.len(), 1);
        assert!(matches!(obj.attr[0], TeeCryptObj::obj_secret(_)));

        let key = b"\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f";
        let mut secret = tee_cryp_obj_secret_wrapper::new(32);
        secret.set_secret_data(key as &[u8]);
        assert_eq!(secret.key(), key);

        let _ = core::mem::replace(&mut obj.attr[0], TeeCryptObj::obj_secret(secret));
        drop(obj);

        let res = tee_cryp_state_alloc(
            TEE_ALG_AES_CMAC,
            TEE_OperationMode::TEE_MODE_MAC,
            Some(obj_id as _),
            None,
            &mut state,
        );
        assert!(res.is_ok());

        let res = tee_cryp_hash_init(state);
        assert!(res.is_ok());

        let data = b"\x00\x11\x22\x33\x44\x55\x66\x77\x88\x99\xaa\xbb\xcc\xdd\xee\xff";
        let res = tee_cryp_hash_update(state, data);
        assert!(res.is_ok());

        let mut hash: [u8; 16] = [0; 16];
        let res = tee_cryp_hash_final(state, &[], &mut hash);
        assert!(res.is_ok());
        let hash_size = res.unwrap();

        assert_eq!(hash_size, 16);
        assert_eq!(
            hash,
            [
                0x38, 0x7b, 0x36, 0x22, 0x8b, 0xa7, 0x77, 0x44, 0x5b, 0xaf, 0xa0, 0x36, 0x45, 0xb9,
                0x40, 0x10
            ]
        );
    }

    /// Populate a transient AES object and allocate `TEE_ALG_AES_CMAC` state (optee crypt TA).
    fn alloc_aes_cmac_mac_state(key: &[u8], state: &mut u32) -> TestResult {
        let mut obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        assert!(syscall_cryp_obj_alloc(TEE_TYPE_AES as _, 128, obj_id.as_user_ref()).is_ok());
        let obj_id = obj_id.read();

        let obj_arc = tee_obj_get(obj_id as tee_obj_id_type).unwrap();
        let mut obj = obj_arc.lock();
        let mut secret = tee_cryp_obj_secret_wrapper::new(32);
        secret.set_secret_data(key);
        let _ = core::mem::replace(&mut obj.attr[0], TeeCryptObj::obj_secret(secret));
        drop(obj);

        assert!(
            tee_cryp_state_alloc(
                TEE_ALG_AES_CMAC,
                TEE_OperationMode::TEE_MODE_MAC,
                Some(obj_id as _),
                None,
                state,
            )
            .is_ok()
        );
        TestResult::Ok
    }

    /// `xtee_test` regression_4002 MAC case 46: `TEE_ALG_AES_CMAC` + `B_MAC_CMAC_VECT2_*`.
    ///
    /// Mirrors CA/TA sequence (op1/op2/op3):
    /// MAC_INIT(op1) → COPY(op3←op1) → MAC_UPDATE(op1, 9B) → COPY(op2←op1) →
    /// MAC_FINAL(op2, tail 7B) → MAC_INIT(op1) → MAC_FINAL(op1, full) →
    /// MAC_FINAL(op3, full)  // TEE_MACCompareFinal compute path
    #[unittest::def_test(custom)]
    fn test_regression_4002_mac_aes_cmac_vect2() {
        const DATA: [u8; 16] = [
            0x6b, 0xc1, 0xbe, 0xe2, 0x2e, 0x40, 0x9f, 0x96, 0xe9, 0x3d, 0x7e, 0x11, 0x73, 0x93,
            0x17, 0x2a,
        ];
        const KEY: [u8; 16] = [
            0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6, 0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf,
            0x4f, 0x3c,
        ];
        const EXPECTED: [u8; 16] = [
            0x07, 0x0a, 0x16, 0xb4, 0x6b, 0x4d, 0x41, 0x44, 0xf7, 0x9b, 0xdd, 0x9d, 0xd0, 0x4a,
            0x28, 0x7c,
        ];
        const IN_INCR: usize = 9;

        let mut op1: u32 = 0;
        let mut op2: u32 = 0;
        let mut op3: u32 = 0;
        let r = alloc_aes_cmac_mac_state(&KEY, &mut op1);
        if r.is_failed() {
            return r;
        }
        let r = alloc_aes_cmac_mac_state(&KEY, &mut op2);
        if r.is_failed() {
            return r;
        }
        let r = alloc_aes_cmac_mac_state(&KEY, &mut op3);
        if r.is_failed() {
            return r;
        }

        // MAC_INIT(op1)
        assert!(tee_cryp_hash_init(op1).is_ok());

        // COPY(op3 ← op1) right after MAC_INIT (before MAC_UPDATE)
        assert!(tee_cryp_state_copy(op3, op1).is_ok());

        // MAC_UPDATE(op1): single 9-byte chunk (multiple_incr == false)
        assert!(tee_cryp_hash_update(op1, &DATA[..IN_INCR]).is_ok());

        // COPY(op2 ← op1) after partial update
        assert!(tee_cryp_state_copy(op2, op1).is_ok());

        // MAC_FINAL_COMPUTE(op2, tail): remaining 7 bytes
        let mut out_tail = [0u8; 16];
        let tail = &DATA[IN_INCR..];
        assert_eq!(tail.len(), 7);
        let len_tail = tee_cryp_hash_final(op2, tail, &mut out_tail).unwrap();
        assert_eq!(len_tail, 16);
        assert_eq!(out_tail, EXPECTED, "tail final (op2)");

        // MAC_INIT(op1) again — clears partial progress on op1
        assert!(tee_cryp_hash_init(op1).is_ok());

        // MAC_FINAL_COMPUTE(op1, full input)
        let mut out_full = [0u8; 16];
        let len_full = tee_cryp_hash_final(op1, &DATA, &mut out_full).unwrap();
        assert_eq!(len_full, 16);
        assert_eq!(out_full, EXPECTED, "full final after re-init (op1)");

        // MAC_FINAL_COMPARE path: MACComputeFinal(op3, full) without re-init on op3
        let mut out_op3 = [0u8; 16];
        let len_op3 = tee_cryp_hash_final(op3, &DATA, &mut out_op3).unwrap();
        assert_eq!(len_op3, 16);
        assert_eq!(out_op3, EXPECTED, "compare path (op3 snapshot)");
        assert_eq!(out_op3, out_full, "op3 snapshot vs op1 re-init full");
    }

    /// Minimal repro: op3 only gets post-MAC_INIT copy, then one-shot full final.
    #[unittest::def_test(custom)]
    fn test_regression_4002_mac_aes_cmac_vect2_op3_snapshot_only() {
        const DATA: [u8; 16] = [
            0x6b, 0xc1, 0xbe, 0xe2, 0x2e, 0x40, 0x9f, 0x96, 0xe9, 0x3d, 0x7e, 0x11, 0x73, 0x93,
            0x17, 0x2a,
        ];
        const KEY: [u8; 16] = [
            0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6, 0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf,
            0x4f, 0x3c,
        ];
        const EXPECTED: [u8; 16] = [
            0x07, 0x0a, 0x16, 0xb4, 0x6b, 0x4d, 0x41, 0x44, 0xf7, 0x9b, 0xdd, 0x9d, 0xd0, 0x4a,
            0x28, 0x7c,
        ];

        let mut op1: u32 = 0;
        let mut op3: u32 = 0;
        let r = alloc_aes_cmac_mac_state(&KEY, &mut op1);
        if r.is_failed() {
            return r;
        }
        let r = alloc_aes_cmac_mac_state(&KEY, &mut op3);
        if r.is_failed() {
            return r;
        }

        assert!(tee_cryp_hash_init(op1).is_ok());
        assert!(tee_cryp_state_copy(op3, op1).is_ok());

        let mut out_op1 = [0u8; 16];
        assert!(tee_cryp_hash_init(op1).is_ok());
        let len1 = tee_cryp_hash_final(op1, &DATA, &mut out_op1).unwrap();
        assert_eq!(len1, 16);
        assert_eq!(out_op1, EXPECTED);

        let mut out_op3 = [0u8; 16];
        let len3 = tee_cryp_hash_final(op3, &DATA, &mut out_op3).unwrap();
        assert_eq!(len3, 16);
        assert_eq!(out_op3, EXPECTED);
        assert_eq!(out_op3, out_op1);
    }

    #[unittest::def_test(custom)]
    fn test_cryp_sm4_ecb_encrypt() {
        let mut state: u32 = 0;
        let mut obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        let result = syscall_cryp_obj_alloc(TEE_TYPE_SM4 as _, 128, obj_id.as_user_ref());
        assert!(result.is_ok());
        let obj_id = obj_id.read();

        // 随机生成密钥
        let result = syscall_obj_generate_key(obj_id as c_ulong, 128, core::ptr::null(), 0);
        assert!(result.is_ok());

        let obj_arc = tee_obj_get(obj_id as tee_obj_id_type);
        assert!(obj_arc.is_ok());
        let obj_arc = obj_arc.unwrap();
        let mut obj = obj_arc.lock();

        assert_eq!(obj.info.objectType, TEE_TYPE_SM4);
        assert_eq!(obj.info.maxObjectSize, 128);
        assert_eq!(obj.info.objectUsage, TEE_USAGE_DEFAULT);
        assert_eq!(obj.attr.len(), 1);
        assert!(matches!(obj.attr[0], TeeCryptObj::obj_secret(_)));

        let key = b"abcdefghabcdefgh";
        let mut secret = tee_cryp_obj_secret_wrapper::new(32);
        secret.set_secret_data(key as &[u8]);
        assert_eq!(secret.key(), key);

        // 赋值固定的key用于验证结果
        let _ = core::mem::replace(&mut obj.attr[0], TeeCryptObj::obj_secret(secret));
        drop(obj);

        let res = tee_cryp_state_alloc(
            TEE_ALG_SM4_ECB_NOPAD,
            TEE_OperationMode::TEE_MODE_ENCRYPT,
            Some(obj_id as _),
            None,
            &mut state,
        );
        assert!(res.is_ok());

        let data1 = b"abcdefghabcdefgh";
        let data2 = b"1234567890987654";

        let res = tee_cryp_cipher_init(state, None, CipherPaddingMode::None);
        assert!(res.is_ok());

        // 由于mbedtls限制，输出缓冲区需要比输入数据长至少一个block_size
        let mut out = [0u8; 48];
        let mut total_len = 0;

        let res = tee_cryp_cipher_update(state, &data1[..], &mut out[total_len..]);
        assert!(res.is_ok());
        total_len += res.unwrap();

        let res = tee_cryp_cipher_update(state, &data2[..], &mut out[total_len..]);
        assert!(res.is_ok());
        total_len += res.unwrap();

        let res = tee_cryp_cipher_final(state, &[], &mut out[total_len..]);
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), 0);

        let output = &out[..32];
        assert_eq!(total_len, 32);
        assert_eq!(
            output,
            [
                0x1b, 0x22, 0x97, 0x80, 0x2e, 0x42, 0xe4, 0xe6, 0xfb, 0x7d, 0xce, 0x53, 0x25, 0xd8,
                0x02, 0x09, 0x53, 0x34, 0x8f, 0xa1, 0xd9, 0xc7, 0x46, 0x75, 0x25, 0x3c, 0x97, 0xae,
                0xfd, 0xdd, 0xa0, 0xe7
            ]
        );
    }

    #[unittest::def_test(custom)]
    fn test_cryp_cipher_update_rejects_uninitialized_state_and_short_buffer() {
        let mut obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        let result = syscall_cryp_obj_alloc(TEE_TYPE_SM4 as _, 128, obj_id.as_user_ref());
        assert!(result.is_ok());
        let obj_id = obj_id.read();

        let result = syscall_obj_generate_key(obj_id as c_ulong, 128, core::ptr::null(), 0);
        assert!(result.is_ok());

        let mut state: u32 = 0;
        let result = tee_cryp_state_alloc(
            TEE_ALG_SM4_ECB_NOPAD,
            TEE_OperationMode::TEE_MODE_ENCRYPT,
            Some(obj_id),
            None,
            &mut state,
        );
        assert!(result.is_ok());

        let input = *b"abcdefghabcdefgh";
        let mut short = [0u8; 8];
        let result = tee_cryp_cipher_update(state, &input, &mut short);
        assert_eq!(result.err(), Some(TEE_ERROR_BAD_STATE));

        let result = tee_cryp_cipher_init(state, None, CipherPaddingMode::None);
        assert!(result.is_ok());

        let result = tee_cryp_cipher_update(state, &input, &mut short);
        assert_eq!(result.err(), Some(TEE_ERROR_SHORT_BUFFER));
    }

    #[unittest::def_test(custom)]
    fn test_cryp_sm4_ecb_decrypt() {
        let mut state: u32 = 0;
        let mut obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        let result = syscall_cryp_obj_alloc(TEE_TYPE_SM4 as _, 128, obj_id.as_user_ref());
        assert!(result.is_ok());
        let obj_id = obj_id.read();

        // 随机生成密钥
        let result = syscall_obj_generate_key(obj_id as c_ulong, 128, core::ptr::null(), 0);
        assert!(result.is_ok());

        let obj_arc = tee_obj_get(obj_id as tee_obj_id_type);
        assert!(obj_arc.is_ok());
        let obj_arc = obj_arc.unwrap();
        let mut obj = obj_arc.lock();

        assert_eq!(obj.info.objectType, TEE_TYPE_SM4);
        assert_eq!(obj.info.maxObjectSize, 128);
        assert_eq!(obj.info.objectUsage, TEE_USAGE_DEFAULT);
        assert_eq!(obj.attr.len(), 1);
        assert!(matches!(obj.attr[0], TeeCryptObj::obj_secret(_)));

        let key = b"abcdefgh12345678";
        let mut secret = tee_cryp_obj_secret_wrapper::new(32);
        secret.set_secret_data(key as &[u8]);
        assert_eq!(secret.key(), key);

        // 赋值固定的key用于验证结果
        let _ = core::mem::replace(&mut obj.attr[0], TeeCryptObj::obj_secret(secret));
        drop(obj);

        let res = tee_cryp_state_alloc(
            TEE_ALG_SM4_ECB_NOPAD,
            TEE_OperationMode::TEE_MODE_DECRYPT,
            Some(obj_id as _),
            None,
            &mut state,
        );
        assert!(res.is_ok());

        let data1: [u8; 16] = [
            0x9b, 0x46, 0x5b, 0x81, 0x3f, 0xea, 0x31, 0xd6, 0x78, 0xe9, 0xad, 0x06, 0x00, 0x21,
            0x53, 0x48,
        ];
        let data2: [u8; 16] = [
            0x6e, 0x51, 0x8c, 0xae, 0xe0, 0xe1, 0x0f, 0x6e, 0xb8, 0x95, 0x5c, 0x2e, 0x38, 0x24,
            0x81, 0xd7,
        ];

        let res = tee_cryp_cipher_init(state, None, CipherPaddingMode::None);
        assert!(res.is_ok());

        let mut out = [0u8; 48];
        let mut total_len = 0;

        let res = tee_cryp_cipher_update(state, &data1[..], &mut out[total_len..]);
        assert!(res.is_ok());
        total_len += res.unwrap();

        let res = tee_cryp_cipher_update(state, &data2[..], &mut out[total_len..]);
        assert!(res.is_ok());
        total_len += res.unwrap();

        let res = tee_cryp_cipher_final(state, &[], &mut out[total_len..]);
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), 0);

        let output = &out[..32];
        assert_eq!(total_len, 32);
        assert_eq!(output, *b"abcdefghijklmnop1234567887654321");
    }

    #[unittest::def_test(custom)]
    fn test_cryp_sm4_cbc_encrypt() {
        let mut state: u32 = 0;
        let mut obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        let result = syscall_cryp_obj_alloc(TEE_TYPE_SM4 as _, 128, obj_id.as_user_ref());
        assert!(result.is_ok());
        let obj_id = obj_id.read();

        // 随机生成密钥
        let result = syscall_obj_generate_key(obj_id as c_ulong, 128, core::ptr::null(), 0);
        assert!(result.is_ok());

        let obj_arc = tee_obj_get(obj_id as tee_obj_id_type);
        assert!(obj_arc.is_ok());
        let obj_arc = obj_arc.unwrap();
        let mut obj = obj_arc.lock();

        assert_eq!(obj.info.objectType, TEE_TYPE_SM4);
        assert_eq!(obj.info.maxObjectSize, 128);
        assert_eq!(obj.info.objectUsage, TEE_USAGE_DEFAULT);
        assert_eq!(obj.attr.len(), 1);
        assert!(matches!(obj.attr[0], TeeCryptObj::obj_secret(_)));

        let key = b"abcdefghabcdefgh";
        let mut secret = tee_cryp_obj_secret_wrapper::new(32);
        secret.set_secret_data(key as &[u8]);
        assert_eq!(secret.key(), key);

        // 赋值固定的key用于验证结果
        let _ = core::mem::replace(&mut obj.attr[0], TeeCryptObj::obj_secret(secret));
        drop(obj);

        let res = tee_cryp_state_alloc(
            TEE_ALG_SM4_CBC_NOPAD,
            TEE_OperationMode::TEE_MODE_ENCRYPT,
            Some(obj_id as _),
            None,
            &mut state,
        );
        assert!(res.is_ok());

        let data = b"abcdefghabcdefgh1234567890987654";
        let iv = b"1234qwerasdfzxcv";

        let res = tee_cryp_cipher_init(state, Some(&iv[..]), CipherPaddingMode::Pkcs7);
        assert!(res.is_ok());

        let mut out = [0u8; 48];
        let mut total_len = 0;

        let res = tee_cryp_cipher_update(state, &data[..], &mut out[total_len..]);
        assert!(res.is_ok());
        total_len += res.unwrap();

        // 处理填充
        let res = tee_cryp_cipher_final(state, &[], &mut out[total_len..]);
        assert!(res.is_ok());
        total_len += res.unwrap();

        assert_eq!(total_len, 48);
        assert_eq!(
            &out[..total_len],
            [
                0xce, 0x3b, 0x91, 0x3b, 0x42, 0xf3, 0x9d, 0x3d, 0x61, 0xfb, 0x75, 0x2f, 0xff, 0x81,
                0x51, 0xc6, 0x13, 0xf1, 0x0a, 0x8b, 0xb9, 0x5c, 0x8e, 0xe1, 0x59, 0x56, 0x6c, 0xc9,
                0xcb, 0x91, 0x57, 0xf8, 0xf3, 0x4f, 0xa5, 0xa9, 0x0c, 0x02, 0x39, 0xcc, 0x76, 0x1b,
                0x4f, 0xe2, 0xb1, 0xbc, 0xd1, 0x96
            ]
        );
    }

    #[unittest::def_test(custom)]
    fn test_cryp_sm4_cbc_decrypt() {
        let mut state: u32 = 0;
        let mut obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        let result = syscall_cryp_obj_alloc(TEE_TYPE_SM4 as _, 128, obj_id.as_user_ref());
        assert!(result.is_ok());
        let obj_id = obj_id.read();

        // 随机生成密钥
        let result = syscall_obj_generate_key(obj_id as c_ulong, 128, core::ptr::null(), 0);
        assert!(result.is_ok());

        let obj_arc = tee_obj_get(obj_id as tee_obj_id_type);
        assert!(obj_arc.is_ok());
        let obj_arc = obj_arc.unwrap();
        let mut obj = obj_arc.lock();

        assert_eq!(obj.info.objectType, TEE_TYPE_SM4);
        assert_eq!(obj.info.maxObjectSize, 128);
        assert_eq!(obj.info.objectUsage, TEE_USAGE_DEFAULT);
        assert_eq!(obj.attr.len(), 1);
        assert!(matches!(obj.attr[0], TeeCryptObj::obj_secret(_)));

        let key = b"abcdefghabcdefgh";
        let mut secret = tee_cryp_obj_secret_wrapper::new(32);
        secret.set_secret_data(key as &[u8]);
        assert_eq!(secret.key(), key);

        // 赋值固定的key用于验证结果
        let _ = core::mem::replace(&mut obj.attr[0], TeeCryptObj::obj_secret(secret));
        drop(obj);

        let res = tee_cryp_state_alloc(
            TEE_ALG_SM4_CBC_NOPAD,
            TEE_OperationMode::TEE_MODE_DECRYPT,
            Some(obj_id as _),
            None,
            &mut state,
        );
        assert!(res.is_ok());

        // 解密的数据需要包括一个block_size大小的填充
        let data: [u8; 48] = [
            0xce, 0x3b, 0x91, 0x3b, 0x42, 0xf3, 0x9d, 0x3d, 0x61, 0xfb, 0x75, 0x2f, 0xff, 0x81,
            0x51, 0xc6, 0x13, 0xf1, 0x0a, 0x8b, 0xb9, 0x5c, 0x8e, 0xe1, 0x59, 0x56, 0x6c, 0xc9,
            0xcb, 0x91, 0x57, 0xf8, 0xf3, 0x4f, 0xa5, 0xa9, 0x0c, 0x02, 0x39, 0xcc, 0x76, 0x1b,
            0x4f, 0xe2, 0xb1, 0xbc, 0xd1, 0x96,
        ];
        let iv = b"1234qwerasdfzxcv";

        let res = tee_cryp_cipher_init(state, Some(&iv[..]), CipherPaddingMode::Pkcs7);
        assert!(res.is_ok());

        // 输出区域大小仍然需要比输入数据大一个block_size
        let mut out = [0u8; 64];
        let mut total_len = 0;

        let res = tee_cryp_cipher_update(state, &data[..], &mut out[total_len..]);
        assert!(res.is_ok());
        total_len += res.unwrap();

        let res = tee_cryp_cipher_final(state, &[], &mut out[total_len..]);
        assert!(res.is_ok());
        total_len += res.unwrap();

        assert_eq!(total_len, 32);
        assert_eq!(&out[..32], *b"abcdefghabcdefgh1234567890987654");
    }

    #[unittest::def_test(custom)]
    fn test_cryp_sm4_gcm_encrypt() {
        let mut state: u32 = 0;
        let mut obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        let result = syscall_cryp_obj_alloc(TEE_TYPE_SM4 as _, 128, obj_id.as_user_ref());
        assert!(result.is_ok());
        let obj_id = obj_id.read();

        // 随机生成密钥
        let result = syscall_obj_generate_key(obj_id as c_ulong, 128, core::ptr::null(), 0);
        assert!(result.is_ok());

        let obj_arc = tee_obj_get(obj_id as tee_obj_id_type);
        assert!(obj_arc.is_ok());
        let obj_arc = obj_arc.unwrap();
        let mut obj = obj_arc.lock();

        assert_eq!(obj.info.objectType, TEE_TYPE_SM4);
        assert_eq!(obj.info.maxObjectSize, 128);
        assert_eq!(obj.info.objectUsage, TEE_USAGE_DEFAULT);
        assert_eq!(obj.attr.len(), 1);
        assert!(matches!(obj.attr[0], TeeCryptObj::obj_secret(_)));

        let key: [u8; 16] = [
            0x69, 0xEE, 0xDF, 0x37, 0x77, 0xE5, 0x94, 0xC3, 0x0E, 0x94, 0xE9, 0xC5, 0xE2, 0xBC,
            0xE4, 0x67,
        ];
        let mut secret = tee_cryp_obj_secret_wrapper::new(32);
        secret.set_secret_data(&key);
        assert_eq!(secret.key(), key);

        // 赋值固定的key用于验证结果
        let _ = core::mem::replace(&mut obj.attr[0], TeeCryptObj::obj_secret(secret));
        drop(obj);

        let res = tee_cryp_state_alloc(
            TEE_ALG_SM4_GCM,
            TEE_OperationMode::TEE_MODE_ENCRYPT,
            Some(obj_id as _),
            None,
            &mut state,
        );
        assert!(res.is_ok());

        let data: [u8; 64] = [
            0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xBB, 0xBB, 0xBB, 0xBB, 0xBB, 0xBB,
            0xBB, 0xBB, 0xCC, 0xCC, 0xCC, 0xCC, 0xCC, 0xCC, 0xCC, 0xCC, 0xDD, 0xDD, 0xDD, 0xDD,
            0xDD, 0xDD, 0xDD, 0xDD, 0xEE, 0xEE, 0xEE, 0xEE, 0xEE, 0xEE, 0xEE, 0xEE, 0xFF, 0xFF,
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xEE, 0xEE, 0xEE, 0xEE, 0xEE, 0xEE, 0xEE, 0xEE,
            0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA,
        ];
        let nonce: [u8; 12] = [
            0xA3, 0x33, 0x06, 0x38, 0xA8, 0x09, 0xBA, 0x35, 0x8D, 0x6C, 0x09, 0x8E,
        ];
        let ad: [u8; 20] = [
            0xFE, 0xED, 0xFA, 0xCE, 0xDE, 0xAD, 0xBE, 0xEF, 0xFE, 0xED, 0xFA, 0xCE, 0xDE, 0xAD,
            0xBE, 0xEF, 0xAB, 0xAD, 0xDA, 0xD2,
        ];
        let mut tag = [0u8; 16];
        let mut out = [0u8; 80];
        let mut total_len = 0;

        let res = tee_cryp_authenc_init(state, &nonce, None, None, None);
        assert!(res.is_ok());

        let res = tee_cryp_authenc_update_aad(state, &ad);
        assert!(res.is_ok());

        let res = tee_cryp_authenc_update_payload(state, &data[..], &mut out[total_len..]);
        assert!(res.is_ok());
        total_len += res.unwrap();

        let res = tee_cryp_authenc_enc_final(state, None, &mut out[total_len..], &mut tag);
        assert!(res.is_ok());
        total_len += res.unwrap();

        assert_eq!(total_len, 64);
        assert_eq!(
            &out[..64],
            [
                0x0C, 0x29, 0xFC, 0x49, 0x07, 0x11, 0x9F, 0x99, 0xC4, 0x92, 0xE2, 0xFA, 0x7B, 0x63,
                0x3F, 0x4E, 0x16, 0x5B, 0xE5, 0x35, 0x85, 0xAB, 0xED, 0x71, 0x8B, 0xA3, 0x9C, 0xAB,
                0x80, 0xA0, 0x63, 0x92, 0x73, 0x1E, 0x5C, 0xE6, 0xE3, 0x58, 0x1D, 0xCA, 0xF1, 0x19,
                0x03, 0x7D, 0x99, 0x8A, 0x0F, 0x52, 0x2D, 0x68, 0x0A, 0x9D, 0xCB, 0x40, 0x5A, 0xAD,
                0xF8, 0x00, 0xC0, 0xC7, 0x98, 0xBA, 0xE3, 0x8A
            ]
        );
        assert_eq!(
            tag,
            [
                0x19, 0x7F, 0x6C, 0xC5, 0x52, 0x3D, 0xA3, 0x6A, 0x3B, 0x2C, 0x42, 0x92, 0x44, 0xC4,
                0x70, 0xAA
            ]
        );
    }

    #[unittest::def_test(custom)]
    fn test_cryp_sm4_gcm_split_aad_matches_one_shot() {
        let mut state: u32 = 0;
        let mut obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        syscall_cryp_obj_alloc(TEE_TYPE_SM4 as _, 128, obj_id.as_user_ref()).unwrap();
        let obj_id = obj_id.read();
        syscall_obj_generate_key(obj_id as c_ulong, 128, core::ptr::null(), 0).unwrap();

        let obj = tee_obj_get(obj_id as tee_obj_id_type).unwrap();
        let mut obj_guard = obj.lock();
        let key = [
            0x69, 0xEE, 0xDF, 0x37, 0x77, 0xE5, 0x94, 0xC3, 0x0E, 0x94, 0xE9, 0xC5, 0xE2, 0xBC,
            0xE4, 0x67,
        ];
        let mut secret = tee_cryp_obj_secret_wrapper::new(32);
        secret.set_secret_data(&key).unwrap();
        let _ = core::mem::replace(&mut obj_guard.attr[0], TeeCryptObj::obj_secret(secret));
        drop(obj_guard);

        tee_cryp_state_alloc(
            TEE_ALG_SM4_GCM,
            TEE_OperationMode::TEE_MODE_ENCRYPT,
            Some(obj_id as _),
            None,
            &mut state,
        )
        .unwrap();

        let data: [u8; 64] = [
            0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xBB, 0xBB, 0xBB, 0xBB, 0xBB, 0xBB,
            0xBB, 0xBB, 0xCC, 0xCC, 0xCC, 0xCC, 0xCC, 0xCC, 0xCC, 0xCC, 0xDD, 0xDD, 0xDD, 0xDD,
            0xDD, 0xDD, 0xDD, 0xDD, 0xEE, 0xEE, 0xEE, 0xEE, 0xEE, 0xEE, 0xEE, 0xEE, 0xFF, 0xFF,
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xEE, 0xEE, 0xEE, 0xEE, 0xEE, 0xEE, 0xEE, 0xEE,
            0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA,
        ];
        let nonce = [
            0xA3, 0x33, 0x06, 0x38, 0xA8, 0x09, 0xBA, 0x35, 0x8D, 0x6C, 0x09, 0x8E,
        ];
        let ad = [
            0xFE, 0xED, 0xFA, 0xCE, 0xDE, 0xAD, 0xBE, 0xEF, 0xFE, 0xED, 0xFA, 0xCE, 0xDE, 0xAD,
            0xBE, 0xEF, 0xAB, 0xAD, 0xDA, 0xD2,
        ];

        tee_cryp_authenc_init(state, &nonce, None, None, None).unwrap();
        tee_cryp_authenc_update_aad(state, &ad[..10]).unwrap();
        tee_cryp_authenc_update_aad(state, &ad[10..]).unwrap();

        let mut out = [0u8; 80];
        let mut tag = [0u8; 16];
        let n = tee_cryp_authenc_update_payload(state, &data, &mut out).unwrap();
        tee_cryp_authenc_enc_final(state, None, &mut out[n..], &mut tag).unwrap();

        assert_eq!(n, 64);
        assert_eq!(
            &out[..64],
            [
                0x0C, 0x29, 0xFC, 0x49, 0x07, 0x11, 0x9F, 0x99, 0xC4, 0x92, 0xE2, 0xFA, 0x7B, 0x63,
                0x3F, 0x4E, 0x16, 0x5B, 0xE5, 0x35, 0x85, 0xAB, 0xED, 0x71, 0x8B, 0xA3, 0x9C, 0xAB,
                0x80, 0xA0, 0x63, 0x92, 0x73, 0x1E, 0x5C, 0xE6, 0xE3, 0x58, 0x1D, 0xCA, 0xF1, 0x19,
                0x03, 0x7D, 0x99, 0x8A, 0x0F, 0x52, 0x2D, 0x68, 0x0A, 0x9D, 0xCB, 0x40, 0x5A, 0xAD,
                0xF8, 0x00, 0xC0, 0xC7, 0x98, 0xBA, 0xE3, 0x8A
            ]
        );
        assert_eq!(
            tag,
            [
                0x19, 0x7F, 0x6C, 0xC5, 0x52, 0x3D, 0xA3, 0x6A, 0x3B, 0x2C, 0x42, 0x92, 0x44, 0xC4,
                0x70, 0xAA
            ]
        );
    }

    /// OP-TEE xtest regression_4005 AE case 6/7: AES-GCM vect1 (empty AAD/payload).
    #[unittest::def_test(custom)]
    fn test_cryp_aes_gcm_vect1_empty_payload() {
        let key = [0u8; 16];
        let nonce = [0u8; 12];
        let tag_expect = [
            0x58, 0xe2, 0xfc, 0xce, 0xfa, 0x7e, 0x30, 0x61, 0x36, 0x7f, 0x1d, 0x57, 0xa4, 0xe7,
            0x45, 0x5a,
        ];

        let mut enc_obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        syscall_cryp_obj_alloc(TEE_TYPE_AES as _, 128, enc_obj_id.as_user_ref()).unwrap();
        let enc_obj_id = enc_obj_id.read();
        let obj = tee_obj_get(enc_obj_id as tee_obj_id_type).unwrap();
        let mut obj_guard = obj.lock();
        let mut secret = tee_cryp_obj_secret_wrapper::new(32);
        secret.set_secret_data(&key).unwrap();
        let _ = core::mem::replace(&mut obj_guard.attr[0], TeeCryptObj::obj_secret(secret));
        drop(obj_guard);

        let mut enc_state: u32 = 0;
        tee_cryp_state_alloc(
            TEE_ALG_AES_GCM,
            TEE_OperationMode::TEE_MODE_ENCRYPT,
            Some(enc_obj_id as _),
            None,
            &mut enc_state,
        )
        .unwrap();
        tee_cryp_authenc_init(enc_state, &nonce, None, None, None).unwrap();

        let mut out = [0u8; 16];
        let mut tag = [0u8; 16];
        let n = tee_cryp_authenc_enc_final(enc_state, None, &mut out, &mut tag).unwrap();
        assert_eq!(n, 0);
        assert_eq!(tag, tag_expect);

        let mut dec_obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        let mut dec_obj2_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        syscall_cryp_obj_alloc(TEE_TYPE_AES as _, 128, dec_obj_id.as_user_ref()).unwrap();
        syscall_cryp_obj_alloc(TEE_TYPE_AES as _, 128, dec_obj2_id.as_user_ref()).unwrap();
        let dec_obj_id = dec_obj_id.read();
        let dec_obj2_id = dec_obj2_id.read();
        for obj_id in [dec_obj_id, dec_obj2_id] {
            let obj = tee_obj_get(obj_id as tee_obj_id_type).unwrap();
            let mut obj_guard = obj.lock();
            let mut secret = tee_cryp_obj_secret_wrapper::new(32);
            secret.set_secret_data(&key).unwrap();
            let _ = core::mem::replace(&mut obj_guard.attr[0], TeeCryptObj::obj_secret(secret));
        }

        let mut dec_state: u32 = 0;
        let mut dec_state2: u32 = 0;
        tee_cryp_state_alloc(
            TEE_ALG_AES_GCM,
            TEE_OperationMode::TEE_MODE_DECRYPT,
            Some(dec_obj_id as _),
            None,
            &mut dec_state,
        )
        .unwrap();
        tee_cryp_state_alloc(
            TEE_ALG_AES_GCM,
            TEE_OperationMode::TEE_MODE_DECRYPT,
            Some(dec_obj2_id as _),
            None,
            &mut dec_state2,
        )
        .unwrap();
        tee_cryp_authenc_init(dec_state, &nonce, None, None, None).unwrap();
        tee_cryp_state_copy(dec_state2, dec_state).unwrap();

        let mut plain = [0u8; 16];
        let n = tee_cryp_authenc_dec_final(dec_state, None, &mut plain, &tag_expect).unwrap();
        assert_eq!(n, 0);

        let mut plain2 = [0u8; 16];
        let n2 = tee_cryp_authenc_dec_final(dec_state2, None, &mut plain2, &tag_expect).unwrap();
        assert_eq!(n2, 0);
    }

    /// OP-TEE xtest regression_4005 AE case 8: AES-GCM vect2, 9+7 payload split + copy.
    #[unittest::def_test(custom)]
    fn test_cryp_aes_gcm_vect2_split_encrypt_copy() {
        let key = [0u8; 16];
        let nonce = [0u8; 12];
        let plain = [0u8; 16];
        let cipher_expect = [
            0x03, 0x88, 0xda, 0xce, 0x60, 0xb6, 0xa3, 0x92, 0xf3, 0x28, 0xc2, 0xb9, 0x71, 0xb2,
            0xfe, 0x78,
        ];
        let tag_expect = [
            0xab, 0x6e, 0x47, 0xd4, 0x2c, 0xec, 0x13, 0xbd, 0xf5, 0x3a, 0x67, 0xb2, 0x12, 0x57,
            0xbd, 0xdf,
        ];

        let mut obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        let mut obj2_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        syscall_cryp_obj_alloc(TEE_TYPE_AES as _, 128, obj_id.as_user_ref()).unwrap();
        syscall_cryp_obj_alloc(TEE_TYPE_AES as _, 128, obj2_id.as_user_ref()).unwrap();
        let obj_id = obj_id.read();
        let obj2_id = obj2_id.read();
        for id in [obj_id, obj2_id] {
            let obj = tee_obj_get(id as tee_obj_id_type).unwrap();
            let mut guard = obj.lock();
            let mut secret = tee_cryp_obj_secret_wrapper::new(32);
            secret.set_secret_data(&key).unwrap();
            let _ = core::mem::replace(&mut guard.attr[0], TeeCryptObj::obj_secret(secret));
        }

        let mut src_state: u32 = 0;
        let mut dst_state: u32 = 0;
        tee_cryp_state_alloc(
            TEE_ALG_AES_GCM,
            TEE_OperationMode::TEE_MODE_ENCRYPT,
            Some(obj_id as _),
            None,
            &mut src_state,
        )
        .unwrap();
        tee_cryp_state_alloc(
            TEE_ALG_AES_GCM,
            TEE_OperationMode::TEE_MODE_ENCRYPT,
            Some(obj2_id as _),
            None,
            &mut dst_state,
        )
        .unwrap();
        tee_cryp_authenc_init(src_state, &nonce, None, None, None).unwrap();

        let mut out = [0u8; 32];
        let n = tee_cryp_authenc_update_payload(src_state, &plain[..9], &mut out).unwrap();
        assert_eq!(n, 9);
        assert_eq!(&out[..9], &cipher_expect[..9]);

        tee_cryp_state_copy(dst_state, src_state).unwrap();

        let mut tag_src = [0u8; 16];
        let mut tag_dst = [0u8; 16];
        let tail_src =
            tee_cryp_authenc_enc_final(src_state, Some(&plain[9..]), &mut out[9..], &mut tag_src)
                .unwrap();
        let tail_dst =
            tee_cryp_authenc_enc_final(dst_state, Some(&plain[9..]), &mut out[9..], &mut tag_dst)
                .unwrap();

        assert_eq!(tail_src, 7);
        assert_eq!(tail_dst, 7);
        assert_eq!(&out[..16], cipher_expect);
        assert_eq!(tag_src, tag_expect);
        assert_eq!(tag_dst, tag_expect);
    }

    /// OP-TEE xtest regression_4005 AE case 14: AES-GCM vect6, 60-byte nonce.
    #[unittest::def_test(custom)]
    fn test_cryp_aes_gcm_vect6_long_nonce_encrypt() {
        let key = [
            0xfe, 0xff, 0xe9, 0x92, 0x86, 0x65, 0x73, 0x1c, 0x6d, 0x6a, 0x8f, 0x94, 0x67, 0x30,
            0x83, 0x08,
        ];
        let nonce = [
            0x93, 0x13, 0x22, 0x5d, 0xf8, 0x84, 0x06, 0xe5, 0x55, 0x90, 0x9c, 0x5a, 0xff, 0x52,
            0x69, 0xaa, 0x6a, 0x7a, 0x95, 0x38, 0x53, 0x4f, 0x7d, 0xa1, 0xe4, 0xc3, 0x03, 0xd2,
            0xa3, 0x18, 0xa7, 0x28, 0xc3, 0xc0, 0xc9, 0x51, 0x56, 0x80, 0x95, 0x39, 0xfc, 0xf0,
            0xe2, 0x42, 0x9a, 0x6b, 0x52, 0x54, 0x16, 0xae, 0xdb, 0xf5, 0xa0, 0xde, 0x6a, 0x57,
            0xa6, 0x37, 0xb3, 0x9b,
        ];
        let aad = [
            0xfe, 0xed, 0xfa, 0xce, 0xde, 0xad, 0xbe, 0xef, 0xfe, 0xed, 0xfa, 0xce, 0xde, 0xad,
            0xbe, 0xef, 0xab, 0xad, 0xda, 0xd2,
        ];
        let plain = [
            0xd9, 0x31, 0x32, 0x25, 0xf8, 0x84, 0x06, 0xe5, 0xa5, 0x59, 0x09, 0xc5, 0xaf, 0xf5,
            0x26, 0x9a, 0x86, 0xa7, 0xa9, 0x53, 0x15, 0x34, 0xf7, 0xda, 0x2e, 0x4c, 0x30, 0x3d,
            0x8a, 0x31, 0x8a, 0x72, 0x1c, 0x3c, 0x0c, 0x95, 0x95, 0x68, 0x09, 0x53, 0x2f, 0xcf,
            0x0e, 0x24, 0x49, 0xa6, 0xb5, 0x25, 0xb1, 0x6a, 0xed, 0xf5, 0xaa, 0x0d, 0xe6, 0x57,
            0xba, 0x63, 0x7b, 0x39,
        ];
        let cipher_expect = [
            0x8c, 0xe2, 0x49, 0x98, 0x62, 0x56, 0x15, 0xb6, 0x03, 0xa0, 0x33, 0xac, 0xa1, 0x3f,
            0xb8, 0x94, 0xbe, 0x91, 0x12, 0xa5, 0xc3, 0xa2, 0x11, 0xa8, 0xba, 0x26, 0x2a, 0x3c,
            0xca, 0x7e, 0x2c, 0xa7, 0x01, 0xe4, 0xa9, 0xa4, 0xfb, 0xa4, 0x3c, 0x90, 0xcc, 0xdc,
            0xb2, 0x81, 0xd4, 0x8c, 0x7c, 0x6f, 0xd6, 0x28, 0x75, 0xd2, 0xac, 0xa4, 0x17, 0x03,
            0x4c, 0x34, 0xae, 0xe5,
        ];
        let tag_expect = [
            0x61, 0x9c, 0xc5, 0xae, 0xff, 0xfe, 0x0b, 0xfa, 0x46, 0x2a, 0xf4, 0x3c, 0x16, 0x99,
            0xd0, 0x50,
        ];

        let mut obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        syscall_cryp_obj_alloc(TEE_TYPE_AES as _, 128, obj_id.as_user_ref()).unwrap();
        let obj_id = obj_id.read();
        let obj = tee_obj_get(obj_id as tee_obj_id_type).unwrap();
        let mut guard = obj.lock();
        let mut secret = tee_cryp_obj_secret_wrapper::new(32);
        secret.set_secret_data(&key).unwrap();
        let _ = core::mem::replace(&mut guard.attr[0], TeeCryptObj::obj_secret(secret));
        drop(guard);

        let mut state: u32 = 0;
        tee_cryp_state_alloc(
            TEE_ALG_AES_GCM,
            TEE_OperationMode::TEE_MODE_ENCRYPT,
            Some(obj_id as _),
            None,
            &mut state,
        )
        .unwrap();
        tee_cryp_authenc_init(state, &nonce, None, None, None).unwrap();

        tee_cryp_authenc_update_aad(state, &aad[..5]).unwrap();
        tee_cryp_authenc_update_aad(state, &aad[5..]).unwrap();

        let mut out = [0u8; 64];
        let mut n = 0usize;
        for chunk in plain.chunks(9) {
            n += tee_cryp_authenc_update_payload(state, chunk, &mut out[n..]).unwrap();
        }
        assert_eq!(n, plain.len());

        let mut tag = [0u8; 16];
        let tail = tee_cryp_authenc_enc_final(state, None, &mut out[n..], &mut tag).unwrap();
        assert_eq!(tail, 0);
        assert_eq!(&out[..plain.len()], cipher_expect);
        assert_eq!(tag, tag_expect);
    }

    #[unittest::def_test(custom)]
    fn test_cryp_authenc_aad_rejected_after_payload() {
        let mut state: u32 = 0;
        let mut obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        syscall_cryp_obj_alloc(TEE_TYPE_SM4 as _, 128, obj_id.as_user_ref()).unwrap();
        let obj_id = obj_id.read();
        syscall_obj_generate_key(obj_id as c_ulong, 128, core::ptr::null(), 0).unwrap();

        tee_cryp_state_alloc(
            TEE_ALG_SM4_GCM,
            TEE_OperationMode::TEE_MODE_ENCRYPT,
            Some(obj_id as _),
            None,
            &mut state,
        )
        .unwrap();

        let nonce = [0u8; 12];
        let data = [0u8; 16];
        let mut out = [0u8; 32];
        tee_cryp_authenc_init(state, &nonce, None, None, None).unwrap();
        tee_cryp_authenc_update_payload(state, &data, &mut out).unwrap();
        let res = tee_cryp_authenc_update_aad(state, b"late");
        assert_eq!(res.err(), Some(TEE_ERROR_BAD_PARAMETERS));
    }

    #[unittest::def_test(custom)]
    fn test_cryp_authenc_requires_ae_state() {
        let mut obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        let result = syscall_cryp_obj_alloc(TEE_TYPE_SM4 as _, 128, obj_id.as_user_ref());
        assert!(result.is_ok());
        let obj_id = obj_id.read();

        let result = syscall_obj_generate_key(obj_id as c_ulong, 128, core::ptr::null(), 0);
        assert!(result.is_ok());

        let mut state: u32 = 0;
        let result = tee_cryp_state_alloc(
            TEE_ALG_SM4_ECB_NOPAD,
            TEE_OperationMode::TEE_MODE_ENCRYPT,
            Some(obj_id),
            None,
            &mut state,
        );
        assert!(result.is_ok());

        let result = tee_cryp_cipher_init(state, None, CipherPaddingMode::None);
        assert!(result.is_ok());

        let result = tee_cryp_authenc_update_aad(state, b"aad");
        assert_eq!(result.err(), Some(TEE_ERROR_BAD_STATE));
    }

    fn des3_ecb_encrypt_test_setup(key: &[u8]) -> TeeResult<u32> {
        let mut obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        syscall_cryp_obj_alloc(TEE_TYPE_DES3 as _, 192, obj_id.as_user_ref())?;
        let obj_id = obj_id.read();

        let obj_arc = tee_obj_get(obj_id as tee_obj_id_type)?;
        let mut obj = obj_arc.lock();
        let mut secret = tee_cryp_obj_secret_wrapper::new(24);
        secret.set_secret_data(key)?;
        let _ = core::mem::replace(&mut obj.attr[0], TeeCryptObj::obj_secret(secret));
        drop(obj);

        let mut state: u32 = 0;
        tee_cryp_state_alloc(
            TEE_ALG_DES3_ECB_NOPAD,
            TEE_OperationMode::TEE_MODE_ENCRYPT,
            Some(obj_id),
            None,
            &mut state,
        )?;
        tee_cryp_cipher_init(state, None, CipherPaddingMode::None)?;
        Ok(state)
    }

    #[unittest::def_test(custom)]
    fn test_des3_ecb_2key_cipher_init() {
        let key = [
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff,
        ];
        let res = des3_ecb_encrypt_test_setup(&key);
        assert!(res.is_ok());
    }

    /// mbedtls `test_vec_ecb` vector for DES-EDE-ECB encrypt.
    #[unittest::def_test(custom)]
    fn test_des3_ecb_2key_encrypt() {
        let key = [
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff,
        ];
        let plain = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let expect = [0x92, 0x95, 0xb5, 0x9b, 0xb3, 0x84, 0x73, 0x6e];

        let state = des3_ecb_encrypt_test_setup(&key).unwrap();
        let mut out = [0u8; 8];
        let len = tee_cryp_cipher_update(state, &plain, &mut out).unwrap();
        assert_eq!(len, 8);
        assert_eq!(&out, &expect);
    }

    #[unittest::def_test(custom)]
    fn test_des3_ecb_3key_cipher_init() {
        let key = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54,
            0x32, 0x10, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67,
        ];
        let res = des3_ecb_encrypt_test_setup(&key);
        assert!(res.is_ok());
    }

    fn aes_xts_test_setup_keys(key1: &[u8], key2: &[u8]) -> (tee_obj_id_type, tee_obj_id_type) {
        let mut obj1_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        let mut obj2_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        syscall_cryp_obj_alloc(TEE_TYPE_AES as _, 128, obj1_id.as_user_ref()).unwrap();
        syscall_cryp_obj_alloc(TEE_TYPE_AES as _, 128, obj2_id.as_user_ref()).unwrap();
        let obj1_id = obj1_id.read() as tee_obj_id_type;
        let obj2_id = obj2_id.read() as tee_obj_id_type;

        for (id, material) in [(obj1_id, key1), (obj2_id, key2)] {
            let obj_arc = tee_obj_get(id).unwrap();
            let mut obj = obj_arc.lock();
            let mut secret = tee_cryp_obj_secret_wrapper::new(32);
            secret.set_secret_data(material).unwrap();
            let _ = core::mem::replace(&mut obj.attr[0], TeeCryptObj::obj_secret(secret));
        }
        (obj1_id, obj2_id)
    }

    #[unittest::def_test(custom)]
    fn test_aes_xts_incremental_update_matches_one_shot() {
        // IEEE P1619 vector (mbedtls aes_test_xts index 1): 32-byte unit.
        let key1 = [0x11u8; 16];
        let key2 = [0x22u8; 16];
        let iv = [
            0x33, 0x33, 0x33, 0x33, 0x33, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00,
        ];
        let plain = [0x44u8; 32];
        let expect = [
            0xc4, 0x54, 0x18, 0x5e, 0x6a, 0x16, 0x93, 0x6e, 0x39, 0x33, 0x40, 0x38, 0xac, 0xef,
            0x83, 0x8b, 0xfb, 0x18, 0x6f, 0xff, 0x74, 0x80, 0xad, 0xc4, 0x28, 0x93, 0x82, 0xec,
            0xd6, 0xd3, 0x94, 0xf0,
        ];

        let (obj1_id, obj2_id) = aes_xts_test_setup_keys(&key1, &key2);

        let mut state: u32 = 0;
        tee_cryp_state_alloc(
            TEE_ALG_AES_XTS,
            TEE_OperationMode::TEE_MODE_ENCRYPT,
            Some(obj1_id as _),
            Some(obj2_id as _),
            &mut state,
        )
        .unwrap();
        tee_cryp_cipher_init(state, Some(&iv), CipherPaddingMode::None).unwrap();

        let mut out = [0u8; 32];
        let mut off = 0usize;
        // Simulate regression_4003 in_incr=8: first diff at 16 if tweak is not continued.
        for chunk in plain.chunks(8) {
            let n = tee_cryp_cipher_update(state, chunk, &mut out[off..]).unwrap();
            off += n;
        }
        aes_xts_test_set_user_base(state, 0);
        let n = tee_cryp_cipher_final(state, &[], &mut out[off..]).unwrap();
        aes_xts_test_apply_patch(state, &mut out);
        off += n;
        assert_eq!(off, 32);
        assert_eq!(&out, &expect);
    }

    #[unittest::def_test(custom)]
    fn test_aes_xts_512_byte_incremental_update() {
        use crate::tee::crypto::aes_xts::{aes_xts_crypt, aes_xts_init};

        let key1 = [0x42u8; 16];
        let key2 = [0x24u8; 16];
        let iv = [0x07u8; 16];
        let plain = [0x5au8; 512];
        let mut expect = [0u8; 512];

        let mut combined = Vec::with_capacity(32);
        combined.extend_from_slice(&key1);
        combined.extend_from_slice(&key2);
        let mut xts = aes_xts_init(&combined, Some(&iv), false).unwrap();
        aes_xts_crypt(&mut xts, &plain, &mut expect).unwrap();

        let (obj1_id, obj2_id) = aes_xts_test_setup_keys(&key1, &key2);

        let mut state: u32 = 0;
        tee_cryp_state_alloc(
            TEE_ALG_AES_XTS,
            TEE_OperationMode::TEE_MODE_ENCRYPT,
            Some(obj1_id as _),
            Some(obj2_id as _),
            &mut state,
        )
        .unwrap();
        tee_cryp_cipher_init(state, Some(&iv), CipherPaddingMode::None).unwrap();

        let mut out = [0u8; 512];
        let mut off = 0usize;
        for chunk in plain.chunks(8) {
            off += tee_cryp_cipher_update(state, chunk, &mut out[off..]).unwrap();
        }
        aes_xts_test_set_user_base(state, 0);
        off += tee_cryp_cipher_final(state, &[], &mut out[off..]).unwrap();
        aes_xts_test_apply_patch(state, &mut out);
        assert_eq!(off, 512);
        assert_eq!(&out, &expect);
    }

    fn aes_xts_reference_crypt(
        key1: &[u8; 16],
        key2: &[u8; 16],
        iv: &[u8; 16],
        plain: &[u8],
        decrypt: bool,
    ) -> alloc::vec::Vec<u8> {
        use crate::tee::crypto::aes_xts::{aes_xts_crypt, aes_xts_init};

        let mut combined = alloc::vec::Vec::with_capacity(32);
        combined.extend_from_slice(key1);
        combined.extend_from_slice(key2);
        let mut xts = aes_xts_init(&combined, Some(iv), decrypt).unwrap();
        let mut out = alloc::vec![0u8; plain.len()];
        aes_xts_crypt(&mut xts, plain, &mut out).unwrap();
        out
    }

    /// Buffered update + final, matching libutee split for non-block-aligned lengths.
    fn aes_xts_reference_crypt_buffered(
        key1: &[u8; 16],
        key2: &[u8; 16],
        iv: &[u8; 16],
        input: &[u8],
        decrypt: bool,
    ) -> alloc::vec::Vec<u8> {
        use crate::tee::crypto::aes_xts::{
            AesXtsStream, aes_xts_final_buffered, aes_xts_init, aes_xts_update_buffered,
        };

        let mut combined = alloc::vec::Vec::with_capacity(32);
        combined.extend_from_slice(key1);
        combined.extend_from_slice(key2);
        let mut xts = aes_xts_init(&combined, Some(iv), decrypt).unwrap();
        let mut out = alloc::vec![0u8; input.len()];
        let mut pending = [0u8; 32];
        let mut pending_len = 0usize;
        let rem = input.len() % 16;
        let w = if rem == 0 {
            let stream = AesXtsStream {
                prior_bytes: 0,
                is_final: true,
            };
            aes_xts_update_buffered(
                &mut xts,
                &mut pending,
                &mut pending_len,
                input,
                &mut out,
                &stream,
            )
            .unwrap()
        } else {
            let stream_up = AesXtsStream {
                prior_bytes: 0,
                is_final: false,
            };
            aes_xts_update_buffered(
                &mut xts,
                &mut pending,
                &mut pending_len,
                &input[..input.len() - rem],
                &mut out,
                &stream_up,
            )
            .unwrap()
        };
        let stream_fin = AesXtsStream {
            prior_bytes: input.len() - rem,
            is_final: true,
        };
        let (n, patch) = aes_xts_final_buffered(
            &mut xts,
            &mut pending,
            &mut pending_len,
            &input[input.len() - rem..],
            &mut out[w..],
            &stream_fin,
        )
        .unwrap();
        if let Some(pb) = patch {
            if w >= 16 {
                out[w - 16..w].copy_from_slice(&pb);
            }
        }
        core::assert_eq!(w + n, input.len());
        out
    }

    fn aes_xts_test_set_user_base(state: u32, base: usize) {
        let cs = tee_cryp_state_get(state).unwrap();
        let mut guard = cs.lock();
        if let CrypCtx::CipherCtx(op) = &mut guard.ctx
            && let Some(xts) = &mut op.xts
        {
            xts.user_base = Some(base);
        }
    }

    fn aes_xts_test_apply_patch(state: u32, out: &mut [u8]) {
        let cs = tee_cryp_state_get(state).unwrap();
        let mut guard = cs.lock();
        let CrypCtx::CipherCtx(op) = &mut guard.ctx else {
            return;
        };
        if let Some(xts) = &mut op.xts {
            xts.apply_patch_to_buffer(out);
        }
    }

    fn aes_xts_tee_encrypt_incremental(
        key1: &[u8; 16],
        key2: &[u8; 16],
        iv: &[u8; 16],
        plain: &[u8],
        chunk: usize,
    ) -> alloc::vec::Vec<u8> {
        let (obj1_id, obj2_id) = aes_xts_test_setup_keys(key1, key2);
        let mut state: u32 = 0;
        tee_cryp_state_alloc(
            TEE_ALG_AES_XTS,
            TEE_OperationMode::TEE_MODE_ENCRYPT,
            Some(obj1_id as _),
            Some(obj2_id as _),
            &mut state,
        )
        .unwrap();
        tee_cryp_cipher_init(state, Some(iv), CipherPaddingMode::None).unwrap();
        aes_xts_test_set_user_base(state, 0);

        let mut out = alloc::vec![0u8; plain.len()];
        let first = chunk.min(plain.len());
        let mut off = tee_cryp_cipher_update(state, &plain[..first], &mut out[..]).unwrap();
        off += tee_cryp_cipher_final(state, &plain[first..], &mut out[off..]).unwrap();
        aes_xts_test_apply_patch(state, &mut out);
        core::assert_eq!(off, plain.len());
        out
    }

    #[unittest::def_test(custom)]
    fn test_aes_xts_host_split_block_aligned_decrypt() {
        let key1 = [0x11u8; 16];
        let key2 = [0x22u8; 16];
        let iv = [0x33u8; 16];

        let plain32 = alloc::vec![0x77u8; 32];
        let ct32 = aes_xts_reference_crypt(&key1, &key2, &iv, &plain32, false);
        let dec32 = aes_xts_reference_crypt_host_split(&key1, &key2, &iv, &ct32, true, 6);
        core::assert_eq!(dec32, plain32, "host split 32-byte decrypt");

        let plain512 = alloc::vec![0x66u8; 512];
        let ct512 = aes_xts_reference_crypt(&key1, &key2, &iv, &plain512, false);
        let dec512 = aes_xts_reference_crypt_host_split(&key1, &key2, &iv, &ct512, true, 8);
        core::assert_eq!(dec512, plain512, "host split 512-byte decrypt");
    }

    #[unittest::def_test(custom)]
    fn test_aes_xts_short_lengths_stealing() {
        let key1 = [0x11u8; 16];
        let key2 = [0x22u8; 16];
        let iv = [0x33u8; 16];

        for len in 17..=20 {
            let plain = alloc::vec![0x44u8; len];
            let expect = aes_xts_reference_crypt(&key1, &key2, &iv, &plain, false);
            let expect_buf = aes_xts_reference_crypt_buffered(&key1, &key2, &iv, &plain, false);
            core::assert_eq!(expect, expect_buf, "reference crypt vs buffered len={len}");

            let one_shot = aes_xts_tee_encrypt_incremental(&key1, &key2, &iv, &plain, len);
            core::assert_eq!(one_shot, expect, "one-shot encrypt len={len}");

            let incr6 = aes_xts_tee_encrypt_incremental(&key1, &key2, &iv, &plain, 6);
            core::assert_eq!(incr6, expect, "incr6 encrypt len={len}");

            let ct = aes_xts_reference_crypt(&key1, &key2, &iv, &plain, false);
            let dec = aes_xts_reference_crypt(&key1, &key2, &iv, &ct, true);
            core::assert_eq!(dec, plain, "reference roundtrip len={len}");

            let ct_tee = aes_xts_tee_encrypt_incremental(&key1, &key2, &iv, &plain, 6);
            let dec_tee = aes_xts_tee_decrypt_incremental(&key1, &key2, &iv, &ct_tee, 6);
            core::assert_eq!(dec_tee, plain, "tee roundtrip len={len}");
        }

        let plain32 = alloc::vec![0x55u8; 32];
        let ct32 = aes_xts_tee_encrypt_incremental(&key1, &key2, &iv, &plain32, 8);
        let dec32 = aes_xts_tee_decrypt_incremental(&key1, &key2, &iv, &ct32, 8);
        core::assert_eq!(dec32, plain32, "32-byte decrypt roundtrip");

        let plain512 = alloc::vec![0x66u8; 512];
        let ct512 = aes_xts_tee_encrypt_incremental(&key1, &key2, &iv, &plain512, 8);
        let dec512 = aes_xts_tee_decrypt_incremental(&key1, &key2, &iv, &ct512, 8);
        core::assert_eq!(dec512, plain512, "512-byte decrypt roundtrip");

        let plain32b = alloc::vec![0x77u8; 32];
        let ct32b = aes_xts_tee_encrypt_incremental(&key1, &key2, &iv, &plain32b, 6);
        let dec32b = aes_xts_tee_decrypt_incremental(&key1, &key2, &iv, &ct32b, 6);
        core::assert_eq!(dec32b, plain32b, "32-byte decrypt in_incr=6 roundtrip");
    }

    /// Host regression_4003: `cipher_update(in_incr)` then `cipher_do_final(rest)`.
    fn aes_xts_reference_crypt_host_split(
        key1: &[u8; 16],
        key2: &[u8; 16],
        iv: &[u8; 16],
        input: &[u8],
        decrypt: bool,
        in_incr: usize,
    ) -> alloc::vec::Vec<u8> {
        use crate::tee::crypto::aes_xts::{
            AesXtsStream, aes_xts_final_buffered, aes_xts_init, aes_xts_update_buffered,
        };

        let mut combined = alloc::vec::Vec::with_capacity(32);
        combined.extend_from_slice(key1);
        combined.extend_from_slice(key2);
        let mut xts = aes_xts_init(&combined, Some(iv), decrypt).unwrap();
        let mut out = alloc::vec![0u8; input.len()];
        let mut pending = [0u8; 32];
        let mut pending_len = 0usize;

        let first = in_incr.min(input.len());
        let stream_up = AesXtsStream {
            prior_bytes: 0,
            is_final: false,
        };
        let w = aes_xts_update_buffered(
            &mut xts,
            &mut pending,
            &mut pending_len,
            &input[..first],
            &mut out,
            &stream_up,
        )
        .unwrap();

        let stream_fin = AesXtsStream {
            prior_bytes: first,
            is_final: true,
        };
        let (n, patch) = aes_xts_final_buffered(
            &mut xts,
            &mut pending,
            &mut pending_len,
            &input[first..],
            &mut out[w..],
            &stream_fin,
        )
        .unwrap();
        if let Some(pb) = patch {
            if w >= 16 {
                out[w - 16..w].copy_from_slice(&pb);
            }
        }
        core::assert_eq!(w + n, input.len());
        out
    }

    fn aes_xts_tee_decrypt_incremental(
        key1: &[u8; 16],
        key2: &[u8; 16],
        iv: &[u8; 16],
        ct: &[u8],
        chunk: usize,
    ) -> alloc::vec::Vec<u8> {
        let (obj1_id, obj2_id) = aes_xts_test_setup_keys(key1, key2);
        let mut state: u32 = 0;
        tee_cryp_state_alloc(
            TEE_ALG_AES_XTS,
            TEE_OperationMode::TEE_MODE_DECRYPT,
            Some(obj1_id as _),
            Some(obj2_id as _),
            &mut state,
        )
        .unwrap();
        tee_cryp_cipher_init(state, Some(iv), CipherPaddingMode::None).unwrap();
        aes_xts_test_set_user_base(state, 0);

        let mut out = alloc::vec![0u8; ct.len()];
        let first = chunk.min(ct.len());
        let mut off = tee_cryp_cipher_update(state, &ct[..first], &mut out[..]).unwrap();
        off += tee_cryp_cipher_final(state, &ct[first..], &mut out[off..]).unwrap();
        aes_xts_test_apply_patch(state, &mut out);
        core::assert_eq!(off, ct.len());
        out
    }

    #[unittest::def_test(custom)]
    fn test_des3_ecb_rejects_invalid_key_length() {
        let key = [0u8; 8];
        let res = des3_ecb_encrypt_test_setup(&key);
        assert_eq!(res.err(), Some(TEE_ERROR_BAD_PARAMETERS));
    }

    #[unittest::def_test(custom)]
    fn test_cryp_aes_ccm_encrypt_decrypt() {
        let mut enc_state: u32 = 0;
        let mut dec_state: u32 = 0;
        let mut enc_obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        let mut dec_obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();

        let result = syscall_cryp_obj_alloc(TEE_TYPE_AES as _, 128, enc_obj_id.as_user_ref());
        assert!(result.is_ok());
        let enc_obj_id = enc_obj_id.read();

        let result = syscall_cryp_obj_alloc(TEE_TYPE_AES as _, 128, dec_obj_id.as_user_ref());
        assert!(result.is_ok());
        let dec_obj_id = dec_obj_id.read();

        let key = [
            0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x4b, 0x4c, 0x4d,
            0x4e, 0x4f,
        ];
        let nonce = [0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16];
        let aad = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07];
        let plain = [0x20, 0x21, 0x22, 0x23];
        let cipher_expect = [0x71, 0x62, 0x01, 0x5b];
        let tag_expect = [0x4d, 0xac, 0x25, 0x5d];

        let enc_obj = tee_obj_get(enc_obj_id as tee_obj_id_type);
        assert!(enc_obj.is_ok());
        let enc_obj = enc_obj.unwrap();
        let mut enc_obj_guard = enc_obj.lock();
        let mut enc_secret = tee_cryp_obj_secret_wrapper::new(32);
        enc_secret.set_secret_data(&key);
        let _ = core::mem::replace(
            &mut enc_obj_guard.attr[0],
            TeeCryptObj::obj_secret(enc_secret),
        );
        drop(enc_obj_guard);

        let dec_obj = tee_obj_get(dec_obj_id as tee_obj_id_type);
        assert!(dec_obj.is_ok());
        let dec_obj = dec_obj.unwrap();
        let mut dec_obj_guard = dec_obj.lock();
        let mut dec_secret = tee_cryp_obj_secret_wrapper::new(32);
        dec_secret.set_secret_data(&key);
        let _ = core::mem::replace(
            &mut dec_obj_guard.attr[0],
            TeeCryptObj::obj_secret(dec_secret),
        );
        drop(dec_obj_guard);

        let res = tee_cryp_state_alloc(
            TEE_ALG_AES_CCM,
            TEE_OperationMode::TEE_MODE_ENCRYPT,
            Some(enc_obj_id as _),
            None,
            &mut enc_state,
        );
        assert!(res.is_ok());
        let res = tee_cryp_state_alloc(
            TEE_ALG_AES_CCM,
            TEE_OperationMode::TEE_MODE_DECRYPT,
            Some(dec_obj_id as _),
            None,
            &mut dec_state,
        );
        assert!(res.is_ok());

        let res = tee_cryp_authenc_init(
            enc_state,
            &nonce,
            Some(aad.len()),
            Some(tag_expect.len()),
            Some(plain.len()),
        );
        assert!(res.is_ok());
        let res = tee_cryp_authenc_update_aad(enc_state, &aad);
        assert!(res.is_ok());

        let mut enc_out = [0u8; 20];
        let res = tee_cryp_authenc_update_payload(enc_state, &plain, &mut enc_out);
        assert!(res.is_ok());
        let enc_len = res.unwrap();
        assert_eq!(enc_len, plain.len());
        assert_eq!(&enc_out[..enc_len], &cipher_expect);

        let mut tag = [0u8; 4];
        let res = tee_cryp_authenc_enc_final(enc_state, None, &mut enc_out[enc_len..], &mut tag);
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), 0);
        assert_eq!(tag, tag_expect);

        let res = tee_cryp_authenc_init(
            dec_state,
            &nonce,
            Some(aad.len()),
            Some(tag_expect.len()),
            Some(cipher_expect.len()),
        );
        assert!(res.is_ok());
        let res = tee_cryp_authenc_update_aad(dec_state, &aad);
        assert!(res.is_ok());

        let mut dec_out = [0u8; 20];
        let res = tee_cryp_authenc_update_payload(dec_state, &cipher_expect, &mut dec_out);
        assert!(res.is_ok());
        let dec_len = res.unwrap();
        assert_eq!(dec_len, plain.len());
        assert_eq!(&dec_out[..dec_len], &plain);

        let res = tee_cryp_authenc_dec_final(dec_state, None, &mut dec_out[dec_len..], &tag_expect);
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), 0);
    }

    #[unittest::def_test(custom)]
    fn test_cryp_sm4_gcm_decrypt() {
        let mut state: u32 = 0;
        let mut obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        let result = syscall_cryp_obj_alloc(TEE_TYPE_SM4 as _, 128, obj_id.as_user_ref());
        assert!(result.is_ok());
        let obj_id = obj_id.read();

        // 随机生成密钥
        let result = syscall_obj_generate_key(obj_id as c_ulong, 128, core::ptr::null(), 0);
        assert!(result.is_ok());

        let obj_arc = tee_obj_get(obj_id as tee_obj_id_type);
        assert!(obj_arc.is_ok());
        let obj_arc = obj_arc.unwrap();
        let mut obj = obj_arc.lock();

        assert_eq!(obj.info.objectType, TEE_TYPE_SM4);
        assert_eq!(obj.info.maxObjectSize, 128);
        assert_eq!(obj.info.objectUsage, TEE_USAGE_DEFAULT);
        assert_eq!(obj.attr.len(), 1);
        assert!(matches!(obj.attr[0], TeeCryptObj::obj_secret(_)));

        let key: [u8; 16] = [
            0x69, 0xEE, 0xDF, 0x37, 0x77, 0xE5, 0x94, 0xC3, 0x0E, 0x94, 0xE9, 0xC5, 0xE2, 0xBC,
            0xE4, 0x67,
        ];
        let mut secret = tee_cryp_obj_secret_wrapper::new(32);
        secret.set_secret_data(&key);
        assert_eq!(secret.key(), key);

        // 赋值固定的key用于验证结果
        let _ = core::mem::replace(&mut obj.attr[0], TeeCryptObj::obj_secret(secret));
        drop(obj);

        let res = tee_cryp_state_alloc(
            TEE_ALG_SM4_GCM,
            TEE_OperationMode::TEE_MODE_DECRYPT,
            Some(obj_id as _),
            None,
            &mut state,
        );
        assert!(res.is_ok());

        let data: [u8; 64] = [
            0x0C, 0x29, 0xFC, 0x49, 0x07, 0x11, 0x9F, 0x99, 0xC4, 0x92, 0xE2, 0xFA, 0x7B, 0x63,
            0x3F, 0x4E, 0x16, 0x5B, 0xE5, 0x35, 0x85, 0xAB, 0xED, 0x71, 0x8B, 0xA3, 0x9C, 0xAB,
            0x80, 0xA0, 0x63, 0x92, 0x73, 0x1E, 0x5C, 0xE6, 0xE3, 0x58, 0x1D, 0xCA, 0xF1, 0x19,
            0x03, 0x7D, 0x99, 0x8A, 0x0F, 0x52, 0x2D, 0x68, 0x0A, 0x9D, 0xCB, 0x40, 0x5A, 0xAD,
            0xF8, 0x00, 0xC0, 0xC7, 0x98, 0xBA, 0xE3, 0x8A,
        ];
        let nonce: [u8; 12] = [
            0xA3, 0x33, 0x06, 0x38, 0xA8, 0x09, 0xBA, 0x35, 0x8D, 0x6C, 0x09, 0x8E,
        ];
        let ad: [u8; 20] = [
            0xFE, 0xED, 0xFA, 0xCE, 0xDE, 0xAD, 0xBE, 0xEF, 0xFE, 0xED, 0xFA, 0xCE, 0xDE, 0xAD,
            0xBE, 0xEF, 0xAB, 0xAD, 0xDA, 0xD2,
        ];
        let tag = [
            0x19, 0x7F, 0x6C, 0xC5, 0x52, 0x3D, 0xA3, 0x6A, 0x3B, 0x2C, 0x42, 0x92, 0x44, 0xC4,
            0x70, 0xAA,
        ];
        let mut out = [0u8; 80];
        let mut total_len = 0;

        let res = tee_cryp_authenc_init(state, &nonce, None, None, None);
        assert!(res.is_ok());

        let res = tee_cryp_authenc_update_aad(state, &ad);
        assert!(res.is_ok());

        let res = tee_cryp_authenc_update_payload(state, &data[..], &mut out[total_len..]);
        assert!(res.is_ok());
        total_len += res.unwrap();

        let res = tee_cryp_authenc_dec_final(state, None, &mut out[total_len..], &tag);
        assert!(res.is_ok());

        assert_eq!(total_len, 64);
        assert_eq!(
            &out[..64],
            [
                0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xBB, 0xBB, 0xBB, 0xBB, 0xBB, 0xBB,
                0xBB, 0xBB, 0xCC, 0xCC, 0xCC, 0xCC, 0xCC, 0xCC, 0xCC, 0xCC, 0xDD, 0xDD, 0xDD, 0xDD,
                0xDD, 0xDD, 0xDD, 0xDD, 0xEE, 0xEE, 0xEE, 0xEE, 0xEE, 0xEE, 0xEE, 0xEE, 0xFF, 0xFF,
                0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xEE, 0xEE, 0xEE, 0xEE, 0xEE, 0xEE, 0xEE, 0xEE,
                0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA
            ]
        );
    }

    #[unittest::def_test(custom)]
    fn test_cryp_sm2_sign_verify() {
        // alloc sm2 key pair
        let mut obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        let res = syscall_cryp_obj_alloc(TEE_TYPE_SM2_DSA_KEYPAIR as _, 256, obj_id.as_user_ref());
        assert!(res.is_ok());
        let obj_id = obj_id.read();
        // sm2 no need usr_params
        let res = syscall_obj_generate_key(obj_id as c_ulong, 256, core::ptr::null(), 0);
        assert!(res.is_ok());
        // get attr from obj
        let obj_arc = tee_obj_get(obj_id as tee_obj_id_type);
        assert!(obj_arc.is_ok());
        let obj_arc = obj_arc.unwrap();
        let obj = obj_arc.lock();
        assert_eq!(obj.info.objectType, TEE_TYPE_SM2_DSA_KEYPAIR);
        assert_eq!(obj.info.maxObjectSize, 256);
        assert_eq!(obj.info.objectUsage, TEE_USAGE_DEFAULT);
        assert_eq!(obj.attr.len(), 1);
        assert!(matches!(obj.attr[0], TeeCryptObj::ecc_keypair(_)));
        drop(obj);

        let mut obj_id_pub = TestUserValue::<c_uint>::from_value(0).unwrap();
        let res = syscall_cryp_obj_alloc(
            TEE_TYPE_SM2_DSA_PUBLIC_KEY as _,
            256,
            obj_id_pub.as_user_ref(),
        );
        assert!(res.is_ok());
        let obj_id_pub = obj_id_pub.read();

        let res = syscall_cryp_obj_copy(obj_id_pub as _, obj_id as _);
        assert!(res.is_ok());

        let mut state: u32 = 0;
        let res = tee_cryp_state_alloc(
            TEE_ALG_SM2_DSA_SM3,
            TEE_OperationMode::TEE_MODE_SIGN,
            Some(obj_id as _),
            None,
            &mut state,
        );
        assert!(res.is_ok());

        let mut state_pub: u32 = 0;
        let res = tee_cryp_state_alloc(
            TEE_ALG_SM2_DSA_SM3,
            TEE_OperationMode::TEE_MODE_VERIFY,
            Some(obj_id_pub as _),
            None,
            &mut state_pub,
        );
        assert!(res.is_ok());

        let data = b"SIGNATURE TEST SIGNATURE TEST SI";

        let mut hash_state: u32 = 0;
        tee_cryp_state_alloc(
            TEE_ALG_SM3,
            TEE_OperationMode::TEE_MODE_DIGEST,
            None,
            None,
            &mut hash_state,
        )
        .unwrap();
        tee_cryp_hash_init(hash_state).unwrap();
        tee_cryp_hash_update(hash_state, data).unwrap();
        let mut digest = [0u8; 32];
        let digest_len = tee_cryp_hash_final(hash_state, &[], &mut digest).unwrap();
        let _ = tee_cryp_state_free(hash_state);

        let mut signature1 = [0u8; 64];
        let len =
            tee_cryp_asymm_operate(state, &digest[..digest_len], &mut signature1, None).unwrap();
        assert_eq!(len, 64);

        tee_cryp_asymm_verify(state_pub, &digest[..digest_len], &signature1[..len]).unwrap();
    }

    #[unittest::def_test(custom)]
    fn test_cryp_sm2_verify_with_pub_key() {
        // alloc sm2 key pair
        let mut obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        let res =
            syscall_cryp_obj_alloc(TEE_TYPE_SM2_DSA_PUBLIC_KEY as _, 256, obj_id.as_user_ref());
        assert!(res.is_ok());
        let obj_id = obj_id.read();

        // get attr from obj
        let obj_arc = tee_obj_get(obj_id as tee_obj_id_type);
        assert!(obj_arc.is_ok());
        let obj_arc = obj_arc.unwrap();
        let mut obj = obj_arc.lock();
        assert_eq!(obj.info.objectType, TEE_TYPE_SM2_DSA_PUBLIC_KEY);
        assert_eq!(obj.info.maxObjectSize, 256);
        assert_eq!(obj.info.objectUsage, TEE_USAGE_DEFAULT);
        assert_eq!(obj.attr.len(), 1);
        assert!(matches!(obj.attr[0], TeeCryptObj::ecc_public_key(_)));
        drop(obj);

        let ptx: [u8; 46] = [
            0xB2, 0xE1, 0x4C, 0x5C, 0x79, 0xC6, 0xDF, 0x5B, 0x85, 0xF4, 0xFE, 0x7E, 0xD8, 0xDB,
            0x7A, 0x26, 0x2B, 0x9D, 0xA7, 0xE0, 0x7C, 0xCB, 0x0E, 0xA9, 0xF4, 0x74, 0x7B, 0x8C,
            0xCD, 0xA8, 0xA4, 0xF3, 0x6D, 0x65, 0x73, 0x73, 0x61, 0x67, 0x65, 0x20, 0x64, 0x69,
            0x67, 0x65, 0x73, 0x74,
        ];
        const SIG: [u8; 64] = [
            0xF5, 0xA0, 0x3B, 0x06, 0x48, 0xD2, 0xC4, 0x63, 0x0E, 0xEA, 0xC5, 0x13, 0xE1, 0xBB,
            0x81, 0xA1, 0x59, 0x44, 0xDA, 0x38, 0x27, 0xD5, 0xB7, 0x41, 0x43, 0xAC, 0x7E, 0xAC,
            0xEE, 0xE7, 0x20, 0xB3, 0xB1, 0xB6, 0xAA, 0x29, 0xDF, 0x21, 0x2F, 0xD8, 0x76, 0x31,
            0x82, 0xBC, 0x0D, 0x42, 0x1C, 0xA1, 0xBB, 0x90, 0x38, 0xFD, 0x1F, 0x7F, 0x42, 0xD4,
            0x84, 0x0B, 0x69, 0xC4, 0x85, 0xBB, 0xC1, 0xAA,
        ];
        const PUB_X: [u8; 32] = [
            0x09, 0xF9, 0xDF, 0x31, 0x1E, 0x54, 0x21, 0xA1, 0x50, 0xDD, 0x7D, 0x16, 0x1E, 0x4B,
            0xC5, 0xC6, 0x72, 0x17, 0x9F, 0xAD, 0x18, 0x33, 0xFC, 0x07, 0x6B, 0xB0, 0x8F, 0xF3,
            0x56, 0xF3, 0x50, 0x20,
        ];
        const PUB_Y: [u8; 32] = [
            0xCC, 0xEA, 0x49, 0x0C, 0xE2, 0x67, 0x75, 0xA5, 0x2D, 0xC6, 0xEA, 0x71, 0x8C, 0xC1,
            0xAA, 0x60, 0x0A, 0xED, 0x05, 0xFB, 0xF3, 0x5E, 0x08, 0x4A, 0x66, 0x32, 0xF6, 0x07,
            0x2D, 0xA9, 0xAD, 0x13,
        ];

        let mut hash_state: u32 = 0;
        tee_cryp_state_alloc(
            TEE_ALG_SM3,
            TEE_OperationMode::TEE_MODE_DIGEST,
            None,
            None,
            &mut hash_state,
        )
        .unwrap();
        tee_cryp_hash_init(hash_state).unwrap();
        tee_cryp_hash_update(hash_state, &ptx).unwrap();
        let mut digest = [0u8; 32];
        let digest_len = tee_cryp_hash_final(hash_state, &[], &mut digest).unwrap();
        let _ = tee_cryp_state_free(hash_state);

        let mut usr_x = crate::user_vec![0u8; 32];
        let mut usr_y = crate::user_vec![0u8; 32];
        usr_x.copy_from_slice(&PUB_X);
        usr_y.copy_from_slice(&PUB_Y);

        let mut usr_attrs = crate::user_vec![utee_attribute::default(); 2];
        tee_init_ref_attribute(
            &mut usr_attrs[0],
            TEE_ATTR_ECC_PUBLIC_VALUE_X,
            &usr_x[..],
            32,
        );
        tee_init_ref_attribute(
            &mut usr_attrs[1],
            TEE_ATTR_ECC_PUBLIC_VALUE_Y,
            &usr_y[..],
            32,
        );
        syscall_cryp_obj_populate(
            obj_id as c_ulong,
            usr_attrs.as_mut_ptr(),
            usr_attrs.len() as c_ulong,
        )
        .unwrap();

        let mut state: u32 = 0;
        tee_cryp_state_alloc(
            TEE_ALG_SM2_DSA_SM3,
            TEE_OperationMode::TEE_MODE_VERIFY,
            Some(obj_id as _),
            None,
            &mut state,
        )
        .unwrap();

        tee_cryp_asymm_verify(state, &digest[..digest_len], &SIG).unwrap();
    }

    /// 与 `test_cryp_sm2_verify_with_pub_key` 相同数据与验签结论，公钥通过 `tee_init_ref_attribute` +
    /// `syscall_cryp_obj_populate` 写入（对齐 TA：`TEE_InitRefAttribute` + `TEE_PopulateTransientObject`）。
    #[unittest::def_test(custom)]
    fn test_cryp_sm2_verify_with_pub_key_via_init_ref_attr() {
        let mut obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        syscall_cryp_obj_alloc(TEE_TYPE_SM2_DSA_PUBLIC_KEY as _, 256, obj_id.as_user_ref())
            .unwrap();
        let obj_id = obj_id.read();

        let ptx: [u8; 46] = [
            0xB2, 0xE1, 0x4C, 0x5C, 0x79, 0xC6, 0xDF, 0x5B, 0x85, 0xF4, 0xFE, 0x7E, 0xD8, 0xDB,
            0x7A, 0x26, 0x2B, 0x9D, 0xA7, 0xE0, 0x7C, 0xCB, 0x0E, 0xA9, 0xF4, 0x74, 0x7B, 0x8C,
            0xCD, 0xA8, 0xA4, 0xF3, 0x6D, 0x65, 0x73, 0x73, 0x61, 0x67, 0x65, 0x20, 0x64, 0x69,
            0x67, 0x65, 0x73, 0x74,
        ];
        const SIG: [u8; 64] = [
            0xF5, 0xA0, 0x3B, 0x06, 0x48, 0xD2, 0xC4, 0x63, 0x0E, 0xEA, 0xC5, 0x13, 0xE1, 0xBB,
            0x81, 0xA1, 0x59, 0x44, 0xDA, 0x38, 0x27, 0xD5, 0xB7, 0x41, 0x43, 0xAC, 0x7E, 0xAC,
            0xEE, 0xE7, 0x20, 0xB3, 0xB1, 0xB6, 0xAA, 0x29, 0xDF, 0x21, 0x2F, 0xD8, 0x76, 0x31,
            0x82, 0xBC, 0x0D, 0x42, 0x1C, 0xA1, 0xBB, 0x90, 0x38, 0xFD, 0x1F, 0x7F, 0x42, 0xD4,
            0x84, 0x0B, 0x69, 0xC4, 0x85, 0xBB, 0xC1, 0xAA,
        ];
        const PUB_X: [u8; 32] = [
            0x09, 0xF9, 0xDF, 0x31, 0x1E, 0x54, 0x21, 0xA1, 0x50, 0xDD, 0x7D, 0x16, 0x1E, 0x4B,
            0xC5, 0xC6, 0x72, 0x17, 0x9F, 0xAD, 0x18, 0x33, 0xFC, 0x07, 0x6B, 0xB0, 0x8F, 0xF3,
            0x56, 0xF3, 0x50, 0x20,
        ];
        const PUB_Y: [u8; 32] = [
            0xCC, 0xEA, 0x49, 0x0C, 0xE2, 0x67, 0x75, 0xA5, 0x2D, 0xC6, 0xEA, 0x71, 0x8C, 0xC1,
            0xAA, 0x60, 0x0A, 0xED, 0x05, 0xFB, 0xF3, 0x5E, 0x08, 0x4A, 0x66, 0x32, 0xF6, 0x07,
            0x2D, 0xA9, 0xAD, 0x13,
        ];

        let mut hash_state: u32 = 0;
        tee_cryp_state_alloc(
            TEE_ALG_SM3,
            TEE_OperationMode::TEE_MODE_DIGEST,
            None,
            None,
            &mut hash_state,
        )
        .unwrap();
        tee_cryp_hash_init(hash_state).unwrap();
        tee_cryp_hash_update(hash_state, &ptx).unwrap();
        let mut digest = [0u8; 32];
        let digest_len = tee_cryp_hash_final(hash_state, &[], &mut digest).unwrap();
        let _ = tee_cryp_state_free(hash_state);

        let mut usr_x = crate::user_vec![0u8; 32];
        let mut usr_y = crate::user_vec![0u8; 32];
        usr_x.copy_from_slice(&PUB_X);
        usr_y.copy_from_slice(&PUB_Y);

        let mut usr_attrs = crate::user_vec![utee_attribute::default(); 2];
        tee_init_ref_attribute(
            &mut usr_attrs[0],
            TEE_ATTR_ECC_PUBLIC_VALUE_X,
            &usr_x[..],
            32,
        );
        tee_init_ref_attribute(
            &mut usr_attrs[1],
            TEE_ATTR_ECC_PUBLIC_VALUE_Y,
            &usr_y[..],
            32,
        );

        syscall_cryp_obj_populate(
            obj_id as c_ulong,
            usr_attrs.as_mut_ptr(),
            usr_attrs.len() as c_ulong,
        )
        .unwrap();

        let mut state: u32 = 0;
        tee_cryp_state_alloc(
            TEE_ALG_SM2_DSA_SM3,
            TEE_OperationMode::TEE_MODE_VERIFY,
            Some(obj_id as _),
            None,
            &mut state,
        )
        .unwrap();

        tee_cryp_asymm_verify(state, &digest[..digest_len], &SIG).unwrap();
    }

    #[unittest::def_test(custom)]
    fn test_cryp_sm2_enc_dec() {
        // alloc sm2 key pair
        let mut obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        let res = syscall_cryp_obj_alloc(TEE_TYPE_SM2_PKE_KEYPAIR as _, 256, obj_id.as_user_ref());
        assert!(res.is_ok());
        let obj_id = obj_id.read();
        // sm2 no need usr_params
        let res = syscall_obj_generate_key(obj_id as c_ulong, 256, core::ptr::null(), 0);
        assert!(res.is_ok());
        // get attr from obj
        let obj_arc = tee_obj_get(obj_id as tee_obj_id_type);
        assert!(obj_arc.is_ok());
        let obj_arc = obj_arc.unwrap();
        let obj = obj_arc.lock();
        assert_eq!(obj.info.objectType, TEE_TYPE_SM2_PKE_KEYPAIR);
        assert_eq!(obj.info.maxObjectSize, 256);
        assert_eq!(obj.info.objectUsage, TEE_USAGE_DEFAULT);
        assert_eq!(obj.attr.len(), 1);
        assert!(matches!(obj.attr[0], TeeCryptObj::ecc_keypair(_)));
        drop(obj);

        let mut obj_id_pub = TestUserValue::<c_uint>::from_value(0).unwrap();
        let res = syscall_cryp_obj_alloc(
            TEE_TYPE_SM2_PKE_PUBLIC_KEY as _,
            256,
            obj_id_pub.as_user_ref(),
        );
        assert!(res.is_ok());
        let obj_id_pub = obj_id_pub.read();

        let res = syscall_cryp_obj_copy(obj_id_pub as _, obj_id as _);
        assert!(res.is_ok());

        let mut state_enc: u32 = 0;
        let res = tee_cryp_state_alloc(
            TEE_ALG_SM2_PKE,
            TEE_OperationMode::TEE_MODE_ENCRYPT,
            Some(obj_id_pub as _),
            None,
            &mut state_enc,
        );
        assert!(res.is_ok());

        let mut state_dec: u32 = 0;
        let res = tee_cryp_state_alloc(
            TEE_ALG_SM2_PKE,
            TEE_OperationMode::TEE_MODE_DECRYPT,
            Some(obj_id as _),
            None,
            &mut state_dec,
        );
        assert!(res.is_ok());

        let data = b"SIGNATURE TEST SIGNATURE TEST SI";
        let mut cipher1 = [0u8; 141];
        let mut cipher2 = [0u8; 141];
        let mut clear1 = [0u8; 141];
        let mut clear2 = [0u8; 141];

        let res = tee_cryp_asymm_operate(state_enc, data, &mut cipher1, None);
        assert!(res.is_ok());
        let mut len1 = res.unwrap();

        let res = tee_cryp_asymm_operate(state_enc, data, &mut cipher2, None);
        assert!(res.is_ok());
        let mut len2 = res.unwrap();

        assert_ne!(&cipher1[..len1], &cipher2[..len2]);

        let res = tee_cryp_asymm_operate(state_dec, &cipher1[..len1], &mut clear1, None);
        assert!(res.is_ok());
        let len3 = res.unwrap();

        let res = tee_cryp_asymm_operate(state_dec, &cipher2[..len2], &mut clear2, None);
        assert!(res.is_ok());
        let len4 = res.unwrap();

        assert_eq!(&clear1[..len3], &clear2[..len4]);
        assert_eq!(&clear1[..len3], data);
    }

    const RSA_TEST_DER: &[u8] = &[
        0x30, 0x82, 0x04, 0xa3, 0x02, 0x01, 0x00, 0x02, 0x82, 0x01, 0x01, 0x00, 0x8d, 0x76, 0xa1,
        0x2e, 0xb6, 0xc0, 0xe5, 0x1e, 0x1a, 0x06, 0x74, 0x13, 0x57, 0x6a, 0xc2, 0x6c, 0x02, 0x9d,
        0x82, 0x91, 0x5b, 0xb0, 0xe5, 0xa9, 0x7f, 0xe0, 0x6d, 0x3f, 0xc0, 0x94, 0x88, 0x8e, 0x72,
        0xd4, 0x4a, 0xc1, 0xf5, 0x54, 0x71, 0x63, 0x10, 0xaa, 0xef, 0x9d, 0xa5, 0x1a, 0xdc, 0x00,
        0x82, 0x2d, 0xea, 0x5f, 0x5b, 0xe8, 0x73, 0x6e, 0x03, 0xf8, 0x07, 0x90, 0x8c, 0xd5, 0x52,
        0xf5, 0x6d, 0xfc, 0x4d, 0xe5, 0x6a, 0x87, 0x5a, 0x85, 0xf7, 0x34, 0x85, 0x9a, 0x19, 0x3a,
        0x74, 0x46, 0x1e, 0xcb, 0x30, 0x77, 0x8d, 0x68, 0x8a, 0xb8, 0xfd, 0x6e, 0xbc, 0xee, 0xd2,
        0xd0, 0xb3, 0xd0, 0x1c, 0x44, 0x29, 0xd0, 0xd6, 0x91, 0xb5, 0xa8, 0xc1, 0xe3, 0x88, 0x64,
        0x40, 0x16, 0x31, 0x6c, 0xdc, 0x4b, 0xba, 0x69, 0xc3, 0xcd, 0x8d, 0x4a, 0xd8, 0x7d, 0xf4,
        0xa7, 0xe2, 0xe8, 0xc5, 0x01, 0x6f, 0xcc, 0x91, 0x22, 0x81, 0x52, 0x83, 0x11, 0x28, 0xb3,
        0x97, 0x1d, 0x57, 0xa2, 0x2a, 0x01, 0x77, 0x65, 0x87, 0x3e, 0xdc, 0x6c, 0x7f, 0x0a, 0xca,
        0x95, 0x04, 0x6a, 0x4e, 0x47, 0xa4, 0xfb, 0xa1, 0x42, 0x19, 0x0f, 0x80, 0x14, 0xed, 0xf9,
        0x4a, 0x42, 0x9c, 0x6f, 0xef, 0x0f, 0x82, 0x51, 0xbb, 0x46, 0x66, 0xc6, 0xfd, 0xd9, 0x01,
        0x93, 0x6d, 0xda, 0x36, 0xc7, 0x58, 0x37, 0x4b, 0xa7, 0xdb, 0xbd, 0xb2, 0x6f, 0x5b, 0x33,
        0x4b, 0x78, 0x70, 0x7e, 0xe8, 0x02, 0xdd, 0x5f, 0xa4, 0x2f, 0xea, 0x3c, 0x6b, 0xfb, 0x51,
        0xe1, 0x19, 0x21, 0x9f, 0x52, 0xd6, 0x29, 0x53, 0x09, 0x98, 0xbc, 0x3e, 0x3b, 0xb3, 0xdc,
        0x25, 0x13, 0x36, 0x1b, 0x24, 0xf4, 0x33, 0xdd, 0xdf, 0xa8, 0xd6, 0xe8, 0x97, 0x11, 0x2f,
        0x9a, 0x81, 0xc1, 0xb6, 0xf1, 0x7b, 0xa5, 0xa4, 0x2c, 0xda, 0x41, 0xb6, 0x11, 0x02, 0x03,
        0x01, 0x00, 0x01, 0x02, 0x82, 0x01, 0x00, 0x38, 0x98, 0xb9, 0xab, 0xe2, 0xda, 0x11, 0xd0,
        0x95, 0x40, 0xf7, 0xb7, 0xb5, 0x45, 0xb5, 0x3b, 0x59, 0x60, 0x83, 0x18, 0x7c, 0xc2, 0xad,
        0x5f, 0xbf, 0x15, 0x9f, 0x1f, 0xde, 0x80, 0x8e, 0x91, 0xcf, 0x47, 0x38, 0x11, 0x99, 0x81,
        0x8b, 0x4b, 0xc3, 0x23, 0x60, 0x72, 0x85, 0xd7, 0xd5, 0x25, 0x2e, 0xf0, 0x07, 0xd0, 0xd7,
        0x08, 0x8d, 0x05, 0xfa, 0xf8, 0x84, 0xae, 0x44, 0x6a, 0x24, 0xa2, 0xa4, 0xba, 0x48, 0xbf,
        0xfc, 0x7a, 0xe2, 0xb0, 0xae, 0x52, 0x89, 0x11, 0x39, 0xfe, 0xb4, 0xfe, 0x48, 0xdb, 0xaa,
        0x2c, 0x6a, 0x9a, 0xe4, 0xc5, 0x56, 0x3f, 0xb3, 0xbf, 0x29, 0x00, 0xee, 0xaf, 0xd8, 0x5f,
        0x3d, 0x0b, 0x9c, 0x8c, 0xf7, 0x4c, 0xe9, 0x25, 0x8b, 0x2f, 0xf0, 0xa3, 0xf0, 0x6a, 0x49,
        0x48, 0xd2, 0xef, 0xf5, 0xb2, 0x8b, 0x50, 0xe2, 0x84, 0xa2, 0x19, 0x79, 0x22, 0xff, 0x8e,
        0x16, 0xbe, 0x00, 0x70, 0xc4, 0x6d, 0xd0, 0x29, 0x54, 0x28, 0x99, 0x97, 0x84, 0xc9, 0xaf,
        0xd8, 0xb6, 0xb1, 0x44, 0x6d, 0x4a, 0x74, 0x82, 0x4e, 0xde, 0x44, 0x1c, 0x47, 0x11, 0x52,
        0x86, 0x48, 0xd7, 0x78, 0x52, 0xa9, 0x98, 0x20, 0x9d, 0x83, 0x39, 0x3d, 0xe5, 0xd6, 0xed,
        0x94, 0x6a, 0x67, 0xd0, 0x65, 0x23, 0xf6, 0xdd, 0xe1, 0xe3, 0xed, 0xe9, 0x6b, 0x85, 0xcb,
        0x91, 0x0b, 0xcd, 0xc4, 0x6b, 0xe4, 0x90, 0xd4, 0xeb, 0x7b, 0x80, 0x0b, 0x67, 0x9d, 0xb5,
        0x37, 0x0b, 0x83, 0x7d, 0x79, 0x45, 0x6b, 0x60, 0x7d, 0x6f, 0xe3, 0xe0, 0x5e, 0x92, 0xf6,
        0x13, 0x67, 0xd2, 0xd4, 0xdc, 0x43, 0x5f, 0xd8, 0xee, 0xf5, 0x28, 0x05, 0x64, 0x78, 0x6a,
        0x6f, 0xaf, 0xef, 0x64, 0x52, 0x93, 0x70, 0x4f, 0x9a, 0xab, 0xce, 0x4a, 0x51, 0x63, 0x2a,
        0xf1, 0x33, 0xfd, 0xd8, 0x1e, 0xf9, 0xef, 0xf1, 0x02, 0x81, 0x81, 0x00, 0xcf, 0xa7, 0x89,
        0x75, 0xdd, 0x09, 0x66, 0x8b, 0x4e, 0xda, 0x52, 0x38, 0x4a, 0xc3, 0x7c, 0xca, 0x90, 0x68,
        0x4a, 0xbb, 0x78, 0x14, 0xc1, 0x83, 0x24, 0xb2, 0x2e, 0x39, 0x20, 0x8a, 0x00, 0x97, 0x8d,
        0xf3, 0x21, 0x5a, 0xad, 0x03, 0xc7, 0xb2, 0xe9, 0x17, 0x10, 0x85, 0x63, 0x23, 0xe3, 0xc9,
        0x73, 0x91, 0xa8, 0x5a, 0x8d, 0xb6, 0x40, 0x0f, 0x98, 0xb8, 0x2a, 0x8f, 0x7e, 0x59, 0x80,
        0x8a, 0xee, 0xb9, 0xe9, 0x9b, 0x2e, 0x83, 0xd4, 0x85, 0xc1, 0xdc, 0x1e, 0xc9, 0x44, 0x48,
        0x2a, 0x13, 0x06, 0x09, 0x02, 0x3e, 0x3f, 0xfb, 0xf2, 0xe8, 0x1a, 0x2d, 0xec, 0x40, 0xea,
        0x0e, 0x2b, 0x7f, 0xf3, 0x79, 0xdc, 0x11, 0x3b, 0x0d, 0xb8, 0x3f, 0x4f, 0x06, 0x02, 0x17,
        0x7c, 0x79, 0xa7, 0x36, 0x56, 0xef, 0xcd, 0x1a, 0x41, 0x00, 0x2c, 0xe8, 0x2e, 0x55, 0x9b,
        0x10, 0xea, 0x19, 0xb2, 0xe3, 0x02, 0x81, 0x81, 0x00, 0xae, 0x66, 0x06, 0x29, 0xcd, 0x44,
        0x6b, 0x4d, 0xb0, 0x1e, 0xba, 0xb8, 0x4f, 0x5e, 0x06, 0xaa, 0x02, 0x58, 0xc9, 0xb5, 0x46,
        0x68, 0xe0, 0xaf, 0x48, 0x48, 0x82, 0x45, 0xd2, 0x9c, 0xa5, 0x2d, 0x9d, 0xe6, 0x7a, 0x16,
        0xe6, 0xba, 0x8c, 0xe9, 0x2b, 0x61, 0xaf, 0x40, 0x8c, 0xab, 0x38, 0x17, 0x4e, 0xe1, 0xf7,
        0x0d, 0x52, 0xb8, 0x78, 0xcc, 0x4d, 0xcb, 0xdc, 0xe4, 0xb7, 0x4f, 0x41, 0xdf, 0xde, 0x34,
        0x20, 0x5f, 0xac, 0x45, 0x6f, 0xed, 0xcd, 0xc0, 0x4d, 0x88, 0x7a, 0xf4, 0xc9, 0x8a, 0xa4,
        0xf7, 0x40, 0x41, 0x4d, 0xb6, 0x98, 0x1f, 0x2a, 0x42, 0x42, 0x62, 0xd2, 0xb1, 0xef, 0x84,
        0x94, 0x87, 0x09, 0xfe, 0xf1, 0xba, 0xb2, 0xb8, 0x6c, 0x99, 0xb2, 0x77, 0xa6, 0xd8, 0x91,
        0x07, 0xb5, 0xd9, 0x7d, 0xe8, 0x59, 0xc0, 0xfa, 0x5a, 0x55, 0xf4, 0x3a, 0x82, 0xf4, 0x78,
        0xa1, 0x7b, 0x02, 0x81, 0x80, 0x3f, 0x6e, 0xfa, 0x7a, 0xda, 0xce, 0xe8, 0x58, 0x5d, 0xfa,
        0x2b, 0x6b, 0xae, 0xcb, 0x10, 0xf0, 0x00, 0x35, 0x1b, 0xbf, 0x30, 0xeb, 0x86, 0x41, 0xbd,
        0x90, 0x00, 0xb6, 0xca, 0xcd, 0xdd, 0x68, 0x6e, 0xa0, 0x7a, 0xeb, 0xec, 0x36, 0x5f, 0x66,
        0xb3, 0xf5, 0xab, 0xc2, 0x53, 0x8a, 0xbf, 0x26, 0xe6, 0xfa, 0xf3, 0xe6, 0xd5, 0xab, 0x7a,
        0xde, 0x48, 0xd4, 0xd9, 0x8b, 0x84, 0x19, 0x6b, 0x3f, 0x05, 0xb6, 0x1d, 0x3a, 0x9e, 0x76,
        0xff, 0x10, 0xed, 0x2b, 0x84, 0xec, 0x0e, 0xc3, 0xcc, 0xb6, 0x8a, 0xfd, 0x6d, 0x85, 0xfe,
        0x9d, 0xc4, 0x92, 0x4a, 0x8d, 0x04, 0xc2, 0xbf, 0xbd, 0x1c, 0x64, 0xb5, 0xc7, 0xe0, 0x06,
        0x13, 0x78, 0x19, 0x74, 0x9d, 0x7b, 0x44, 0x60, 0x50, 0x52, 0x09, 0x56, 0x7c, 0x30, 0x3d,
        0x03, 0x6c, 0x1f, 0xd5, 0x98, 0x07, 0xaf, 0x76, 0xf3, 0x2f, 0xd0, 0x31, 0xe9, 0x02, 0x81,
        0x81, 0x00, 0xa6, 0x61, 0x77, 0x67, 0xd2, 0x09, 0x80, 0x45, 0xb1, 0xcc, 0xdf, 0x5e, 0x8f,
        0x79, 0xa8, 0xe9, 0xf1, 0x2b, 0x3b, 0xe4, 0xd1, 0xb3, 0xa5, 0x08, 0x14, 0xf1, 0xf8, 0x37,
        0x1c, 0xe3, 0x8d, 0x42, 0xa3, 0xee, 0x0a, 0x74, 0x66, 0xd3, 0x7b, 0x33, 0xc8, 0xcb, 0x7d,
        0x23, 0x1c, 0x11, 0x0d, 0x86, 0x4f, 0x1f, 0x8d, 0x4f, 0x0c, 0xa8, 0x29, 0xb6, 0xe0, 0x51,
        0xaa, 0x00, 0x1a, 0x52, 0x67, 0x0a, 0x69, 0x37, 0x59, 0xdb, 0x6c, 0xc3, 0x22, 0x31, 0xc1,
        0xa5, 0xc1, 0x52, 0x7f, 0xdb, 0xa1, 0x9b, 0xc0, 0x1e, 0x93, 0x12, 0xba, 0x4d, 0x85, 0x7b,
        0xd6, 0x19, 0x38, 0xb4, 0x87, 0x46, 0x72, 0xb8, 0x0d, 0xeb, 0x77, 0x41, 0xde, 0xe4, 0xbb,
        0x34, 0xef, 0x87, 0x02, 0x98, 0xdc, 0x78, 0xa8, 0x84, 0xae, 0x9d, 0x3c, 0x5d, 0xbb, 0xa3,
        0x3c, 0x35, 0x8a, 0xe3, 0x62, 0x1f, 0x25, 0x95, 0x20, 0x99, 0x02, 0x81, 0x80, 0x5b, 0xfb,
        0x99, 0x65, 0xaa, 0x0d, 0x55, 0xf5, 0x66, 0x27, 0x95, 0xc8, 0xb2, 0x68, 0x7f, 0x8b, 0xd3,
        0x26, 0xd1, 0x51, 0x68, 0xe3, 0x5f, 0x84, 0x1b, 0x13, 0xbf, 0xec, 0xb4, 0x92, 0x09, 0xa8,
        0x0c, 0xac, 0x5f, 0x99, 0x3a, 0xd5, 0xda, 0xdd, 0xee, 0xba, 0x1c, 0xce, 0x92, 0x7c, 0x54,
        0xd4, 0xf8, 0x6a, 0xc3, 0xb3, 0x07, 0xea, 0xce, 0x18, 0xad, 0x8e, 0x26, 0x5e, 0x54, 0xa1,
        0x87, 0x77, 0x6a, 0x7b, 0x23, 0x2e, 0x76, 0xb6, 0x3a, 0xe7, 0xd9, 0x67, 0x0d, 0x7e, 0x19,
        0xd9, 0x6e, 0x2c, 0xe0, 0x00, 0xd6, 0x8e, 0xd2, 0x5a, 0xc9, 0x59, 0x44, 0x58, 0xd8, 0x73,
        0x15, 0x0f, 0x17, 0x63, 0x3e, 0xef, 0x74, 0x2f, 0xfe, 0xbd, 0x50, 0x07, 0x5f, 0x7d, 0x15,
        0x23, 0xab, 0xc2, 0x77, 0x6d, 0xc9, 0x3d, 0x08, 0x1a, 0x88, 0xdd, 0x45, 0x26, 0xd9, 0x2d,
        0xe9, 0xde, 0xb9, 0x58, 0x36, 0x5f,
    ];

    /// 与 mbedtls `Pk::from_private_key(TEST_DER, …)` 相同思路：从 PKCS#8 DER 注入 `rsa_keypair`，供 OAEP/PSS 等路径使用。
    fn rsa_keypair_from_pkcs8_der(der: &'static [u8]) -> rsa_keypair {
        let res = Pk::from_private_key(der, None);
        ::core::assert!(res.is_ok());
        let pk = res.unwrap();
        let res = rsa_keypair::new(TEE_TYPE_RSA_KEYPAIR, 2048);
        ::core::assert!(res.is_ok());
        let mut kp = res.unwrap();
        let res = pk.rsa_public_modulus();
        ::core::assert!(res.is_ok());
        kp.n = BigNum::from_mpi(res.unwrap());
        let res = pk.rsa_public_exponent();
        ::core::assert!(res.is_ok());
        let exp = res.unwrap();
        let res = Mpi::new(exp as i64);
        ::core::assert!(res.is_ok());
        kp.e = BigNum::from_mpi(res.unwrap());
        let res = pk.rsa_private_exponent();
        ::core::assert!(res.is_ok());
        kp.d = BigNum::from_mpi(res.unwrap());
        let res = pk.rsa_private_prime1();
        ::core::assert!(res.is_ok());
        kp.p = BigNum::from_mpi(res.unwrap());
        let res = pk.rsa_private_prime2();
        ::core::assert!(res.is_ok());
        kp.q = BigNum::from_mpi(res.unwrap());
        let res = pk.rsa_crt_dp();
        ::core::assert!(res.is_ok());
        kp.dp = BigNum::from_mpi(res.unwrap());
        let res = pk.rsa_crt_dq();
        ::core::assert!(res.is_ok());
        kp.dq = BigNum::from_mpi(res.unwrap());
        let res = pk.rsa_crt_qp();
        ::core::assert!(res.is_ok());
        kp.qp = BigNum::from_mpi(res.unwrap());
        kp
    }

    fn install_rsa_test_der_key_objects() -> (u32, u32) {
        let res = TestUserValue::<c_uint>::from_value(0);
        ::core::assert!(res.is_ok());
        let mut kp_obj = res.unwrap();
        let res = syscall_cryp_obj_alloc(TEE_TYPE_RSA_KEYPAIR as _, 2048, kp_obj.as_user_ref());
        ::core::assert!(res.is_ok());
        let kp_id = kp_obj.read();

        let rsa_kp = rsa_keypair_from_pkcs8_der(RSA_TEST_DER);
        let obj_arc = tee_obj_get(kp_id as tee_obj_id_type);
        ::core::assert!(obj_arc.is_ok());
        let obj_arc = obj_arc.unwrap();
        let mut obj = obj_arc.lock();
        let _ = core::mem::replace(&mut obj.attr[0], TeeCryptObj::rsa_keypair(rsa_kp));
        obj.have_attrs = (1u32 << 8) - 1;
        obj.info.objectSize = 2048;
        obj.info.handleFlags |= TEE_HANDLE_FLAG_INITIALIZED;
        drop(obj);

        let res = TestUserValue::<c_uint>::from_value(0);
        ::core::assert!(res.is_ok());
        let mut pub_obj = res.unwrap();
        let res = syscall_cryp_obj_alloc(TEE_TYPE_RSA_PUBLIC_KEY as _, 2048, pub_obj.as_user_ref());
        ::core::assert!(res.is_ok());
        let pub_id = pub_obj.read();
        let res = syscall_cryp_obj_copy(pub_id as _, kp_id as _);
        ::core::assert!(res.is_ok());
        (kp_id, pub_id)
    }

    #[unittest::def_test(custom)]
    fn test_cryp_rsa_oaep_sha256_encrypt_decrypt_with_label() {
        let (kp_id, pub_id) = install_rsa_test_der_key_objects();

        let mut st_enc: u32 = 0;
        let res = tee_cryp_state_alloc(
            TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA256,
            TEE_OperationMode::TEE_MODE_ENCRYPT,
            Some(pub_id),
            None,
            &mut st_enc,
        );
        assert!(res.is_ok());

        let mut st_dec: u32 = 0;
        let res = tee_cryp_state_alloc(
            TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA256,
            TEE_OperationMode::TEE_MODE_DECRYPT,
            Some(kp_id),
            None,
            &mut st_dec,
        );
        assert!(res.is_ok());

        let plain = b"testing123";
        let label = b"MY_LABEL";
        let mut cipher = [0u8; 256];
        let res = tee_cryp_asymm_operate(st_enc, plain, &mut cipher, Some(label));
        assert!(res.is_ok());
        let cipher_len = res.unwrap();
        assert_eq!(cipher_len, cipher.len());

        let mut out = [0u8; 32];
        let res = tee_cryp_asymm_operate(st_dec, &cipher, &mut out, Some(label));
        assert!(res.is_ok());
        let out_len = res.unwrap();
        assert_eq!(out_len, plain.len());
        assert_eq!(&out[..out_len], plain.as_slice());

        let res = tee_cryp_asymm_operate(st_dec, &cipher, &mut out, Some(b"WRONG_LABEL"));
        assert_eq!(res.err(), Some(TEE_ERROR_BAD_PARAMETERS));
    }

    /// OP-TEE `regression_4000_data.h` → `ac_rsaes_oaep_vect1` (also regression_4006 case 46/47).
    mod ac_rsaes_oaep_vect1 {
        pub const MODULUS: [u8; 128] = [
            0xa8, 0xb3, 0xb2, 0x84, 0xaf, 0x8e, 0xb5, 0x0b, 0x38, 0x70, 0x34, 0xa8, 0x60, 0xf1,
            0x46, 0xc4, 0x91, 0x9f, 0x31, 0x87, 0x63, 0xcd, 0x6c, 0x55, 0x98, 0xc8, 0xae, 0x48,
            0x11, 0xa1, 0xe0, 0xab, 0xc4, 0xc7, 0xe0, 0xb0, 0x82, 0xd6, 0x93, 0xa5, 0xe7, 0xfc,
            0xed, 0x67, 0x5c, 0xf4, 0x66, 0x85, 0x12, 0x77, 0x2c, 0x0c, 0xbc, 0x64, 0xa7, 0x42,
            0xc6, 0xc6, 0x30, 0xf5, 0x33, 0xc8, 0xcc, 0x72, 0xf6, 0x2a, 0xe8, 0x33, 0xc4, 0x0b,
            0xf2, 0x58, 0x42, 0xe9, 0x84, 0xbb, 0x78, 0xbd, 0xbf, 0x97, 0xc0, 0x10, 0x7d, 0x55,
            0xbd, 0xb6, 0x62, 0xf5, 0xc4, 0xe0, 0xfa, 0xb9, 0x84, 0x5c, 0xb5, 0x14, 0x8e, 0xf7,
            0x39, 0x2d, 0xd3, 0xaa, 0xff, 0x93, 0xae, 0x1e, 0x6b, 0x66, 0x7b, 0xb3, 0xd4, 0x24,
            0x76, 0x16, 0xd4, 0xf5, 0xba, 0x10, 0xd4, 0xcf, 0xd2, 0x26, 0xde, 0x88, 0xd3, 0x9f,
            0x16, 0xfb,
        ];
        pub const PUB_EXP: [u8; 3] = [0x01, 0x00, 0x01];
        pub const PRIV_EXP: [u8; 128] = [
            0x53, 0x33, 0x9c, 0xfd, 0xb7, 0x9f, 0xc8, 0x46, 0x6a, 0x65, 0x5c, 0x73, 0x16, 0xac,
            0xa8, 0x5c, 0x55, 0xfd, 0x8f, 0x6d, 0xd8, 0x98, 0xfd, 0xaf, 0x11, 0x95, 0x17, 0xef,
            0x4f, 0x52, 0xe8, 0xfd, 0x8e, 0x25, 0x8d, 0xf9, 0x3f, 0xee, 0x18, 0x0f, 0xa0, 0xe4,
            0xab, 0x29, 0x69, 0x3c, 0xd8, 0x3b, 0x15, 0x2a, 0x55, 0x3d, 0x4a, 0xc4, 0xd1, 0x81,
            0x2b, 0x8b, 0x9f, 0xa5, 0xaf, 0x0e, 0x7f, 0x55, 0xfe, 0x73, 0x04, 0xdf, 0x41, 0x57,
            0x09, 0x26, 0xf3, 0x31, 0x1f, 0x15, 0xc4, 0xd6, 0x5a, 0x73, 0x2c, 0x48, 0x31, 0x16,
            0xee, 0x3d, 0x3d, 0x2d, 0x0a, 0xf3, 0x54, 0x9a, 0xd9, 0xbf, 0x7c, 0xbf, 0xb7, 0x8a,
            0xd8, 0x84, 0xf8, 0x4d, 0x5b, 0xeb, 0x04, 0x72, 0x4d, 0xc7, 0x36, 0x9b, 0x31, 0xde,
            0xf3, 0x7d, 0x0c, 0xf5, 0x39, 0xe9, 0xcf, 0xcd, 0xd3, 0xde, 0x65, 0x37, 0x29, 0xea,
            0xd5, 0xd1,
        ];
        pub const PRIME1: [u8; 64] = [
            0xd3, 0x27, 0x37, 0xe7, 0x26, 0x7f, 0xfe, 0x13, 0x41, 0xb2, 0xd5, 0xc0, 0xd1, 0x50,
            0xa8, 0x1b, 0x58, 0x6f, 0xb3, 0x13, 0x2b, 0xed, 0x2f, 0x8d, 0x52, 0x62, 0x86, 0x4a,
            0x9c, 0xb9, 0xf3, 0x0a, 0xf3, 0x8b, 0xe4, 0x48, 0x59, 0x8d, 0x41, 0x3a, 0x17, 0x2e,
            0xfb, 0x80, 0x2c, 0x21, 0xac, 0xf1, 0xc1, 0x1c, 0x52, 0x0c, 0x2f, 0x26, 0xa4, 0x71,
            0xdc, 0xad, 0x21, 0x2e, 0xac, 0x7c, 0xa3, 0x9d,
        ];
        pub const PRIME2: [u8; 64] = [
            0xcc, 0x88, 0x53, 0xd1, 0xd5, 0x4d, 0xa6, 0x30, 0xfa, 0xc0, 0x04, 0xf4, 0x71, 0xf2,
            0x81, 0xc7, 0xb8, 0x98, 0x2d, 0x82, 0x24, 0xa4, 0x90, 0xed, 0xbe, 0xb3, 0x3d, 0x3e,
            0x3d, 0x5c, 0xc9, 0x3c, 0x47, 0x65, 0x70, 0x3d, 0x1d, 0xd7, 0x91, 0x64, 0x2f, 0x1f,
            0x11, 0x6a, 0x0d, 0xd8, 0x52, 0xbe, 0x24, 0x19, 0xb2, 0xaf, 0x72, 0xbf, 0xe9, 0xa0,
            0x30, 0xe8, 0x60, 0xb0, 0x28, 0x8b, 0x5d, 0x77,
        ];
        pub const EXP1: [u8; 64] = [
            0x0e, 0x12, 0xbf, 0x17, 0x18, 0xe9, 0xce, 0xf5, 0x59, 0x9b, 0xa1, 0xc3, 0x88, 0x2f,
            0xe8, 0x04, 0x6a, 0x90, 0x87, 0x4e, 0xef, 0xce, 0x8f, 0x2c, 0xcc, 0x20, 0xe4, 0xf2,
            0x74, 0x1f, 0xb0, 0xa3, 0x3a, 0x38, 0x48, 0xae, 0xc9, 0xc9, 0x30, 0x5f, 0xbe, 0xcb,
            0xd2, 0xd7, 0x68, 0x19, 0x96, 0x7d, 0x46, 0x71, 0xac, 0xc6, 0x43, 0x1e, 0x40, 0x37,
            0x96, 0x8d, 0xb3, 0x78, 0x78, 0xe6, 0x95, 0xc1,
        ];
        pub const EXP2: [u8; 64] = [
            0x95, 0x29, 0x7b, 0x0f, 0x95, 0xa2, 0xfa, 0x67, 0xd0, 0x07, 0x07, 0xd6, 0x09, 0xdf,
            0xd4, 0xfc, 0x05, 0xc8, 0x9d, 0xaf, 0xc2, 0xef, 0x6d, 0x6e, 0xa5, 0x5b, 0xec, 0x77,
            0x1e, 0xa3, 0x33, 0x73, 0x4d, 0x92, 0x51, 0xe7, 0x90, 0x82, 0xec, 0xda, 0x86, 0x6e,
            0xfe, 0xf1, 0x3c, 0x45, 0x9e, 0x1a, 0x63, 0x13, 0x86, 0xb7, 0xe3, 0x54, 0xc8, 0x99,
            0xf5, 0xf1, 0x12, 0xca, 0x85, 0xd7, 0x15, 0x83,
        ];
        pub const COEFF: [u8; 64] = [
            0x4f, 0x45, 0x6c, 0x50, 0x24, 0x93, 0xbd, 0xc0, 0xed, 0x2a, 0xb7, 0x56, 0xa3, 0xa6,
            0xed, 0x4d, 0x67, 0x35, 0x2a, 0x69, 0x7d, 0x42, 0x16, 0xe9, 0x32, 0x12, 0xb1, 0x27,
            0xa6, 0x3d, 0x54, 0x11, 0xce, 0x6f, 0xa9, 0x8d, 0x5d, 0xbe, 0xfd, 0x73, 0x26, 0x3e,
            0x37, 0x28, 0x14, 0x27, 0x43, 0x81, 0x81, 0x66, 0xed, 0x7d, 0xd6, 0x36, 0x87, 0xdd,
            0x2a, 0x8c, 0xa1, 0xd2, 0xf4, 0xfb, 0xd8, 0xe1,
        ];
        pub const PTX: [u8; 28] = [
            0x66, 0x28, 0x19, 0x4e, 0x12, 0x07, 0x3d, 0xb0, 0x3b, 0xa9, 0x4c, 0xda, 0x9e, 0xf9,
            0x53, 0x23, 0x97, 0xd5, 0x0d, 0xba, 0x79, 0xb9, 0x87, 0x00, 0x4a, 0xfe, 0xfe, 0x34,
        ];
        pub const CIPHER: [u8; 128] = [
            0x35, 0x4f, 0xe6, 0x7b, 0x4a, 0x12, 0x6d, 0x5d, 0x35, 0xfe, 0x36, 0xc7, 0x77, 0x79,
            0x1a, 0x3f, 0x7b, 0xa1, 0x3d, 0xef, 0x48, 0x4e, 0x2d, 0x39, 0x08, 0xaf, 0xf7, 0x22,
            0xfa, 0xd4, 0x68, 0xfb, 0x21, 0x69, 0x6d, 0xe9, 0x5d, 0x0b, 0xe9, 0x11, 0xc2, 0xd3,
            0x17, 0x4f, 0x8a, 0xfc, 0xc2, 0x01, 0x03, 0x5f, 0x7b, 0x6d, 0x8e, 0x69, 0x40, 0x2d,
            0xe5, 0x45, 0x16, 0x18, 0xc2, 0x1a, 0x53, 0x5f, 0xa9, 0xd7, 0xbf, 0xc5, 0xb8, 0xdd,
            0x9f, 0xc2, 0x43, 0xf8, 0xcf, 0x92, 0x7d, 0xb3, 0x13, 0x22, 0xd6, 0xe8, 0x81, 0xea,
            0xa9, 0x1a, 0x99, 0x61, 0x70, 0xe6, 0x57, 0xa0, 0x5a, 0x26, 0x64, 0x26, 0xd9, 0x8c,
            0x88, 0x00, 0x3f, 0x84, 0x77, 0xc1, 0x22, 0x70, 0x94, 0xa0, 0xd9, 0xfa, 0x1e, 0x8c,
            0x40, 0x24, 0x30, 0x9c, 0xe1, 0xec, 0xcc, 0xb5, 0x21, 0x00, 0x35, 0xd4, 0x7a, 0xc7,
            0x2e, 0x8a,
        ];
    }

    /// Populate `ac_rsaes_oaep_vect1` into a transient RSA keypair object.
    /// `with_crt == false` matches regression_4006 case 46 (n/e/d only); `true` adds CRT factors.
    fn install_ac_rsaes_oaep_vect1_keypair(with_crt: bool) -> u32 {
        use ac_rsaes_oaep_vect1::*;

        let mut kp_obj = TestUserValue::<c_uint>::from_value(0).unwrap();
        syscall_cryp_obj_alloc(TEE_TYPE_RSA_KEYPAIR as _, 1024, kp_obj.as_user_ref()).unwrap();
        let kp_id = kp_obj.read();

        let mut usr_n = crate::user_vec![0u8; 128];
        let mut usr_e = crate::user_vec![0u8; 3];
        let mut usr_d = crate::user_vec![0u8; 128];
        usr_n.copy_from_slice(&MODULUS);
        usr_e.copy_from_slice(&PUB_EXP);
        usr_d.copy_from_slice(&PRIV_EXP);

        if with_crt {
            let mut usr_p = crate::user_vec![0u8; 64];
            let mut usr_q = crate::user_vec![0u8; 64];
            let mut usr_dp = crate::user_vec![0u8; 64];
            let mut usr_dq = crate::user_vec![0u8; 64];
            let mut usr_qp = crate::user_vec![0u8; 64];
            usr_p.copy_from_slice(&PRIME1);
            usr_q.copy_from_slice(&PRIME2);
            usr_dp.copy_from_slice(&EXP1);
            usr_dq.copy_from_slice(&EXP2);
            usr_qp.copy_from_slice(&COEFF);

            let mut usr_attrs = crate::user_vec![utee_attribute::default(); 8];
            tee_init_ref_attribute(&mut usr_attrs[0], TEE_ATTR_RSA_MODULUS, &usr_n[..], 128);
            tee_init_ref_attribute(
                &mut usr_attrs[1],
                TEE_ATTR_RSA_PUBLIC_EXPONENT,
                &usr_e[..],
                3,
            );
            tee_init_ref_attribute(
                &mut usr_attrs[2],
                TEE_ATTR_RSA_PRIVATE_EXPONENT,
                &usr_d[..],
                128,
            );
            tee_init_ref_attribute(&mut usr_attrs[3], TEE_ATTR_RSA_PRIME1, &usr_p[..], 64);
            tee_init_ref_attribute(&mut usr_attrs[4], TEE_ATTR_RSA_PRIME2, &usr_q[..], 64);
            tee_init_ref_attribute(&mut usr_attrs[5], TEE_ATTR_RSA_EXPONENT1, &usr_dp[..], 64);
            tee_init_ref_attribute(&mut usr_attrs[6], TEE_ATTR_RSA_EXPONENT2, &usr_dq[..], 64);
            tee_init_ref_attribute(&mut usr_attrs[7], TEE_ATTR_RSA_COEFFICIENT, &usr_qp[..], 64);
            syscall_cryp_obj_populate(kp_id as c_ulong, usr_attrs.as_mut_ptr(), 8).unwrap();
        } else {
            let mut usr_attrs = crate::user_vec![utee_attribute::default(); 3];
            tee_init_ref_attribute(&mut usr_attrs[0], TEE_ATTR_RSA_MODULUS, &usr_n[..], 128);
            tee_init_ref_attribute(
                &mut usr_attrs[1],
                TEE_ATTR_RSA_PUBLIC_EXPONENT,
                &usr_e[..],
                3,
            );
            tee_init_ref_attribute(
                &mut usr_attrs[2],
                TEE_ATTR_RSA_PRIVATE_EXPONENT,
                &usr_d[..],
                128,
            );
            syscall_cryp_obj_populate(kp_id as c_ulong, usr_attrs.as_mut_ptr(), 3).unwrap();
        }

        kp_id
    }

    fn alloc_ac_rsaes_oaep_vect1_decrypt_op(kp_id: u32) -> u32 {
        let mut st_dec: u32 = 0;
        tee_cryp_state_alloc(
            TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA1,
            TEE_OperationMode::TEE_MODE_DECRYPT,
            Some(kp_id),
            None,
            &mut st_dec,
        )
        .unwrap();
        st_dec
    }

    /// regression_4006 **case 46 host path**: n/e/d key (no CRT), preset ciphertext,
    /// `TEE_ATTR_RSA_OAEP_MGF_HASH` = SHA1 via `syscall_asymm_operate`.
    ///
    /// Phase 1 isolates `tee_cryp_asymm_operate_with_required`; phase 2 matches TA/libutee.
    /// Run: build x-kernel with `UNITTEST=y`, then filter `case46_host_decrypt_path`.
    #[unittest::def_test(custom)]
    fn test_regression_4006_case46_host_decrypt_path() {
        use ac_rsaes_oaep_vect1::{CIPHER, PTX};

        let kp_id = install_ac_rsaes_oaep_vect1_keypair(false);
        let st_dec = alloc_ac_rsaes_oaep_vect1_decrypt_op(kp_id);

        // Phase 1: kernel crypto API (no syscall attribute marshalling).
        let mut out = [0u8; 128];
        let mut required = 0usize;
        let res = tee_cryp_asymm_operate_with_required(
            st_dec,
            &CIPHER,
            &mut out,
            None,
            Some(TEE_ALG_SHA1),
            &mut required,
        );
        if let Err(e) = res {
            panic!(
                "phase1 tee_cryp_asymm_operate_with_required failed: {:#x}",
                e
            );
        }
        assert_eq!(required, PTX.len());
        assert_eq!(&out[..required], PTX);

        // Phase 2: same as host regression_4006 → TA → TEE_AsymmetricDecrypt → syscall.
        // MGF hash and out_len must live in mapped user VA (syscall uses copy_from_user).
        let mut usr_mgf = crate::user_vec![0u8; core::mem::size_of::<u32>()];
        usr_mgf.copy_from_slice(&TEE_ALG_SHA1.to_ne_bytes());
        let mut usr_attrs = crate::user_vec![utee_attribute::default(); 1];
        tee_init_ref_attribute(
            &mut usr_attrs[0],
            TEE_ATTR_RSA_OAEP_MGF_HASH,
            &usr_mgf[..],
            core::mem::size_of::<u32>() as u32,
        );

        let mut usr_cipher = crate::user_vec![0u8; CIPHER.len()];
        usr_cipher.copy_from_slice(&CIPHER);
        let mut usr_out = crate::user_vec![0u8; 512];
        let mut usr_out_len = TestUserValue::<usize>::from_value(usr_out.len()).unwrap();

        let res = syscall_asymm_operate(
            st_dec as usize,
            usr_attrs.as_ptr() as usize,
            1,
            usr_cipher.as_ptr() as usize,
            usr_cipher.len(),
            usr_out.as_mut_ptr() as usize,
            usr_out_len.as_user_ptr() as usize,
        );
        if let Err(e) = res {
            panic!("phase2 syscall_asymm_operate failed: {:#x}", e);
        }
        let out_len = usr_out_len.read();
        assert_eq!(out_len, PTX.len());
        assert_eq!(&usr_out[..out_len], PTX);
    }

    /// regression_4006 **case 46 TA/libutee key path**: empty op key object at
    /// `tee_cryp_state_alloc`, then `syscall_cryp_obj_copy` (same as
    /// `TEE_AllocateOperation` + `TEE_SetOperationKey`), preset ciphertext, MGF via
    /// `syscall_asymm_operate`.
    ///
    /// Phase 1/2 of `test_regression_4006_case46_host_decrypt_path` bind the populated
    /// keypair at state alloc; this test closes the remaining gap to host regression.
    #[unittest::def_test(custom)]
    fn test_regression_4006_case46_set_operation_key_decrypt_path() {
        use ac_rsaes_oaep_vect1::{CIPHER, PTX};

        let src_priv_id = install_ac_rsaes_oaep_vect1_keypair(false);

        // libutee TEE_AllocateOperation: empty key1 + cryp state alloc before SetOperationKey.
        let mut op_key_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        syscall_cryp_obj_alloc(TEE_TYPE_RSA_KEYPAIR as _, 1024, op_key_id.as_user_ref()).unwrap();
        let op_key_id = op_key_id.read();

        let mut st_dec: u32 = 0;
        tee_cryp_state_alloc(
            TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA1,
            TEE_OperationMode::TEE_MODE_DECRYPT,
            Some(op_key_id),
            None,
            &mut st_dec,
        )
        .unwrap();

        // libutee TEE_SetOperationKey → TEE_CopyObjectAttributes1 → _utee_cryp_obj_copy.
        syscall_cryp_obj_copy(op_key_id as _, src_priv_id as _).unwrap();

        let mut usr_mgf = crate::user_vec![0u8; core::mem::size_of::<u32>()];
        usr_mgf.copy_from_slice(&TEE_ALG_SHA1.to_ne_bytes());
        let mut usr_attrs = crate::user_vec![utee_attribute::default(); 1];
        tee_init_ref_attribute(
            &mut usr_attrs[0],
            TEE_ATTR_RSA_OAEP_MGF_HASH,
            &usr_mgf[..],
            core::mem::size_of::<u32>() as u32,
        );

        let mut usr_cipher = crate::user_vec![0u8; CIPHER.len()];
        usr_cipher.copy_from_slice(&CIPHER);
        let mut usr_out = crate::user_vec![0u8; 512];
        let mut usr_out_len = TestUserValue::<usize>::from_value(usr_out.len()).unwrap();

        let res = syscall_asymm_operate(
            st_dec as usize,
            usr_attrs.as_ptr() as usize,
            1,
            usr_cipher.as_ptr() as usize,
            usr_cipher.len(),
            usr_out.as_mut_ptr() as usize,
            usr_out_len.as_user_ptr() as usize,
        );
        if let Err(e) = res {
            panic!(
                "set_operation_key path syscall_asymm_operate failed: {:#x}",
                e
            );
        }
        let out_len = usr_out_len.read();
        assert_eq!(out_len, PTX.len());
        assert_eq!(&usr_out[..out_len], PTX);
    }

    /// CRT key + direct `tee_cryp_asymm_operate` (no MGF attribute); baseline for vect1.
    #[unittest::def_test(custom)]
    fn test_regression_4006_rsa_oaep_vect1_decrypt_crt_baseline() {
        use ac_rsaes_oaep_vect1::{CIPHER, PTX};

        let kp_id = install_ac_rsaes_oaep_vect1_keypair(true);
        let st_dec = alloc_ac_rsaes_oaep_vect1_decrypt_op(kp_id);

        let mut out = [0u8; 128];
        let len = tee_cryp_asymm_operate(st_dec, &CIPHER, &mut out, None).unwrap();
        assert_eq!(len, PTX.len());
        assert_eq!(&out[..len], PTX);
    }

    #[unittest::def_test(custom)]
    fn test_rsa_oaep_mgf_hash_mismatch_not_supported() {
        use crate::tee::crypto::rsa::rsaes_oaep_check_mgf;

        assert_eq!(
            rsaes_oaep_check_mgf(TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA256, TEE_ALG_SHA1).err(),
            Some(TEE_ERROR_NOT_SUPPORTED)
        );
        assert!(rsaes_oaep_check_mgf(TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA256, TEE_ALG_SHA256).is_ok());
    }

    #[unittest::def_test(custom)]
    fn test_cryp_rsa_pkcs1_v15_encrypt_decrypt_random_key() {
        let kp_obj = TestUserValue::<c_uint>::from_value(0);
        assert!(kp_obj.is_ok());
        let mut kp_obj = kp_obj.unwrap();
        let res = syscall_cryp_obj_alloc(TEE_TYPE_RSA_KEYPAIR as _, 2048, kp_obj.as_user_ref());
        assert!(res.is_ok());
        let kp_id = kp_obj.read();

        let res = syscall_obj_generate_key(kp_id as c_ulong, 2048, core::ptr::null(), 0);
        assert!(res.is_ok());

        let obj_arc = tee_obj_get(kp_id as tee_obj_id_type);
        assert!(obj_arc.is_ok());
        let obj_arc = obj_arc.unwrap();
        let obj = obj_arc.lock();
        assert_eq!(obj.info.objectType, TEE_TYPE_RSA_KEYPAIR);
        assert_eq!(obj.info.maxObjectSize, 2048);
        assert!(matches!(obj.attr[0], TeeCryptObj::rsa_keypair(_)));
        drop(obj);

        let pub_obj = TestUserValue::<c_uint>::from_value(0);
        assert!(pub_obj.is_ok());
        let mut pub_obj = pub_obj.unwrap();
        let res = syscall_cryp_obj_alloc(TEE_TYPE_RSA_PUBLIC_KEY as _, 2048, pub_obj.as_user_ref());
        assert!(res.is_ok());
        let pub_id = pub_obj.read();

        let res = syscall_cryp_obj_copy(pub_id as _, kp_id as _);
        assert!(res.is_ok());

        let mut st_enc: u32 = 0;
        let res = tee_cryp_state_alloc(
            TEE_ALG_RSAES_PKCS1_V1_5,
            TEE_OperationMode::TEE_MODE_ENCRYPT,
            Some(pub_id),
            None,
            &mut st_enc,
        );
        assert!(res.is_ok());

        let mut st_dec: u32 = 0;
        let res = tee_cryp_state_alloc(
            TEE_ALG_RSAES_PKCS1_V1_5,
            TEE_OperationMode::TEE_MODE_DECRYPT,
            Some(kp_id),
            None,
            &mut st_dec,
        );
        assert!(res.is_ok());

        let plain = b"random 2048-bit rsa pkcs#1 v1.5";
        let mut cipher = [0u8; 256];
        let res = tee_cryp_asymm_operate(st_enc, plain, &mut cipher, None);
        assert!(res.is_ok());
        let cipher_len = res.unwrap();
        assert_eq!(cipher_len, cipher.len());

        let mut out = [0u8; 256];
        let res = tee_cryp_asymm_operate(st_dec, &cipher[..cipher_len], &mut out, None);
        assert!(res.is_ok());
        let out_len = res.unwrap();
        assert_eq!(out_len, plain.len());
        assert_eq!(&out[..out_len], plain.as_slice());
    }

    /// OP-TEE allows RSA ENCRYPT/VERIFY with a keypair object (public half is used).
    #[unittest::def_test(custom)]
    fn test_cryp_rsa_pkcs1_v15_encrypt_with_keypair_object() {
        let (kp_id, _pub_id) = install_rsa_test_der_key_objects();
        let mut kp2_obj = TestUserValue::<c_uint>::from_value(0).unwrap();
        syscall_cryp_obj_alloc(TEE_TYPE_RSA_KEYPAIR as _, 2048, kp2_obj.as_user_ref()).unwrap();
        let kp2_id = kp2_obj.read();
        syscall_cryp_obj_copy(kp2_id as _, kp_id as _).unwrap();

        let plain = b"encrypt via keypair handle";

        let mut st_enc: u32 = 0;
        tee_cryp_state_alloc(
            TEE_ALG_RSAES_PKCS1_V1_5,
            TEE_OperationMode::TEE_MODE_ENCRYPT,
            Some(kp_id),
            None,
            &mut st_enc,
        )
        .unwrap();

        let mut st_dec: u32 = 0;
        tee_cryp_state_alloc(
            TEE_ALG_RSAES_PKCS1_V1_5,
            TEE_OperationMode::TEE_MODE_DECRYPT,
            Some(kp2_id),
            None,
            &mut st_dec,
        )
        .unwrap();

        let mut cipher = [0u8; 256];
        let cipher_len = tee_cryp_asymm_operate(st_enc, plain, &mut cipher, None).unwrap();

        let mut out = [0u8; 256];
        let out_len =
            tee_cryp_asymm_operate(st_dec, &cipher[..cipher_len], &mut out, None).unwrap();
        assert_eq!(&out[..out_len], plain);
    }

    /// Regression vectors often supply only (n, e, d); CRT primes are optional in OP-TEE.
    #[unittest::def_test(custom)]
    fn test_cryp_rsa_pkcs1_v15_decrypt_without_crt_primes() {
        let (kp_id, pub_id) = install_rsa_test_der_key_objects();
        {
            let obj = tee_obj_get(kp_id as tee_obj_id_type).unwrap();
            let mut guard = obj.lock();
            let TeeCryptObj::rsa_keypair(kp) = &mut guard.attr[0] else {
                panic!("expected rsa keypair");
            };
            kp.p = BigNum::default();
            kp.q = BigNum::default();
            kp.dp = BigNum::default();
            kp.dq = BigNum::default();
            kp.qp = BigNum::default();
        }

        let plain = b"decrypt without crt";
        let mut st_enc: u32 = 0;
        tee_cryp_state_alloc(
            TEE_ALG_RSAES_PKCS1_V1_5,
            TEE_OperationMode::TEE_MODE_ENCRYPT,
            Some(pub_id),
            None,
            &mut st_enc,
        )
        .unwrap();

        let mut st_dec: u32 = 0;
        tee_cryp_state_alloc(
            TEE_ALG_RSAES_PKCS1_V1_5,
            TEE_OperationMode::TEE_MODE_DECRYPT,
            Some(kp_id),
            None,
            &mut st_dec,
        )
        .unwrap();

        let mut cipher = [0u8; 256];
        let cipher_len = tee_cryp_asymm_operate(st_enc, plain, &mut cipher, None).unwrap();

        let mut out = [0u8; 256];
        let out_len =
            tee_cryp_asymm_operate(st_dec, &cipher[..cipher_len], &mut out, None).unwrap();
        assert_eq!(&out[..out_len], plain);
    }

    #[unittest::def_test(custom)]
    fn test_cryp_rsa_nopad_roundtrip_returns_plaintext_len() {
        let (kp_id, pub_id) = install_rsa_test_der_key_objects();
        let plain = b"rsa nopad plain txt";

        let mut st_enc: u32 = 0;
        tee_cryp_state_alloc(
            TEE_ALG_RSA_NOPAD,
            TEE_OperationMode::TEE_MODE_ENCRYPT,
            Some(pub_id),
            None,
            &mut st_enc,
        )
        .unwrap();

        let mut st_dec: u32 = 0;
        tee_cryp_state_alloc(
            TEE_ALG_RSA_NOPAD,
            TEE_OperationMode::TEE_MODE_DECRYPT,
            Some(kp_id),
            None,
            &mut st_dec,
        )
        .unwrap();

        let mut cipher = [0u8; 256];
        let cipher_len = tee_cryp_asymm_operate(st_enc, plain, &mut cipher, None).unwrap();
        assert!(cipher_len <= cipher.len());
        assert!(cipher_len > 0);

        let mut out = [0u8; 256];
        let out_len =
            tee_cryp_asymm_operate(st_dec, &cipher[..cipher_len], &mut out, None).unwrap();
        assert_eq!(out_len, plain.len());
        assert_eq!(&out[..out_len], plain);
    }

    #[unittest::def_test(custom)]
    fn test_cryp_rsa_sign_short_buffer_probe() {
        let (kp_id, _pub_id) = install_rsa_test_der_key_objects();
        let mut st_sign: u32 = 0;
        tee_cryp_state_alloc(
            TEE_ALG_RSASSA_PKCS1_V1_5_SHA256,
            TEE_OperationMode::TEE_MODE_SIGN,
            Some(kp_id),
            None,
            &mut st_sign,
        )
        .unwrap();

        let digest = [0u8; 32];
        let mut required = 0usize;
        let res = tee_cryp_asymm_operate_with_required(
            st_sign,
            &digest,
            &mut [],
            None,
            None,
            &mut required,
        );
        assert_eq!(res.err(), Some(TEE_ERROR_SHORT_BUFFER));
        assert_eq!(required, 256);
    }

    #[unittest::def_test(custom)]
    fn test_cryp_rsa_oaep_sha256_encrypt_decrypt_random_key_with_label() {
        let kp_obj = TestUserValue::<c_uint>::from_value(0);
        assert!(kp_obj.is_ok());
        let mut kp_obj = kp_obj.unwrap();
        let res = syscall_cryp_obj_alloc(TEE_TYPE_RSA_KEYPAIR as _, 2048, kp_obj.as_user_ref());
        assert!(res.is_ok());
        let kp_id = kp_obj.read();

        let res = syscall_obj_generate_key(kp_id as c_ulong, 2048, core::ptr::null(), 0);
        assert!(res.is_ok());

        let obj_arc = tee_obj_get(kp_id as tee_obj_id_type);
        assert!(obj_arc.is_ok());
        let obj_arc = obj_arc.unwrap();
        let obj = obj_arc.lock();
        assert_eq!(obj.info.objectType, TEE_TYPE_RSA_KEYPAIR);
        assert_eq!(obj.info.maxObjectSize, 2048);
        assert!(matches!(obj.attr[0], TeeCryptObj::rsa_keypair(_)));
        drop(obj);

        let pub_obj = TestUserValue::<c_uint>::from_value(0);
        assert!(pub_obj.is_ok());
        let mut pub_obj = pub_obj.unwrap();
        let res = syscall_cryp_obj_alloc(TEE_TYPE_RSA_PUBLIC_KEY as _, 2048, pub_obj.as_user_ref());
        assert!(res.is_ok());
        let pub_id = pub_obj.read();

        let res = syscall_cryp_obj_copy(pub_id as _, kp_id as _);
        assert!(res.is_ok());

        let mut st_enc: u32 = 0;
        let res = tee_cryp_state_alloc(
            TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA256,
            TEE_OperationMode::TEE_MODE_ENCRYPT,
            Some(pub_id),
            None,
            &mut st_enc,
        );
        assert!(res.is_ok());

        let mut st_dec: u32 = 0;
        let res = tee_cryp_state_alloc(
            TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA256,
            TEE_OperationMode::TEE_MODE_DECRYPT,
            Some(kp_id),
            None,
            &mut st_dec,
        );
        assert!(res.is_ok());

        let plain = b"testing123";
        let label = b"MY_LABEL";
        let mut cipher = [0u8; 256];
        let res = tee_cryp_asymm_operate(st_enc, plain, &mut cipher, Some(label));
        assert!(res.is_ok());
        let cipher_len = res.unwrap();
        assert_eq!(cipher_len, cipher.len());

        let mut out = [0u8; 256];
        let res = tee_cryp_asymm_operate(st_dec, &cipher[..cipher_len], &mut out, Some(label));
        assert!(res.is_ok());
        let out_len = res.unwrap();
        assert_eq!(out_len, plain.len());
        assert_eq!(&out[..out_len], plain.as_slice());

        let res = tee_cryp_asymm_operate(
            st_dec,
            &cipher[..cipher_len],
            &mut out,
            Some(b"WRONG_LABEL"),
        );
        assert_eq!(res.err(), Some(TEE_ERROR_BAD_PARAMETERS));
    }

    #[unittest::def_test(custom)]
    fn test_cryp_rsa_pss_sha256_sign_verify_random_key() {
        let kp_obj = TestUserValue::<c_uint>::from_value(0);
        assert!(kp_obj.is_ok());
        let mut kp_obj = kp_obj.unwrap();
        let res = syscall_cryp_obj_alloc(TEE_TYPE_RSA_KEYPAIR as _, 2048, kp_obj.as_user_ref());
        assert!(res.is_ok());
        let kp_id = kp_obj.read();

        let res = syscall_obj_generate_key(kp_id as c_ulong, 2048, core::ptr::null(), 0);
        assert!(res.is_ok());

        let obj_arc = tee_obj_get(kp_id as tee_obj_id_type);
        assert!(obj_arc.is_ok());
        let obj_arc = obj_arc.unwrap();
        let obj = obj_arc.lock();
        assert_eq!(obj.info.objectType, TEE_TYPE_RSA_KEYPAIR);
        assert!(matches!(obj.attr[0], TeeCryptObj::rsa_keypair(_)));
        drop(obj);

        let pub_obj = TestUserValue::<c_uint>::from_value(0);
        assert!(pub_obj.is_ok());
        let mut pub_obj = pub_obj.unwrap();
        let res = syscall_cryp_obj_alloc(TEE_TYPE_RSA_PUBLIC_KEY as _, 2048, pub_obj.as_user_ref());
        assert!(res.is_ok());
        let pub_id = pub_obj.read();

        let res = syscall_cryp_obj_copy(pub_id as _, kp_id as _);
        assert!(res.is_ok());

        let mut st_sign: u32 = 0;
        let res = tee_cryp_state_alloc(
            TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA256,
            TEE_OperationMode::TEE_MODE_SIGN,
            Some(kp_id),
            None,
            &mut st_sign,
        );
        assert!(res.is_ok());

        let mut st_vfy: u32 = 0;
        let res = tee_cryp_state_alloc(
            TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA256,
            TEE_OperationMode::TEE_MODE_VERIFY,
            Some(pub_id),
            None,
            &mut st_vfy,
        );
        assert!(res.is_ok());

        let data = b"SIGNATURE TEST SIGNATURE TEST SI";
        let mut sig = [0u8; 256];
        let res = tee_cryp_asymm_operate(st_sign, data, &mut sig, None);
        assert!(res.is_ok());
        let sig_len = res.unwrap();

        let res = tee_cryp_asymm_verify(st_vfy, data, &sig[..sig_len]);
        assert!(res.is_ok());
    }

    #[unittest::def_test(custom)]
    fn test_cryp_rsa_pss_sha256_sign_verify() {
        let (kp_id, pub_id) = install_rsa_test_der_key_objects();

        let mut st_sign: u32 = 0;
        let res = tee_cryp_state_alloc(
            TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA256,
            TEE_OperationMode::TEE_MODE_SIGN,
            Some(kp_id),
            None,
            &mut st_sign,
        );
        assert!(res.is_ok());

        let mut st_vfy: u32 = 0;
        let res = tee_cryp_state_alloc(
            TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA256,
            TEE_OperationMode::TEE_MODE_VERIFY,
            Some(pub_id),
            None,
            &mut st_vfy,
        );
        assert!(res.is_ok());

        let data = b"SIGNATURE TEST SIGNATURE TEST SI";
        let mut sig = [0u8; 256];
        let res = tee_cryp_asymm_operate(st_sign, data, &mut sig, None);
        assert!(res.is_ok());
        let sig_len = res.unwrap();

        let res = tee_cryp_asymm_verify(st_vfy, data, &sig[..sig_len]);
        assert!(res.is_ok());

        let mut bad = sig;
        bad[sig_len - 1] ^= 0xff;
        let res = tee_cryp_asymm_verify(st_vfy, data, &bad[..sig_len]);
        assert_eq!(res.err(), Some(TEE_ERROR_BAD_PARAMETERS));
    }

    #[unittest::def_test(custom)]
    fn test_cryp_rsa_nopad_sign_operate_rejected() {
        let (kp_id, _pub_id) = install_rsa_test_der_key_objects();

        let mut st: u32 = 0;
        let res = tee_cryp_state_alloc(
            TEE_ALG_RSA_NOPAD,
            TEE_OperationMode::TEE_MODE_SIGN,
            Some(kp_id),
            None,
            &mut st,
        );
        assert!(res.is_ok());

        let data = b"SIGNATURE TEST SIGNATURE TEST SI";
        let mut out = [0u8; 256];
        let res = tee_cryp_asymm_operate(st, data, &mut out, None);
        assert_eq!(res.err(), Some(TEE_ERROR_GENERIC));
    }

    /// regression_4006 case 280：ECDSA P192 VERIFY（NIST 186-2 test vector 1）。
    /// 对齐 host/libutee：`SHA1(ptx)` → `AllocateOperation`（空 key1）→ `SetOperationKey`（copy 公钥）→ verify。
    #[unittest::def_test(custom)]
    fn test_regression_4006_ecdsa_p192_verify_case280() {
        const PTX: [u8; 128] = [
            0x66, 0xe9, 0x8a, 0x16, 0x58, 0x54, 0xcd, 0x07, 0x98, 0x9b, 0x1e, 0xe0, 0xec, 0x3f,
            0x8d, 0xbe, 0x0e, 0xe3, 0xc2, 0xfb, 0x00, 0x51, 0xef, 0x53, 0xa0, 0xbe, 0x03, 0x45,
            0x7c, 0x4f, 0x21, 0xbc, 0xe7, 0xdc, 0x50, 0xef, 0x4d, 0xf3, 0x74, 0x86, 0xc3, 0x20,
            0x7d, 0xfe, 0xe2, 0x6b, 0xde, 0x4e, 0xd6, 0x23, 0x40, 0xcb, 0xb2, 0xda, 0x78, 0x49,
            0x06, 0xb1, 0xb7, 0x83, 0xb4, 0xd6, 0x01, 0xbd, 0xff, 0x4a, 0xe1, 0xa7, 0xe5, 0xe8,
            0x5a, 0x85, 0xaf, 0xa3, 0x20, 0x8d, 0xc6, 0x0f, 0x09, 0x90, 0xc8, 0x23, 0xbe, 0xdd,
            0xdb, 0x3d, 0xb6, 0x63, 0x42, 0x66, 0x65, 0x15, 0x2e, 0xd7, 0xb0, 0x93, 0xd6, 0xbd,
            0xa5, 0x06, 0xc9, 0x3a, 0x69, 0x4b, 0x83, 0xac, 0x71, 0x55, 0x3f, 0x31, 0xf5, 0xcc,
            0x0d, 0x6b, 0xa2, 0xfa, 0x24, 0x80, 0x90, 0xe8, 0x79, 0x65, 0x73, 0xc4, 0x91, 0x5d,
            0x15, 0x86,
        ];
        const SIG: [u8; 48] = [
            0xaf, 0x1f, 0x74, 0x9e, 0x3d, 0xf6, 0x22, 0x0f, 0xf0, 0x4e, 0xfd, 0x17, 0x86, 0x18,
            0xa9, 0x77, 0xe0, 0x83, 0x8b, 0x1b, 0x9d, 0xc1, 0x26, 0xe3, 0x89, 0x90, 0xa0, 0x4c,
            0x6c, 0xc0, 0xff, 0x26, 0x26, 0x4e, 0xcf, 0x8f, 0x78, 0x31, 0x38, 0x1a, 0x9d, 0xbc,
            0x6e, 0x53, 0xcc, 0x8c, 0xc8, 0x60,
        ];
        const PUB_X: [u8; 24] = [
            0x14, 0xf6, 0x97, 0x38, 0x59, 0x96, 0x89, 0xf5, 0x70, 0x6a, 0xb7, 0x13, 0x43, 0xbe,
            0xcc, 0x88, 0x6e, 0xf1, 0x56, 0x9a, 0x2d, 0x11, 0x37, 0xfe,
        ];
        const PUB_Y: [u8; 24] = [
            0x0c, 0xf5, 0xa4, 0x33, 0x90, 0x9e, 0x33, 0x21, 0x7f, 0xb4, 0xdf, 0x6b, 0x95, 0x93,
            0xf7, 0x1d, 0x43, 0xfb, 0x1c, 0x2a, 0x56, 0x53, 0xb7, 0x63,
        ];

        // 1) SHA1(ptx) — 与 regression_4006 VERIFY/SIGN 前置 digest 一致
        let mut hash_state: u32 = 0;
        tee_cryp_state_alloc(
            TEE_ALG_SHA1,
            TEE_OperationMode::TEE_MODE_DIGEST,
            None,
            None,
            &mut hash_state,
        )
        .unwrap();
        tee_cryp_hash_init(hash_state).unwrap();
        tee_cryp_hash_update(hash_state, &PTX).unwrap();
        let mut digest = [0u8; 20];
        let digest_len = tee_cryp_hash_final(hash_state, &[], &mut digest).unwrap();
        assert_eq!(digest_len, 20);
        let _ = tee_cryp_state_free(hash_state);

        // 2) populate 源公钥（regression_4006 create_ecc_keys 的 pub_h）
        let mut src_pub_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        syscall_cryp_obj_alloc(
            TEE_TYPE_ECDSA_PUBLIC_KEY as _,
            192,
            src_pub_id.as_user_ref(),
        )
        .unwrap();
        let src_pub_id = src_pub_id.read();

        let mut usr_x = crate::user_vec![0u8; 24];
        let mut usr_y = crate::user_vec![0u8; 24];
        usr_x.copy_from_slice(&PUB_X);
        usr_y.copy_from_slice(&PUB_Y);

        let mut usr_attrs = crate::user_vec![utee_attribute::default(); 3];
        usr_attrs[0].attribute_id = TEE_ATTR_ECC_CURVE | TEE_ATTR_FLAG_VALUE;
        usr_attrs[0].a = TEE_ECC_CURVE_NIST_P192 as u64;
        usr_attrs[0].b = 0;
        tee_init_ref_attribute(
            &mut usr_attrs[1],
            TEE_ATTR_ECC_PUBLIC_VALUE_X,
            &usr_x[..],
            24,
        );
        tee_init_ref_attribute(
            &mut usr_attrs[2],
            TEE_ATTR_ECC_PUBLIC_VALUE_Y,
            &usr_y[..],
            24,
        );
        syscall_cryp_obj_populate(
            src_pub_id as c_ulong,
            usr_attrs.as_mut_ptr(),
            usr_attrs.len() as c_ulong,
        )
        .unwrap();

        // 3) libutee：AllocateOperation 时为 operation 分配空 key1，再 syscall alloc cryp state
        let mut op_key_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        syscall_cryp_obj_alloc(TEE_TYPE_ECDSA_PUBLIC_KEY as _, 192, op_key_id.as_user_ref())
            .unwrap();
        let op_key_id = op_key_id.read();

        let mut verify_state: u32 = 0;
        tee_cryp_state_alloc(
            TEE_ALG_ECDSA_P192,
            TEE_OperationMode::TEE_MODE_VERIFY,
            Some(op_key_id),
            None,
            &mut verify_state,
        )
        .unwrap();
        let cs = tee_cryp_state_get(verify_state).unwrap();
        assert_eq!(cs.lock().algo, TEE_ALG_ECDSA_SHA1);

        // 4) TEE_SetOperationKey：把 pub_h 属性 copy 到 operation 的 key1
        syscall_cryp_obj_copy(op_key_id as _, src_pub_id as _).unwrap();

        // 5) 验签 — host 日志停在此前的 SET_OPERATION_KEY / VERIFY 之间
        tee_cryp_asymm_verify(verify_state, &digest[..digest_len], &SIG).unwrap();
    }

    /// regression_4006 case 400：ECDSA P521 VERIFY（NIST 186-2 test vector 61）。
    /// P521 为 521 bit，分量/签名固定宽度为 ceil(521/8)=66 字节（总长 132）。
    #[unittest::def_test(custom)]
    fn test_regression_4006_ecdsa_p521_verify_case400() {
        const PTX: [u8; 128] = [
            0xcc, 0x92, 0xca, 0x36, 0xa7, 0x67, 0x60, 0x75, 0x2b, 0x5a, 0x45, 0xca, 0x5d, 0x72,
            0x35, 0x94, 0x71, 0x22, 0xa6, 0x00, 0x2f, 0x1d, 0x4e, 0x7d, 0x9c, 0x6b, 0xe5, 0x70,
            0xd7, 0xbd, 0x2c, 0x29, 0x41, 0xfe, 0x2e, 0x16, 0xe0, 0x2a, 0xc6, 0x37, 0x06, 0x63,
            0x61, 0xd2, 0x2d, 0x42, 0x05, 0x68, 0x26, 0x6b, 0x93, 0xe7, 0x73, 0x64, 0x49, 0x21,
            0xf1, 0xa7, 0x8a, 0x7d, 0xba, 0xf5, 0xe2, 0xed, 0x49, 0xee, 0x45, 0x20, 0xdf, 0xdf,
            0x97, 0xf8, 0x26, 0xdb, 0x72, 0x3e, 0x14, 0x0d, 0x23, 0x95, 0x13, 0x4c, 0xf5, 0xac,
            0x5f, 0xf0, 0xb3, 0xb8, 0xaf, 0xe4, 0x68, 0x22, 0x17, 0xfd, 0x69, 0x7c, 0x2d, 0x8a,
            0x95, 0xba, 0x6b, 0x2d, 0xdc, 0x9f, 0xd4, 0xe9, 0xfe, 0x75, 0xda, 0x7b, 0x95, 0x01,
            0x80, 0xee, 0x56, 0xb6, 0xbc, 0x6a, 0x94, 0x29, 0x1f, 0x4d, 0x05, 0xc5, 0xb7, 0x7c,
            0xc9, 0xc0,
        ];
        const SIG: [u8; 132] = [
            0x00, 0xd3, 0x14, 0xdd, 0xe7, 0x4c, 0xce, 0x60, 0x24, 0x51, 0x89, 0x80, 0xad, 0x85,
            0xcc, 0x7d, 0x5a, 0x29, 0x4e, 0x14, 0x8f, 0xa2, 0x6f, 0x06, 0x48, 0x48, 0x6a, 0x6d,
            0x28, 0x82, 0xca, 0x7a, 0x92, 0xa1, 0xc9, 0x34, 0xc4, 0xb0, 0x1e, 0xe1, 0xf6, 0xcc,
            0x1d, 0xcc, 0x59, 0x20, 0xd4, 0x97, 0x19, 0xa1, 0x82, 0x3c, 0xfa, 0x32, 0xa6, 0x9c,
            0xda, 0x71, 0x0b, 0x0e, 0x95, 0x62, 0x3b, 0xbb, 0x04, 0x51, 0x01, 0x4b, 0x0b, 0x93,
            0xbd, 0xa1, 0x37, 0xa5, 0x29, 0x39, 0x00, 0xeb, 0x6c, 0xb6, 0xb1, 0x51, 0xe3, 0x30,
            0x1b, 0x8e, 0x29, 0x44, 0xea, 0xee, 0x5c, 0xe0, 0xf8, 0xdf, 0x87, 0xc9, 0x84, 0x1b,
            0x61, 0x37, 0x2a, 0x2d, 0x70, 0xe7, 0x75, 0xc6, 0x75, 0x8d, 0x29, 0xa7, 0xd2, 0x4f,
            0x62, 0xc6, 0x9d, 0xc8, 0x84, 0xb5, 0x4c, 0xe6, 0x7a, 0x8e, 0xdb, 0x51, 0xa0, 0x72,
            0xe4, 0x79, 0x7a, 0x9b, 0x03, 0x6d,
        ];
        const PUB_X: [u8; 66] = [
            0x00, 0xf1, 0xdc, 0x7c, 0xcb, 0x09, 0xd6, 0x1e, 0x6a, 0xf3, 0x79, 0xb8, 0x9a, 0xca,
            0x90, 0x5b, 0x49, 0x77, 0x9f, 0xbe, 0x43, 0xa9, 0x4c, 0x8e, 0xf3, 0x84, 0xcc, 0xbf,
            0x66, 0x0f, 0x48, 0x05, 0xc9, 0x65, 0xa3, 0xa2, 0x4e, 0xd5, 0xa9, 0x62, 0xc2, 0x48,
            0x09, 0x41, 0x5c, 0xde, 0xcf, 0xdf, 0xe5, 0x0f, 0xd1, 0x8f, 0x12, 0x66, 0x07, 0x31,
            0x54, 0xb6, 0x2f, 0x35, 0x5f, 0xe4, 0xc9, 0x8a, 0xf6, 0xe5,
        ];
        const PUB_Y: [u8; 66] = [
            0x01, 0x74, 0x0e, 0xb9, 0x5b, 0x8e, 0x31, 0xa0, 0x43, 0x4c, 0x98, 0x8f, 0x2e, 0xdd,
            0x55, 0x0b, 0x8d, 0xc6, 0xc4, 0x5c, 0x6f, 0x50, 0x43, 0x09, 0x25, 0x53, 0x70, 0xcc,
            0xe5, 0x7e, 0x82, 0x1f, 0xcb, 0x4f, 0x60, 0xba, 0xd1, 0x7a, 0x8f, 0xb9, 0xa3, 0xf4,
            0xdc, 0x67, 0xed, 0x48, 0x60, 0xae, 0x6d, 0xd3, 0xed, 0x4b, 0x1f, 0x51, 0xb9, 0x84,
            0x51, 0xb7, 0xe7, 0x09, 0x5c, 0xc8, 0x7d, 0x4d, 0x62, 0x79,
        ];

        // regression_4006 对 ECDSA 统一用 SHA1 做 ptx 摘要
        let mut hash_state: u32 = 0;
        tee_cryp_state_alloc(
            TEE_ALG_SHA1,
            TEE_OperationMode::TEE_MODE_DIGEST,
            None,
            None,
            &mut hash_state,
        )
        .unwrap();
        tee_cryp_hash_init(hash_state).unwrap();
        tee_cryp_hash_update(hash_state, &PTX).unwrap();
        let mut digest = [0u8; 20];
        let digest_len = tee_cryp_hash_final(hash_state, &[], &mut digest).unwrap();
        assert_eq!(digest_len, 20);
        let _ = tee_cryp_state_free(hash_state);

        let mut src_pub_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        syscall_cryp_obj_alloc(
            TEE_TYPE_ECDSA_PUBLIC_KEY as _,
            521,
            src_pub_id.as_user_ref(),
        )
        .unwrap();
        let src_pub_id = src_pub_id.read();

        let mut usr_x = crate::user_vec![0u8; 66];
        let mut usr_y = crate::user_vec![0u8; 66];
        usr_x.copy_from_slice(&PUB_X);
        usr_y.copy_from_slice(&PUB_Y);

        let mut usr_attrs = crate::user_vec![utee_attribute::default(); 3];
        usr_attrs[0].attribute_id = TEE_ATTR_ECC_CURVE | TEE_ATTR_FLAG_VALUE;
        usr_attrs[0].a = TEE_ECC_CURVE_NIST_P521 as u64;
        usr_attrs[0].b = 0;
        tee_init_ref_attribute(
            &mut usr_attrs[1],
            TEE_ATTR_ECC_PUBLIC_VALUE_X,
            &usr_x[..],
            66,
        );
        tee_init_ref_attribute(
            &mut usr_attrs[2],
            TEE_ATTR_ECC_PUBLIC_VALUE_Y,
            &usr_y[..],
            66,
        );
        syscall_cryp_obj_populate(
            src_pub_id as c_ulong,
            usr_attrs.as_mut_ptr(),
            usr_attrs.len() as c_ulong,
        )
        .unwrap();

        let mut op_key_id = TestUserValue::<c_uint>::from_value(0).unwrap();
        syscall_cryp_obj_alloc(TEE_TYPE_ECDSA_PUBLIC_KEY as _, 521, op_key_id.as_user_ref())
            .unwrap();
        let op_key_id = op_key_id.read();

        let mut verify_state: u32 = 0;
        tee_cryp_state_alloc(
            TEE_ALG_ECDSA_P521,
            TEE_OperationMode::TEE_MODE_VERIFY,
            Some(op_key_id),
            None,
            &mut verify_state,
        )
        .unwrap();
        let cs = tee_cryp_state_get(verify_state).unwrap();
        assert_eq!(cs.lock().algo, TEE_ALG_ECDSA_SHA512);

        syscall_cryp_obj_copy(op_key_id as _, src_pub_id as _).unwrap();

        tee_cryp_asymm_verify(verify_state, &digest[..digest_len], &SIG).unwrap();
    }
}

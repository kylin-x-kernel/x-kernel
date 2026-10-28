// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc, vec::Vec};

use ksync::Mutex;
use osvm::MemError;
use posix_types::{UserConstPtr, UserPtr};
use tee_raw_sys::*;

use super::{
    TeeResult,
    crypto::crypto::{
        crypto_hash_alloc_ctx, crypto_hash_final, crypto_hash_init, crypto_hash_update,
        crypto_mac_alloc_ctx, crypto_mac_final, crypto_mac_init, crypto_mac_update,
    },
    libutee::utee_defines::{
        TEE_CHAIN_MODE_XTS, tee_alg_get_chain_mode, tee_alg_get_class, tee_alg_get_main_alg,
    },
    tee_obj::{TeeObj, TeeObjIdType, tee_obj_close, tee_obj_get},
    tee_svc_cryp::{
        KernelAttribute, TeeCryptObj, check_user_attr_count, copy_in_attrs, load_user_attrs,
        tee_obj_attr_clear,
    },
    user_ta::user_ta_ctx,
    vm::vm_check_access_rights,
};
use crate::tee::{
    TEE_ALG_DES3_CMAC, TEE_ALG_RSAES_PKCS1_OAEP_MGF1_MD5, TEE_ALG_RSASSA_PKCS1_PSS_MGF1_MD5,
    TEE_ALG_RSASSA_PKCS1_V1_5, TEE_ALG_SHA3_224, TEE_ALG_SHA3_256, TEE_ALG_SHA3_384,
    TEE_ALG_SHA3_512, TEE_TYPE_CONCAT_KDF_Z, TEE_TYPE_HKDF_IKM, TEE_TYPE_PBKDF2_PASSWORD, crypto,
    crypto::crypto::{
        crypto_acipher_ecc_sign, crypto_acipher_ecc_verify, crypto_acipher_ed25519_sign,
        crypto_acipher_ed25519_verify, crypto_acipher_rsaes_decrypt, crypto_acipher_rsaes_encrypt,
        crypto_acipher_rsanopad_decrypt, crypto_acipher_rsanopad_encrypt,
        crypto_acipher_rsassa_sign, crypto_acipher_rsassa_verify, crypto_acipher_sm2_pke_decrypt,
        crypto_acipher_sm2_pke_encrypt, crypto_authenc_dec_final, crypto_authenc_enc_final,
        crypto_authenc_init, crypto_authenc_update_aad, crypto_cipher_final, crypto_cipher_init,
        crypto_cipher_max_output_len, crypto_cipher_update, crypto_ecc_init, crypto_rsa_init,
        crypto_xts_cipher_final, tee_cryp_derive_key,
    },
    memtag::{memtag_strip_tag, memtag_strip_tag_const},
    rng_software::crypto_rng_read,
    tee_session::{with_tee_session_ctx, with_tee_session_ctx_mut},
    utee_defines::{
        TEE_AES_BLOCK_SIZE, TEE_DES_BLOCK_SIZE, TEE_MD5_HASH_SIZE, TEE_SHA1_HASH_SIZE,
        TEE_SHA224_HASH_SIZE, TEE_SHA256_HASH_SIZE, TEE_SHA384_HASH_SIZE, TEE_SHA512_HASH_SIZE,
        TEE_SM3_HASH_SIZE,
    },
};

/// Lifecycle of a TEE crypto operation handle.
///
/// # Variants
/// * `Initialized` — ready for update / final
/// * `Uninitialized` — allocated but not yet initialized
/// * `Finalized` — digest/MAC `final` has completed; further `update` is
///   `TEE_ERROR_BAD_STATE` (GP DoFinal). The live ctx is kept (clone-finalize)
///   so DigestExtract → CopyOperation still works.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum CrypState {
    Initialized = 0,
    Uninitialized,
    Finalized,
}

/// Rust equivalent of the tee_cryp_state struct
///
/// This structure represents the state of a cryptographic operation in the TEE environment.
/// It contains all the necessary information to manage an active cryptographic operation,
/// including the algorithm, keys, and context-specific data.
///
/// # Fields
/// * `algo` - The cryptographic algorithm identifier (e.g., TEE_ALG_AES_ECB_NOPAD)
/// * `mode` - The operation mode (e.g., encrypt, decrypt, sign, verify)
/// * `key1` - Virtual address of the first key used in the operation (Vaddr is typically usize in Rust)
/// * `key2` - Virtual address of the second key used in the operation (for algorithms requiring multiple keys)
/// * `ctx` - A trait object containing the specific context data for the algorithm
/// * `state` - Current lifecycle state (`Initialized` / `Uninitialized` / `Finalized`)
/// * `id` - Unique identifier for this cryptographic state instance
#[repr(C)]
pub(crate) struct TeeCrypState {
    pub algo: u32,
    pub mode: TEE_OperationMode,
    pub key1: Option<u32>,
    pub key2: Option<u32>,
    pub ctx: CrypCtx,
    pub state: CrypState,
    pub id: u32,
}

#[allow(clippy::large_enum_variant)]
pub(crate) enum CrypCtx {
    CipherCtx(tee_crypto::streaming_cipher::StreamingCipherCtx),
    XtsCtx(crate::tee::crypto::aes_xts::TeeCipherXtsCtx),
    HashCtx(HashContext),
    AsyCtx(crate::tee::crypto::crypto::AsymmetricCtx),
    HmacCtx(HmacContext),
    CmacCtx(CmacContext),
    /// Placeholder used between `state_alloc` and the first `*_init` call, and
    /// after `state_free` takes the context out.
    Others,
    /// Set by AEAD `enc_final`/`dec_final` once the tag has been sealed — the
    /// operation can no longer be used or copied.
    Finalized,
}

/// Hash context backed by tee_crypto streaming hash types.
///
/// `Md5` and `Sha1` variants are kept for GlobalPlatform TEE API compatibility
/// only; both are weak and not recommended for new security-sensitive use.
#[derive(Clone)]
pub(crate) enum HashContext {
    /// Legacy — MD5; not recommended.
    Md5(tee_crypto::md5::Md5),
    /// Legacy — SHA-1; not recommended.
    Sha1(tee_crypto::hash::Sha1),
    Sha224(tee_crypto::hash::Sha224),
    Sha256(tee_crypto::hash::Sha256),
    Sha384(tee_crypto::hash::Sha384),
    Sha512(tee_crypto::hash::Sha512),
    Sm3(tee_crypto::hash::Sm3),
}

/// HMAC context backed by tee_crypto streaming MAC types.
///
/// `HmacMd5` and `HmacSha1` variants are kept for GlobalPlatform TEE API
/// compatibility only; both underlying hashes are weak and not recommended
/// for new security-sensitive use.
#[derive(Clone)]
pub(crate) enum HmacContext {
    /// Legacy — HMAC-MD5; not recommended.
    HmacMd5(tee_crypto::mac::HmacMd5),
    /// Legacy — HMAC-SHA-1; not recommended.
    HmacSha1(tee_crypto::mac::HmacSha1),
    HmacSha224(tee_crypto::mac::HmacSha224),
    HmacSha256(tee_crypto::mac::HmacSha256),
    HmacSha384(tee_crypto::mac::HmacSha384),
    HmacSha512(tee_crypto::mac::HmacSha512),
    HmacSm3(tee_crypto::mac::HmacSm3),
}

/// CMAC context backed by tee_crypto streaming MAC types.
#[derive(Clone)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum CmacContext {
    Aes128(tee_crypto::mac::Aes128Cmac),
    Aes192(tee_crypto::mac::Aes192Cmac),
    Aes256(tee_crypto::mac::Aes256Cmac),
    Sm4(tee_crypto::mac::Sm4Cmac),
    Des3(tee_crypto::mac::Des3Cmac),
}

impl Default for TeeCrypState {
    fn default() -> Self {
        Self {
            algo: 0,
            mode: TEE_OperationMode::TEE_MODE_DECRYPT,
            key1: None,
            key2: None,
            ctx: CrypCtx::Others,
            state: CrypState::Uninitialized,
            id: 0,
        }
    }
}

fn map_user_mem_error(err: MemError) -> u32 {
    match err {
        MemError::OutOfMemory => TEE_ERROR_OUT_OF_MEMORY,
        MemError::InvalidAddr | MemError::NoAccess => TEE_ERROR_BAD_PARAMETERS,
        _ => TEE_ERROR_GENERIC,
    }
}

fn read_optional_user_bytes(addr: usize, len: usize) -> TeeResult<Box<[u8]>> {
    if addr == 0 || len == 0 {
        return Ok(Box::new([]));
    }
    Ok(UserConstPtr::<u8>::from(addr)
        .load_vm_vec(len)
        .map_err(map_user_mem_error)?
        .into_boxed_slice())
}

fn read_required_user_bytes(addr: usize, len: usize) -> TeeResult<Box<[u8]>> {
    if addr == 0 || len == 0 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    Ok(UserConstPtr::<u8>::from(addr)
        .load_vm_vec(len)
        .map_err(map_user_mem_error)?
        .into_boxed_slice())
}

fn read_user_output(addr: usize, len: usize) -> TeeResult<Box<[u8]>> {
    if addr == 0 || len == 0 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    Ok(UserConstPtr::<u8>::from(addr)
        .load_vm_vec(len)
        .map_err(map_user_mem_error)?
        .into_boxed_slice())
}

fn read_user_len(addr: usize) -> TeeResult<usize> {
    if addr == 0 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    UserConstPtr::<usize>::from(addr)
        .read_vm()
        .map_err(map_user_mem_error)
}

fn write_user_len(addr: usize, value: usize) -> TeeResult {
    if addr == 0 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    UserPtr::<usize>::from(addr)
        .write_vm(value)
        .map_err(map_user_mem_error)
}

fn write_user_bytes(addr: usize, data: &[u8]) -> TeeResult {
    if data.is_empty() {
        return Ok(());
    }
    if addr == 0 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    UserPtr::<u8>::from(addr)
        .write_vm_slice(data)
        .map_err(map_user_mem_error)
}

/// Syscall wrapper for incremental crypt ops: optional input memref + sized output memref.
///
/// `tee_scn_cipher_update` and `tee_scn_authenc_update_payload` share this 5-arg layout
/// (state, src, src_len, dst, dst_len); only the kernel update function differs.
fn syscall_cryp_inout_update(
    arg0: usize,
    arg1: usize,
    arg2: usize,
    arg3: usize,
    arg4: usize,
    update: fn(u32, &[u8], &mut [u8]) -> TeeResult<usize>,
) -> TeeResult {
    let input = read_optional_user_bytes(arg1, arg2)?;
    let mut out_len = read_user_len(arg4)?;
    let mut output = read_user_output(arg3, out_len)?;
    let id = arg0 as u32;
    out_len = update(id, &input, &mut output)?;
    write_user_len(arg4, out_len)?;
    write_user_bytes(arg3, &output[..out_len])?;
    Ok(())
}

#[allow(dead_code)]
pub(crate) enum CipherPaddingMode {
    Pkcs7,
    IsoIec78164,
    AnsiX923,
    Zeros,
    None,
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

pub(crate) fn translate_compat_algo(algo: u32) -> u32 {
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

#[expect(
    clippy::too_many_lines,
    reason = "CG-028-focused-functions: linear algorithm-to-key-type table: one short match arm \
              per algorithm class"
)]
fn tee_svc_cryp_check_key_type(o: &TeeObj, algo: u32, mode: TEE_OperationMode) -> TeeResult {
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
            req_key_type2 = TEE_TYPE_SM2_KEP_KEYPAIR;
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

fn tee_cryp_asymm_init(
    cs: Arc<Mutex<TeeCrypState>>,
    algo: u32,
    mode: TEE_OperationMode,
) -> TeeResult {
    match algo {
        TEE_ALG_RSA_NOPAD => match mode {
            TEE_OperationMode::TEE_MODE_ENCRYPT | TEE_OperationMode::TEE_MODE_DECRYPT => {
                crypto_rsa_init(cs.clone(), mode)
            }
            _ => Err(TEE_ERROR_GENERIC),
        },
        TEE_ALG_SM2_PKE => match mode {
            TEE_OperationMode::TEE_MODE_ENCRYPT | TEE_OperationMode::TEE_MODE_DECRYPT => {
                crypto_ecc_init(cs.clone(), true)
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
                crypto_rsa_init(cs.clone(), mode)
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
                crypto_rsa_init(cs.clone(), mode)
            }
            _ => Err(TEE_ERROR_GENERIC),
        },
        TEE_ALG_ECDSA_SHA1 | TEE_ALG_ECDSA_SHA224 | TEE_ALG_ECDSA_SHA256 | TEE_ALG_ECDSA_SHA384
        | TEE_ALG_ECDSA_SHA512 | TEE_ALG_SM2_DSA_SM3 => {
            crypto_ecc_init(cs.clone(), algo == TEE_ALG_SM2_DSA_SM3)
        }
        _ => Ok(()),
    }
}

/// OP-TEE: `pkcs1_get_salt_len()` — when absent, salt length defaults to digest size.
fn pkcs1_get_salt_len(params: Option<&[KernelAttribute]>, default_len: usize) -> usize {
    params
        .into_iter()
        .flatten()
        .find_map(|attr| match attr {
            KernelAttribute::Value { id, a, .. } if *id == TEE_ATTR_RSA_PSS_SALT_LENGTH => {
                Some(*a as usize)
            }
            _ => None,
        })
        .unwrap_or(default_len)
}

fn rsassa_expected_digest_len(algo: u32) -> TeeResult<usize> {
    let hash_algo = match algo {
        TEE_ALG_RSASSA_PKCS1_V1_5_MD5 | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_MD5 => TEE_ALG_MD5,
        TEE_ALG_RSASSA_PKCS1_V1_5_SHA1 | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA1 => TEE_ALG_SHA1,
        TEE_ALG_RSASSA_PKCS1_V1_5_SHA224 | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA224 => TEE_ALG_SHA224,
        TEE_ALG_RSASSA_PKCS1_V1_5_SHA256 | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA256 => TEE_ALG_SHA256,
        TEE_ALG_RSASSA_PKCS1_V1_5_SHA384 | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA384 => TEE_ALG_SHA384,
        TEE_ALG_RSASSA_PKCS1_V1_5_SHA512 | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA512 => TEE_ALG_SHA512,
        _ => return Err(TEE_ERROR_NOT_SUPPORTED),
    };
    let mut size = 0;
    tee_alg_get_digest_size(hash_algo, &mut size)?;
    Ok(size)
}

fn tee_cryp_asymm_require_key_initialized(cs: &TeeCrypState) -> TeeResult {
    let key1 = cs.key1.ok_or(TEE_ERROR_BAD_PARAMETERS)?;
    let obj = tee_obj_get(key1 as TeeObjIdType)?;
    let obj_guard = obj.lock();
    if obj_guard.info.handleFlags & TEE_HANDLE_FLAG_INITIALIZED == 0 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    Ok(())
}

// 创建一个TeeCrypState
pub(super) fn tee_cryp_state_alloc(
    algo: u32,
    mode: TEE_OperationMode,
    key1: Option<u32>,
    key2: Option<u32>,
    state: &mut u32,
) -> TeeResult {
    let algo = translate_compat_algo(algo);
    let mut cs = TeeCrypState::default();

    // Track keys we marked busy so error paths can roll them back.
    let mut busy_key1: Option<u32> = None;
    let mut busy_key2: Option<u32> = None;

    let res = (|| -> TeeResult {
        // 判断密钥对象是否存在，并取出密钥对象
        let mut o1_ok = false;
        let mut o2_ok = false;
        if let Some(key1) = key1
            && let Ok(obj1_arc) = tee_obj_get(key1 as TeeObjIdType)
        {
            o1_ok = true;
            let mut o1 = obj1_arc.lock();
            if o1.busy {
                return Err(TEE_ERROR_BUSY);
            }
            o1.busy = true;
            busy_key1 = Some(key1);
            cs.key1 = Some(o1.info.objectId);
            tee_svc_cryp_check_key_type(&o1, algo, mode)?;
        }

        if let Some(key2) = key2
            && let Ok(obj2_arc) = tee_obj_get(key2 as TeeObjIdType)
        {
            o2_ok = true;
            let mut o2 = obj2_arc.lock();
            if o2.busy {
                return Err(TEE_ERROR_BUSY);
            }
            o2.busy = true;
            busy_key2 = Some(key2);
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

        // GP: crypto_mac_alloc_ctx / crypto_hash_alloc_ctx at state alloc time.
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
    })();

    if res.is_err() {
        for id in [busy_key1, busy_key2].into_iter().flatten() {
            if let Ok(obj) = tee_obj_get(id as TeeObjIdType) {
                obj.lock().busy = false;
            }
        }
    }
    res
}

pub(super) fn syscall_cryp_state_alloc(
    arg0: usize,
    arg1: usize,
    arg2: usize,
    arg3: usize,
    arg4: usize,
) -> TeeResult {
    let mut state = UserPtr::<u32>::from(arg4)
        .read_vm()
        .map_err(map_user_mem_error)?;
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
    UserPtr::<u32>::from(arg4)
        .write_vm(state)
        .map_err(map_user_mem_error)?;
    Ok(())
}

// 复制一个TeeCrypState
pub(super) fn tee_cryp_state_copy(dst_id: u32, src_id: u32) -> TeeResult {
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

    let copied_ctx = match &src_guard.ctx {
        CrypCtx::CipherCtx(cipher) => CrypCtx::CipherCtx(cipher.clone()),
        CrypCtx::XtsCtx(xts) => CrypCtx::XtsCtx(xts.clone()),
        CrypCtx::CmacCtx(cmac) => CrypCtx::CmacCtx(cmac.clone()),
        CrypCtx::HashCtx(hash) => CrypCtx::HashCtx(hash.clone()),
        CrypCtx::HmacCtx(hmac) => CrypCtx::HmacCtx(hmac.clone()),
        _ => return Err(TEE_ERROR_BAD_STATE),
    };
    dst_guard.ctx = copied_ctx;
    dst_guard.state = src_guard.state;

    Ok(())
}

pub(super) fn syscall_cryp_state_copy(arg0: usize, arg1: usize) -> TeeResult {
    tee_cryp_state_copy(arg0 as _, arg1 as _)
}

// 删除一个TeeCrypState：先丢弃运算上下文（内含轮密钥等副本），再移除状态节点，最后清零并关闭密钥对象。
// GP FreeOperation is void / must succeed: clean up best-effort and surface the first error.
pub(super) fn tee_cryp_state_free(id: u32) -> TeeResult {
    let cs = tee_cryp_state_get(id)?;
    let (key1, key2) = {
        let mut g = cs.lock();
        let key1 = g.key1;
        let key2 = g.key2;
        // `crypto_cipher_init` 等会把密钥物化进运算上下文；仅对 TeeObj 做 attr_clear 无法清理这些副本
        let _ = core::mem::replace(&mut g.ctx, CrypCtx::Others);
        (key1, key2)
    };

    let mut first_err: Option<u32> = None;

    if let Err(e) = cryp_state_free(id) {
        first_err = Some(e);
    }

    for key in [key1, key2].into_iter().flatten() {
        match tee_obj_get(key as _) {
            Ok(o) => {
                let clear_res = {
                    let mut o_guard = o.lock();
                    o_guard.busy = false;
                    tee_obj_attr_clear(&mut o_guard)
                };
                if first_err.is_none()
                    && let Err(e) = clear_res
                {
                    first_err = Some(e);
                }
                if let Err(e) = tee_obj_close(key)
                    && first_err.is_none()
                {
                    first_err = Some(e);
                }
            }
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }

    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

pub(super) fn syscall_cryp_state_free(arg0: usize) -> TeeResult {
    tee_cryp_state_free(arg0 as _)
}

/// Close all crypto operation states owned by the current TEE session.
pub(super) fn tee_cryp_state_close_all() -> TeeResult {
    let ids: Vec<u32> =
        with_tee_session_ctx(|ctx| Ok(ctx.cryp_state.iter().map(|(id, _)| id as u32).collect()))?;
    for id in ids {
        if let Err(err) = tee_cryp_state_free(id) {
            error!("tee_cryp_state_close_all: tee_cryp_state_free({id}) failed: {err:#010X?}");
        }
    }
    Ok(())
}

// 根据id获取一个TeeCrypState
pub(super) fn tee_cryp_state_get(id: u32) -> TeeResult<Arc<Mutex<TeeCrypState>>> {
    with_tee_session_ctx(|ctx| match ctx.cryp_state.get(id as _) {
        Some(cs) => Ok(Arc::clone(cs)),
        None => Err(TEE_ERROR_ITEM_NOT_FOUND),
    })
}

// 根据id删除一个TeeCrypState
fn cryp_state_free(id: u32) -> TeeResult {
    with_tee_session_ctx_mut(|ctx| {
        if let Some(_cs) = ctx.cryp_state.try_remove(id as usize) {
            tee_debug!("Remove cryp state {}", id);
            Ok(())
        } else {
            tee_debug!("Remove cryp state failed");
            Err(TEE_ERROR_BAD_STATE)
        }
    })?;
    Ok(())
}

pub(super) fn tee_cryp_hash_init(id: u32) -> TeeResult {
    let cs = tee_cryp_state_get(id)?;
    let cs_guard = cs.lock();
    let algo = cs_guard.algo;
    let key1 = cs_guard.key1;
    drop(cs_guard);

    match tee_alg_get_class(algo) {
        TEE_OPERATION_DIGEST => crypto_hash_init(cs.clone()),
        TEE_OPERATION_MAC => {
            let key1 = key1.ok_or(TEE_ERROR_BAD_PARAMETERS)?;
            let o = tee_obj_get(key1 as TeeObjIdType)?;
            let o_guard = o.lock();
            if o_guard.attr.is_empty() {
                return Err(TEE_ERROR_BAD_STATE);
            }

            // 从tee_obj中读取密钥
            if let TeeCryptObj::ObjSecret(k) = &o_guard.attr[0] {
                let key = k.key();
                crypto_mac_init(cs.clone(), key)
            } else {
                Err(TEE_ERROR_BAD_STATE)
            }
        }
        _ => Err(TEE_ERROR_BAD_PARAMETERS),
    }
}

pub(super) fn syscall_hash_init(arg0: usize) -> TeeResult {
    tee_cryp_hash_init(arg0 as _)
}

pub(super) fn tee_cryp_hash_update(id: u32, chunk: &[u8]) -> TeeResult {
    memtag_strip_tag_const()?;
    vm_check_access_rights(0, 0, 0)?;

    let cs = tee_cryp_state_get(id)?;
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

pub(super) fn syscall_hash_update(arg0: usize, arg1: usize, arg2: usize) -> TeeResult {
    let chunk = read_required_user_bytes(arg1, arg2)?;
    tee_cryp_hash_update(arg0 as _, &chunk)
}

pub(super) fn tee_cryp_hash_final(id: u32, chunk: &[u8], hash: &mut [u8]) -> TeeResult<usize> {
    memtag_strip_tag_const()?;
    memtag_strip_tag()?;
    vm_check_access_rights(0, 0, 0)?;

    let cs = tee_cryp_state_get(id)?;
    let cs_guard = cs.lock();
    let algo = cs_guard.algo;
    let state = cs_guard.state;

    // Allow re-final after DigestExtract-style final (clone-finalize). Reject
    // feeding more data once already finalized — that would mutate the live ctx.
    match state {
        CrypState::Initialized => {}
        CrypState::Finalized if chunk.is_empty() => {}
        _ => return Err(TEE_ERROR_BAD_STATE),
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

pub(super) fn syscall_hash_final(
    arg0: usize,
    arg1: usize,
    arg2: usize,
    arg3: usize,
    arg4: usize,
) -> TeeResult {
    let chunk = read_optional_user_bytes(arg1, arg2)?;
    let mut hash_len = read_user_len(arg4)?;
    let mut hash = read_user_output(arg3, hash_len)?;
    hash_len = tee_cryp_hash_final(arg0 as _, &chunk, &mut hash)?;
    write_user_len(arg4, hash_len)?;
    write_user_bytes(arg3, &hash[..hash_len])?;
    Ok(())
}

/// GP 中只支持 NoPad，此处实现了 Padding 模式的拓展
/// 实际使用中，若要保持ALG类型一致，请使用CipherPaddingMode::None作为参数
pub(super) fn tee_cryp_cipher_init(
    id: u32,
    iv: Option<&[u8]>,
    padding_mode: CipherPaddingMode,
) -> TeeResult {
    let cs = tee_cryp_state_get(id)?;
    let cs_guard = cs.lock();
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
        if let TeeCryptObj::ObjSecret(k) = &obj_key1_guard.attr[0] {
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
        if let TeeCryptObj::ObjSecret(k) = &obj_key2_guard.attr[0] {
            key.extend_from_slice(k.key());
        } else {
            return Err(TEE_ERROR_BAD_STATE);
        }
    }

    drop(cs_guard);
    crypto_cipher_init(cs.clone(), key.as_slice(), iv, padding_mode)
}

pub(super) fn syscall_cipher_init(arg0: usize, arg1: usize, arg2: usize) -> TeeResult {
    let iv = read_optional_user_bytes(arg1, arg2)?;
    let iv = if iv.is_empty() { None } else { Some(iv) };
    tee_cryp_cipher_init(arg0 as _, iv.as_deref(), CipherPaddingMode::None)
}

/// 不足一块的输入会缓存在 `TeeCipherCtx::pending`（GP 语义）；
/// `output` 须能容纳本次可能写出的密文字节数（可能为 0）。
pub(super) fn tee_cryp_cipher_update(id: u32, input: &[u8], output: &mut [u8]) -> TeeResult<usize> {
    memtag_strip_tag_const()?;
    memtag_strip_tag()?;
    vm_check_access_rights(0, 0, 0)?;

    let cs = tee_cryp_state_get(id)?;
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

pub(super) fn syscall_cipher_update(
    arg0: usize,
    arg1: usize,
    arg2: usize,
    arg3: usize,
    arg4: usize,
) -> TeeResult {
    syscall_cryp_inout_update(arg0, arg1, arg2, arg3, arg4, tee_cryp_cipher_update)
}

/// 用于处理最后一个数据块的填充和加密
pub(super) fn tee_cryp_cipher_final(id: u32, input: &[u8], output: &mut [u8]) -> TeeResult<usize> {
    memtag_strip_tag_const()?;
    memtag_strip_tag()?;
    vm_check_access_rights(0, 0, 0)?;

    let cs = tee_cryp_state_get(id)?;
    let cs_guard = cs.lock();

    if cs_guard.state != CrypState::Initialized {
        return Err(TEE_ERROR_BAD_STATE);
    }

    // XTS ciphertext stealing needs the tail input processed with `is_final=true`;
    // routing it through `tee_cryp_cipher_update` first would clear that flag.
    let is_xts = matches!(cs_guard.ctx, CrypCtx::XtsCtx(_));
    drop(cs_guard);

    if is_xts {
        return crypto_xts_cipher_final(cs, input, output);
    }

    let mut len = 0;
    if !input.is_empty() {
        len = tee_cryp_cipher_update(id, input, output)?;
    }

    let tail = crypto_cipher_final(cs.clone(), &mut output[len..])?;

    Ok(len + tail)
}

pub(super) fn syscall_cipher_final(
    arg0: usize,
    arg1: usize,
    arg2: usize,
    arg3: usize,
    arg4: usize,
) -> TeeResult {
    let src = read_optional_user_bytes(arg1, arg2)?;
    let mut dst_len = read_user_len(arg4)?;
    let mut dst = read_user_output(arg3, dst_len)?;
    let id = arg0 as u32;
    dst_len = tee_cryp_cipher_final(id, &src, &mut dst)?;
    write_user_len(arg4, dst_len)?;
    write_user_bytes(arg3, &dst[..dst_len])?;
    Ok(())
}

pub(super) fn syscall_cryp_random_number_generate(arg0: usize, arg1: usize) -> TeeResult {
    let mut buf = read_user_output(arg0, arg1)?;
    crypto_rng_read(&mut buf)?;
    write_user_bytes(arg0, &buf)?;
    Ok(())
}

pub(super) fn tee_cryp_authenc_init(
    id: u32,
    nonce: &[u8],
    aad_len: Option<usize>,
    tag_len: Option<usize>,
    payload_len: Option<usize>,
) -> TeeResult {
    let cs = tee_cryp_state_get(id)?;
    let cs_guard = cs.lock();
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
        if let TeeCryptObj::ObjSecret(k) = &obj_key1_guard.attr[0] {
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

pub(super) fn syscall_authenc_init(
    arg0: usize,
    arg1: usize,
    arg2: usize,
    arg3: usize,
    arg4: usize,
    arg5: usize,
) -> TeeResult {
    let nonce = read_required_user_bytes(arg1, arg2)?;
    let aad_len = if arg4 == 0 { None } else { Some(arg4) };
    let tag_len = if arg3 == 0 { None } else { Some(arg3) };
    let payload_len = if arg5 == 0 { None } else { Some(arg5) };

    tee_cryp_authenc_init(arg0 as _, &nonce, aad_len, tag_len, payload_len)
}

pub(super) fn tee_cryp_authenc_update_aad(id: u32, aad: &[u8]) -> TeeResult {
    memtag_strip_tag()?;
    vm_check_access_rights(0, 0, 0)?;

    let cs = tee_cryp_state_get(id)?;
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

pub(super) fn syscall_authenc_update_aad(arg0: usize, arg1: usize, arg2: usize) -> TeeResult {
    let aad = read_required_user_bytes(arg1, arg2)?;
    tee_cryp_authenc_update_aad(arg0 as _, &aad)
}

pub(super) fn tee_cryp_authenc_update_payload(
    id: u32,
    input: &[u8],
    output: &mut [u8],
) -> TeeResult<usize> {
    memtag_strip_tag_const()?;
    memtag_strip_tag()?;
    vm_check_access_rights(0, 0, 0)?;

    let cs = tee_cryp_state_get(id)?;
    {
        let cs_guard = cs.lock();
        if cs_guard.state != CrypState::Initialized {
            return Err(TEE_ERROR_BAD_STATE);
        }
    }

    let max_out = crypto_cipher_max_output_len(cs.clone(), input.len())?;
    if output.len() < max_out {
        return Err(TEE_ERROR_SHORT_BUFFER);
    }

    crypto::crypto::crypto_authenc_update_payload(cs, input, output)
}

pub(super) fn syscall_authenc_update_payload(
    arg0: usize,
    arg1: usize,
    arg2: usize,
    arg3: usize,
    arg4: usize,
) -> TeeResult {
    syscall_cryp_inout_update(
        arg0,
        arg1,
        arg2,
        arg3,
        arg4,
        tee_cryp_authenc_update_payload,
    )
}

#[cfg(unittest)]
pub(super) fn tee_cryp_authenc_enc_final(
    id: u32,
    input: Option<&[u8]>,
    output: &mut [u8],
    tag: &mut [u8],
) -> TeeResult<usize> {
    let mut required_dst = 0usize;
    let mut required_tag = 0usize;
    tee_cryp_authenc_enc_final_with_required(
        id,
        input,
        output,
        tag,
        &mut required_dst,
        &mut required_tag,
    )
}

fn tee_cryp_authenc_enc_final_with_required(
    id: u32,
    input: Option<&[u8]>,
    output: &mut [u8],
    tag: &mut [u8],
    required_dst: &mut usize,
    required_tag: &mut usize,
) -> TeeResult<usize> {
    memtag_strip_tag_const()?;
    memtag_strip_tag()?;
    vm_check_access_rights(0, 0, 0)?;

    let cs = tee_cryp_state_get(id)?;
    let cs_guard = cs.lock();

    if cs_guard.state != CrypState::Initialized {
        return Err(TEE_ERROR_BAD_STATE);
    }

    drop(cs_guard);
    crypto_authenc_enc_final(cs, input, output, tag, required_dst, required_tag)
}

pub(super) fn syscall_authenc_enc_final(
    arg0: usize,
    arg1: usize,
    arg2: usize,
    arg3: usize,
    arg4: usize,
    arg5: usize,
    arg6: usize,
) -> TeeResult {
    let src = UserConstPtr::<u8>::from(arg1)
        .check_non_null()
        .filter(|_| arg2 != 0)
        .map(|_| read_required_user_bytes(arg1, arg2))
        .transpose()?;
    let mut dst_len = read_user_len(arg4)?;
    let mut dst = read_user_output(arg3, dst_len)?;
    let mut tag_len = read_user_len(arg6)?;
    let mut tag = read_user_output(arg5, tag_len)?;

    let mut required_dst = 0usize;
    let mut required_tag = 0usize;
    let res = match src {
        Some(src) => tee_cryp_authenc_enc_final_with_required(
            arg0 as _,
            Some(&src),
            &mut dst,
            &mut tag,
            &mut required_dst,
            &mut required_tag,
        ),
        None => tee_cryp_authenc_enc_final_with_required(
            arg0 as _,
            None,
            &mut dst,
            &mut tag,
            &mut required_dst,
            &mut required_tag,
        ),
    };

    match res {
        Ok(n) => {
            dst_len = n;
            tag_len = required_tag;
            write_user_len(arg4, dst_len)?;
            write_user_bytes(arg3, &dst[..dst_len])?;
            write_user_len(arg6, tag_len)?;
            write_user_bytes(arg5, &tag[..tag_len])?;
            Ok(())
        }
        Err(TEE_ERROR_SHORT_BUFFER) => {
            write_user_len(arg4, required_dst)?;
            write_user_len(arg6, required_tag)?;
            Err(TEE_ERROR_SHORT_BUFFER)
        }
        Err(e) => Err(e),
    }
}

#[cfg(unittest)]
pub(super) fn tee_cryp_authenc_dec_final(
    id: u32,
    input: Option<&[u8]>,
    output: &mut [u8],
    tag: &[u8],
) -> TeeResult<usize> {
    let mut required_dst = 0usize;
    tee_cryp_authenc_dec_final_with_required(id, input, output, tag, &mut required_dst)
}

fn tee_cryp_authenc_dec_final_with_required(
    id: u32,
    input: Option<&[u8]>,
    output: &mut [u8],
    tag: &[u8],
    required_dst: &mut usize,
) -> TeeResult<usize> {
    memtag_strip_tag_const()?;
    memtag_strip_tag()?;
    vm_check_access_rights(0, 0, 0)?;

    let cs = tee_cryp_state_get(id)?;
    let cs_guard = cs.lock();

    if cs_guard.state != CrypState::Initialized {
        return Err(TEE_ERROR_BAD_STATE);
    }

    drop(cs_guard);
    crypto_authenc_dec_final(cs, input, output, tag, required_dst)
}

pub(super) fn syscall_authenc_dec_final(
    arg0: usize,
    arg1: usize,
    arg2: usize,
    arg3: usize,
    arg4: usize,
    arg5: usize,
    arg6: usize,
) -> TeeResult {
    let src = UserConstPtr::<u8>::from(arg1)
        .check_non_null()
        .filter(|_| arg2 != 0)
        .map(|_| read_required_user_bytes(arg1, arg2))
        .transpose()?;
    let mut dst_len = read_user_len(arg4)?;
    let mut dst = read_user_output(arg3, dst_len)?;
    let tag = read_required_user_bytes(arg5, arg6)?;

    let mut required_dst = 0usize;
    let res = match src {
        Some(src) => tee_cryp_authenc_dec_final_with_required(
            arg0 as _,
            Some(&src),
            &mut dst,
            &tag,
            &mut required_dst,
        ),
        None => tee_cryp_authenc_dec_final_with_required(
            arg0 as _,
            None,
            &mut dst,
            &tag,
            &mut required_dst,
        ),
    };

    match res {
        Ok(n) => {
            dst_len = n;
            write_user_len(arg4, dst_len)?;
            write_user_bytes(arg3, &dst[..dst_len])?;
            Ok(())
        }
        Err(TEE_ERROR_SHORT_BUFFER) => {
            write_user_len(arg4, required_dst)?;
            Err(TEE_ERROR_SHORT_BUFFER)
        }
        Err(e) => Err(e),
    }
}

/// GP: `syscall_cryp_derive_key` (Linux sysno 532).
pub(super) fn syscall_cryp_derive_key(
    state: usize,
    usr_params: usize,
    param_count: usize,
    derived_key: usize,
) -> TeeResult {
    let usr_attrs = load_user_attrs(usr_params as *const utee_attribute, param_count)?;
    let attrs = copy_in_attrs(&mut user_ta_ctx::default(), &usr_attrs)?;

    tee_cryp_derive_key(state as u32, &attrs, derived_key as TeeObjIdType)
}

#[cfg(unittest)]
pub(super) fn tee_cryp_asymm_operate(
    id: u32,
    input: &[u8],
    output: &mut [u8],
    label: Option<&[u8]>,
) -> TeeResult<usize> {
    let mut required = 0usize;
    tee_cryp_asymm_operate_with_required(id, input, output, label, None, None, &mut required)
}

pub(super) fn tee_cryp_asymm_operate_with_required(
    id: u32,
    input: &[u8],
    output: &mut [u8],
    label: Option<&[u8]>,
    pss_salt_len: Option<usize>,
    ed25519_params: Option<&[KernelAttribute]>,
    required: &mut usize,
) -> TeeResult<usize> {
    *required = 0;
    memtag_strip_tag_const()?;
    memtag_strip_tag()?;
    vm_check_access_rights(0, 0, 0)?;

    let cs = tee_cryp_state_get(id)?;
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
                crypto_acipher_rsaes_encrypt(cs.clone(), input, output, label, required)
            }
            TEE_OperationMode::TEE_MODE_DECRYPT => {
                crypto_acipher_rsaes_decrypt(cs.clone(), input, output, label, required)
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
            crypto_acipher_rsassa_sign(cs.clone(), input, output, required, pss_salt_len)
        }
        TEE_ALG_DSA_SHA1 | TEE_ALG_DSA_SHA224 | TEE_ALG_DSA_SHA256 => Err(TEE_ERROR_NOT_SUPPORTED),
        TEE_ALG_ED25519 => {
            crypto_acipher_ed25519_sign(cs.clone(), input, output, required, ed25519_params)
        }
        TEE_ALG_ECDSA_SHA1 | TEE_ALG_ECDSA_SHA224 | TEE_ALG_ECDSA_SHA256 | TEE_ALG_ECDSA_SHA384
        | TEE_ALG_ECDSA_SHA512 | TEE_ALG_SM2_DSA_SM3 => {
            crypto_acipher_ecc_sign(cs.clone(), input, output, required)
        }
        _ => Err(TEE_ERROR_NOT_SUPPORTED),
    }
}

/// 与 GP `syscall_asymm_operate` 一致：`arg1` 为 `utee_attribute` 数组指针，`arg2` 为元素个数。
/// 用于 RSA-OAEP 的 `TEE_ATTR_RSA_OAEP_LABEL` 等 memref 参数（从用户态拷入内核后再参与运算）。
pub(super) fn syscall_asymm_operate(
    arg0: usize,
    arg1: usize,
    arg2: usize,
    arg3: usize,
    arg4: usize,
    arg5: usize,
    arg6: usize,
) -> TeeResult {
    let num_params = arg2;
    check_user_attr_count(num_params)?;

    let params_attrs = if num_params == 0 {
        None
    } else {
        let usr_attrs = load_user_attrs(arg1 as *const utee_attribute, num_params)?;
        let usr_attrs_slice = usr_attrs.as_slice();
        Some(copy_in_attrs(&mut user_ta_ctx::default(), usr_attrs_slice)?)
    };

    let mut label_buf: Option<Box<[u8]>> = None;
    let mut pss_salt_len: Option<usize> = None;
    if let Some(ref attrs) = params_attrs {
        for attr in attrs.iter() {
            match attr {
                super::tee_svc_cryp::KernelAttribute::Memref { id, data }
                    if *id == TEE_ATTR_RSA_OAEP_LABEL =>
                {
                    if data.is_empty() {
                        return Err(TEE_ERROR_BAD_PARAMETERS);
                    }
                    label_buf = Some(data.clone());
                }
                super::tee_svc_cryp::KernelAttribute::Value { id, a, .. }
                    if *id == TEE_ATTR_RSA_PSS_SALT_LENGTH =>
                {
                    pss_salt_len = Some(*a as usize);
                }
                _ => {}
            }
        }
    }

    let src_len = arg4;
    let mut dst_len = read_user_len(arg6)?;
    let src = read_required_user_bytes(arg3, src_len)?;

    let mut dst_buf: Box<[u8]>;
    let has_dst_user = dst_len != 0;
    if dst_len == 0 {
        dst_buf = Box::new([]);
    } else {
        dst_buf = read_user_output(arg5, dst_len)?;
    }

    let mut required = 0usize;
    let label_ref = label_buf.as_deref();
    match tee_cryp_asymm_operate_with_required(
        arg0 as _,
        &src,
        &mut dst_buf,
        label_ref,
        pss_salt_len,
        params_attrs.as_deref(),
        &mut required,
    ) {
        Ok(actual) => {
            dst_len = actual;
            write_user_len(arg6, dst_len)?;
            if has_dst_user {
                write_user_bytes(arg5, &dst_buf[..dst_len])?;
            }
            Ok(())
        }
        Err(TEE_ERROR_SHORT_BUFFER) => {
            if required == 0 {
                return Err(TEE_ERROR_SHORT_BUFFER);
            }
            write_user_len(arg6, required)?;
            Err(TEE_ERROR_SHORT_BUFFER)
        }
        Err(e) => Err(e),
    }
}

/// Kernel-side asymmetric verify on buffers already in kernel memory.
///
/// Corresponds to the crypto dispatch in OP-TEE `syscall_asymm_verify` after user
/// parameters and attributes have been copied in. `params` carries optional
/// algorithm-specific attributes (Ed25519 prehash/context, RSA-PSS salt length, etc.).
pub(super) fn tee_cryp_asymm_verify(
    id: u32,
    data: &[u8],
    signature: &[u8],
    params: Option<&[KernelAttribute]>,
) -> TeeResult {
    memtag_strip_tag()?;
    vm_check_access_rights(0, 0, 0)?;

    let cs = tee_cryp_state_get(id)?;
    let cs_guard = cs.lock();
    let algo = cs_guard.algo;
    let mode = cs_guard.mode;
    let cryp_state = cs_guard.state;
    drop(cs_guard);

    if mode != TEE_OperationMode::TEE_MODE_VERIFY {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    if cryp_state != CrypState::Initialized {
        tee_cryp_asymm_init(cs.clone(), algo, mode)?;
        let mut cs_guard = cs.lock();
        cs_guard.state = CrypState::Initialized;
        drop(cs_guard);
    }

    {
        let cs_guard = cs.lock();
        tee_cryp_asymm_require_key_initialized(&cs_guard)?;
    }

    match tee_alg_get_main_alg(algo) {
        TEE_MAIN_ALGO_RSA => {
            if algo != TEE_ALG_RSASSA_PKCS1_V1_5 {
                let hash_size = rsassa_expected_digest_len(algo)?;
                if data.len() != hash_size {
                    return Err(TEE_ERROR_BAD_PARAMETERS);
                }
                let _salt_len = pkcs1_get_salt_len(params, hash_size);
            }
            crypto_acipher_rsassa_verify(cs.clone(), data, signature)
        }
        TEE_MAIN_ALGO_DSA => Err(TEE_ERROR_NOT_SUPPORTED),
        TEE_MAIN_ALGO_ED25519 => crypto_acipher_ed25519_verify(cs.clone(), data, signature, params),
        TEE_MAIN_ALGO_ECDSA | TEE_MAIN_ALGO_SM2_DSA_SM3 => {
            crypto_acipher_ecc_verify(cs.clone(), data, signature)
        }
        _ => Err(TEE_ERROR_NOT_SUPPORTED),
    }
}

/// GP `syscall_asymm_verify`: validate user buffers, copy attributes, then verify.
///
/// OP-TEE keeps the user/kernel boundary in the syscall; algorithm dispatch lives in
/// [`tee_cryp_asymm_verify`].
pub(super) fn syscall_asymm_verify(
    arg0: usize,
    arg1: usize,
    arg2: usize,
    arg3: usize,
    arg4: usize,
    arg5: usize,
    arg6: usize,
) -> TeeResult {
    check_user_attr_count(arg2)?;
    let cs = tee_cryp_state_get(arg0 as _)?;
    if cs.lock().mode != TEE_OperationMode::TEE_MODE_VERIFY {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    let params_attrs = if arg2 == 0 {
        None
    } else {
        let usr_attrs = load_user_attrs(arg1 as *const utee_attribute, arg2)?;
        Some(copy_in_attrs(&mut user_ta_ctx::default(), &usr_attrs)?)
    };

    let data = read_required_user_bytes(arg3, arg4)?;
    let sig = read_required_user_bytes(arg5, arg6)?;

    tee_cryp_asymm_verify(arg0 as _, &data, &sig, params_attrs.as_deref())
}

unittest::path_mod_test!(pub(super) mod tests_cryp, "../../ut-tests/tee/tee_svc_cryp2/tests_cryp.rs");

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, vec, vec::Vec};
use core::mem::size_of;

use cfg_if::cfg_if;
use ksync::Mutex;
use lazy_static::lazy_static;
use mbedtls::{
    cipher,
    cipher::raw::{Cipher, CipherId, CipherMode, Operation},
    hash,
};
use static_assertions::const_assert;
use tee_raw_sys::{
    TEE_ALG_AES_ECB_NOPAD, TEE_ALG_HMAC_SHA256, TEE_ALG_HMAC_SM3, TEE_ALG_SM4_ECB_NOPAD,
    TEE_ERROR_BAD_PARAMETERS, TEE_ERROR_GENERIC, TEE_ERROR_NOT_IMPLEMENTED, TEE_OperationMode,
    TEE_UUID,
};

use super::{
    TeeResult,
    crypto_temp::crypto_hash_temp::tee_alg_to_hmac_type,
    huk_subkey::{HUK_SUBKEY_MAX_LEN, HukSubkeyUsage, huk_subkey_derive},
    otp_stubs::{TeeHwUniqueKey, tee_otp_get_hw_unique_key},
    utee_defines::{TEE_SHA256_HASH_SIZE, TEE_SM3_HASH_SIZE, TeeAlg},
    utils::slice_fmt,
};

const TEE_FS_KM_CHIP_ID_LENGTH: usize = 32;
pub const TEE_FS_KM_FEK_SIZE: usize = 16; /* bytes */

cfg_if::cfg_if! {
    if #[cfg(feature = "tee_ss_smx")] {
        const TEE_FS_KM_SSK_SIZE: usize = TEE_SM3_HASH_SIZE;
        const TEE_FS_KM_TSK_SIZE: usize = TEE_SM3_HASH_SIZE;
        const TEE_FS_KM_HMAC_ALG: u32 = TEE_ALG_HMAC_SM3;
        const TEE_FS_KM_ENC_FEK_ALG: u32 = TEE_ALG_SM4_ECB_NOPAD;
    } else {
        const TEE_FS_KM_SSK_SIZE: usize = TEE_SHA256_HASH_SIZE;
        const TEE_FS_KM_TSK_SIZE: usize = TEE_SHA256_HASH_SIZE;
        const TEE_FS_KM_HMAC_ALG: u32 = TEE_ALG_HMAC_SHA256;
        const TEE_FS_KM_ENC_FEK_ALG: u32 = TEE_ALG_AES_ECB_NOPAD;
    }
}
#[derive(Debug, Clone)]
pub struct TeeFsSsk {
    pub is_init: bool,
    pub key: [u8; TEE_FS_KM_SSK_SIZE],
}

pub static STRING_FOR_SSK_GEN: &[u8] = b"ONLY_FOR_tee_fs_ssk";

const_assert!(TEE_FS_KM_SSK_SIZE <= HUK_SUBKEY_MAX_LEN);

// Helper function to initialize SSK
fn init_ssk() -> TeeFsSsk {
    let mut ssk = TeeFsSsk {
        is_init: false,
        key: [0u8; TEE_FS_KM_SSK_SIZE],
    };

    let res = huk_subkey_derive(HukSubkeyUsage::Ssk, None, &mut ssk.key);

    match res {
        Ok(_) => {
            ssk.is_init = true;
        }
        Err(_) => {
            // If initialization fails, keep is_init = false and key filled with zeros
            ssk.key.fill(0);
            error!("init_ssk: huk_subkey_derive failed");
        }
    }

    tee_debug!("init_ssk: ssk: {:?}", ssk);
    ssk
}

lazy_static! {
    static ref TEE_FS_SSK: Mutex<TeeFsSsk> = Mutex::new(init_ssk());
}

pub fn crypto_cipher_ecb_nopad(
    algo: TeeAlg,
    mode: TEE_OperationMode,
    key: &[u8],
    input: &[u8],
    output: &mut [u8],
) -> TeeResult {
    debug_assert!(key.len() >= 16);

    let (cipher_id, key_bytes) = match algo {
        TEE_ALG_AES_ECB_NOPAD => (CipherId::Aes, key.len()),
        TEE_ALG_SM4_ECB_NOPAD => (CipherId::SM4, 16),
        _ => return Err(TEE_ERROR_NOT_IMPLEMENTED),
    };

    // 根据模式确定操作类型
    let operation = match mode {
        TEE_OperationMode::TEE_MODE_ENCRYPT => Operation::Encrypt,
        TEE_OperationMode::TEE_MODE_DECRYPT => Operation::Decrypt,
        _ => return Err(TEE_ERROR_BAD_PARAMETERS),
    };

    // 使用 raw 接口创建 Cipher 实例
    let mut cipher_ctx = Cipher::setup(cipher_id, CipherMode::ECB, (key_bytes * 8) as u32)
        .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;

    // 设置密钥
    cipher_ctx
        .set_key(operation, &key[..key_bytes])
        .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;

    // 根据模式执行加密或解密
    let _len = match mode {
        TEE_OperationMode::TEE_MODE_ENCRYPT => cipher_ctx.encrypt(input, output),
        TEE_OperationMode::TEE_MODE_DECRYPT => cipher_ctx.decrypt(input, output),
        _ => return Err(TEE_ERROR_BAD_PARAMETERS),
    }
    .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;

    Ok(())
}

pub fn do_hmac(out_key: &mut [u8], in_key: &[u8], message: &[u8]) -> TeeResult {
    // 参数检查
    if out_key.is_empty() || in_key.is_empty() || message.is_empty() {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    let hmac_type = tee_alg_to_hmac_type(TEE_FS_KM_HMAC_ALG)?;

    let mut mac = hash::Hmac::new(hmac_type, in_key).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;

    mac.update(message).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;

    mac.finish(out_key).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;

    Ok(())
}

impl TeeFsSsk {
    #[allow(dead_code)]
    fn is_initial(&self) -> bool {
        TEE_FS_SSK.lock().is_init
    }
}

pub fn tee_fs_fek_crypt(
    uuid: Option<&TEE_UUID>,
    mode: TEE_OperationMode,
    in_key: Option<&[u8]>,
    size: usize,
    out_key: Option<&mut [u8]>,
) -> TeeResult {
    tee_debug!(
        "tee_fs_fek_crypt: uuid: {:?}, mode: {:?}, in_key: {:?}, size: {:?}, out_key: {:?}",
        uuid,
        mode as u32,
        in_key,
        size,
        out_key
    );
    let mut dst_key = vec![0u8; size];
    let mut tsk = [0u8; TEE_FS_KM_TSK_SIZE];

    if in_key.is_none() || out_key.is_none() {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    if size != TEE_FS_KM_FEK_SIZE || size != out_key.as_ref().unwrap().len() {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    // Extract in_key slice before unsafe block
    let in_key_slice = in_key.ok_or(TEE_ERROR_BAD_PARAMETERS)?;

    let ssk = TEE_FS_SSK.lock();
    if !ssk.is_init {
        error!("tee_fs_fek_crypt: TEE_FS_SSK is not initialized");
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    // Use SSK.key as HMAC key, not in_key_slice (FEK)
    // Consistent with C implementation: do_hmac(tsk, sizeof(tsk), tee_fs_ssk.key, TEE_FS_KM_SSK_SIZE, uuid, sizeof(*uuid))
    let ssk_key_slice = &ssk.key[..];

    if let Some(uuid) = uuid {
        let uuid_bytes = unsafe {
            core::slice::from_raw_parts(
                (uuid as *const TEE_UUID) as *const u8,
                size_of::<TEE_UUID>(),
            )
        };
        do_hmac(&mut tsk, ssk_key_slice, uuid_bytes)?;
    } else {
        let dummy = [0u8, 1];
        do_hmac(&mut tsk, ssk_key_slice, &dummy)?;
    }

    // 使用 crypto_cipher_ecb_nopad 函数进行加密或解密
    crypto_cipher_ecb_nopad(
        TEE_FS_KM_ENC_FEK_ALG,
        mode,
        &tsk,
        in_key_slice,
        &mut dst_key,
    )
    .inspect_err(|e| {
        error!("tee_fs_fek_crypt: crypto_cipher_ecb_nopad failed: {:X?}", e);
    })?;

    if let Some(out_key) = out_key {
        out_key.copy_from_slice(&dst_key);
        tee_debug!(
            "tee_fs_fek_crypt: in_key: {:?}, out_key: {:?}",
            hex::encode(in_key_slice),
            hex::encode(out_key)
        );
    }

    Ok(())
}

#[cfg(feature = "tee_test")]
pub mod tests_tee_fs_key_manager {
    use unittest::{
        test_fn, test_framework::TestDescriptor, test_framework_basic::TestResult, tests_name,
    };

    use super::*;
    use crate::tee::utils::slice_fmt;

    test_fn! {
        using TestResult;

        fn test_crypto_cipher_encrypt() {
            // aes encrypt
            let algo = TEE_ALG_AES_ECB_NOPAD;
            let key = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10];
            let plain = [0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E, 0x1F, 0x20];
            let mut cipher = [0u8; 16];
            let result = crypto_cipher_ecb_nopad(algo, TEE_OperationMode::TEE_MODE_ENCRYPT, &key, &plain, &mut cipher);
            tee_debug!("test_crypto_cipher_encrypt: key: {:?}, plain: {:?}, cipher: {:?}", slice_fmt(&key), slice_fmt(&plain), slice_fmt(&cipher));
            assert!(result.is_ok());
            assert_eq!("D721A0F194231822F398706DD1FFF2B7", hex::encode_upper(&cipher));

            // aes decrypt
            let mut decrypted = [0u8; 16];
            let result = crypto_cipher_ecb_nopad(algo, TEE_OperationMode::TEE_MODE_DECRYPT, &key, &cipher, &mut decrypted);
            assert!(result.is_ok());
            assert_eq!(plain, decrypted);

            // sm4 encrypt
            let algo = TEE_ALG_SM4_ECB_NOPAD;
            let result = crypto_cipher_ecb_nopad(algo, TEE_OperationMode::TEE_MODE_ENCRYPT, &key, &plain, &mut cipher);
            tee_debug!("test_crypto_cipher_encrypt: key: {:?}, plain: {:?}, cipher: {:?}", slice_fmt(&key), slice_fmt(&plain), slice_fmt(&cipher));
            assert!(result.is_ok());
            assert_eq!("4329A6241E39AD7A9A404A814A7EDD32", hex::encode_upper(&cipher));

            // sm4 decrypt
            let result = crypto_cipher_ecb_nopad(algo, TEE_OperationMode::TEE_MODE_DECRYPT, &key, &cipher, &mut decrypted);
            assert!(result.is_ok());
            assert_eq!(plain, decrypted);
        }
    }

    tests_name! {
        TEST_TEE_FS_KEY_MANAGER;
        tee_fs_key_manager;
        //------------------------
        test_crypto_cipher_encrypt,
    }
}

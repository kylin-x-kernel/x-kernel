// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, vec, vec::Vec};

use cfg_if::cfg_if;
use ksync::Mutex;
use lazy_static::lazy_static;
use static_assertions::const_assert;
use tee_crypto::{block_cipher::BlockCipher, mac::Mac};
use tee_raw_sys::{
    TEE_ALG_AES_ECB_NOPAD, TEE_ALG_HMAC_SHA256, TEE_ALG_HMAC_SM3, TEE_ALG_SM4_ECB_NOPAD,
    TEE_ERROR_BAD_PARAMETERS, TEE_ERROR_NOT_IMPLEMENTED, TEE_OperationMode, TEE_UUID,
};

use super::{
    TeeResult,
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
    debug_assert_eq!(input.len(), output.len());

    match algo {
        TEE_ALG_AES_ECB_NOPAD => {
            for (in_block, out_block) in input.chunks(16).zip(output.chunks_mut(16)) {
                if in_block.len() != 16 {
                    return Err(TEE_ERROR_BAD_PARAMETERS);
                }
                out_block.copy_from_slice(in_block);
                match mode {
                    TEE_OperationMode::TEE_MODE_ENCRYPT => {
                        tee_crypto::block_cipher::Aes128Ecb::encrypt(&key[..16], out_block)
                            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?
                    }
                    TEE_OperationMode::TEE_MODE_DECRYPT => {
                        tee_crypto::block_cipher::Aes128Ecb::decrypt(&key[..16], out_block)
                            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?
                    }
                    _ => return Err(TEE_ERROR_BAD_PARAMETERS),
                }
            }
        }
        TEE_ALG_SM4_ECB_NOPAD => {
            for (in_block, out_block) in input.chunks(16).zip(output.chunks_mut(16)) {
                if in_block.len() != 16 {
                    return Err(TEE_ERROR_BAD_PARAMETERS);
                }
                out_block.copy_from_slice(in_block);
                match mode {
                    TEE_OperationMode::TEE_MODE_ENCRYPT => {
                        tee_crypto::block_cipher::Sm4Ecb::encrypt(&key[..16], out_block)
                            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?
                    }
                    TEE_OperationMode::TEE_MODE_DECRYPT => {
                        tee_crypto::block_cipher::Sm4Ecb::decrypt(&key[..16], out_block)
                            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?
                    }
                    _ => return Err(TEE_ERROR_BAD_PARAMETERS),
                }
            }
        }
        _ => return Err(TEE_ERROR_NOT_IMPLEMENTED),
    };

    Ok(())
}

pub fn do_hmac(out_key: &mut [u8], in_key: &[u8], message: &[u8]) -> TeeResult {
    // 参数检查
    if out_key.is_empty() || in_key.is_empty() || message.is_empty() {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    let result = match TEE_FS_KM_HMAC_ALG {
        TEE_ALG_HMAC_SM3 => {
            let mut mac =
                tee_crypto::mac::HmacSm3::new(in_key).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
            mac.update(message);
            mac.finalize()
        }
        TEE_ALG_HMAC_SHA256 => {
            let mut mac =
                tee_crypto::mac::HmacSha256::new(in_key).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
            mac.update(message);
            mac.finalize()
        }
        _ => return Err(TEE_ERROR_NOT_IMPLEMENTED),
    };

    out_key.copy_from_slice(&result[..out_key.len()]);
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
        do_hmac(&mut tsk, ssk_key_slice, bytemuck::bytes_of(uuid))?;
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

#[unittest::mod_test]
pub mod tests_tee_fs_key_manager {
    use unittest::{assert, assert_eq};

    use super::*;
    use crate::tee::utils::slice_fmt;

    #[unittest::def_test]
    fn test_crypto_cipher_encrypt() {
        let algo = TEE_ALG_AES_ECB_NOPAD;
        let key = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
            0x0F, 0x10,
        ];
        let plain = [
            0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E,
            0x1F, 0x20,
        ];
        let mut cipher = [0u8; 16];
        let result = crypto_cipher_ecb_nopad(
            algo,
            TEE_OperationMode::TEE_MODE_ENCRYPT,
            &key,
            &plain,
            &mut cipher,
        );
        tee_debug!(
            "test_crypto_cipher_encrypt: key: {:?}, plain: {:?}, cipher: {:?}",
            slice_fmt(&key),
            slice_fmt(&plain),
            slice_fmt(&cipher)
        );
        assert!(result.is_ok());
        assert_eq!(
            "D721A0F194231822F398706DD1FFF2B7",
            hex::encode_upper(cipher)
        );

        let mut decrypted = [0u8; 16];
        let result = crypto_cipher_ecb_nopad(
            algo,
            TEE_OperationMode::TEE_MODE_DECRYPT,
            &key,
            &cipher,
            &mut decrypted,
        );
        assert!(result.is_ok());
        assert_eq!(plain, decrypted);

        let algo = TEE_ALG_SM4_ECB_NOPAD;
        let result = crypto_cipher_ecb_nopad(
            algo,
            TEE_OperationMode::TEE_MODE_ENCRYPT,
            &key,
            &plain,
            &mut cipher,
        );
        tee_debug!(
            "test_crypto_cipher_encrypt: key: {:?}, plain: {:?}, cipher: {:?}",
            slice_fmt(&key),
            slice_fmt(&plain),
            slice_fmt(&cipher)
        );
        assert!(result.is_ok());
        assert_eq!(
            "4329A6241E39AD7A9A404A814A7EDD32",
            hex::encode_upper(cipher)
        );

        let result = crypto_cipher_ecb_nopad(
            algo,
            TEE_OperationMode::TEE_MODE_DECRYPT,
            &key,
            &cipher,
            &mut decrypted,
        );
        assert!(result.is_ok());
        assert_eq!(plain, decrypted);
    }
}

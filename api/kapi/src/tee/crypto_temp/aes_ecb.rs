// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::boxed::Box;

use mbedtls_sys_auto::{
    AES_DECRYPT, AES_ENCRYPT, aes_context, aes_crypt_ecb, aes_free, aes_init, aes_setkey_dec,
    aes_setkey_enc,
};
use tee_raw_sys::{TEE_ERROR_BAD_PARAMETERS, TEE_ERROR_BAD_STATE, TEE_OperationMode};

use super::crypto_hash_temp::{CryptoCipherCtx, CryptoCipherOps};
use crate::tee::{TeeResult, common::array, utee_defines::TEE_AES_BLOCK_SIZE, utils::slice_fmt};

#[repr(C)]
#[derive(Copy, Clone)]
pub struct MbedAesEcbCtx {
    mbed_mode: i32,
    aes_ctx: aes_context,
}

fn mbed_aes_ecb_init(
    ctx: &mut MbedAesEcbCtx,
    mode: TEE_OperationMode,
    key1: Option<&[u8]>,
    _key2: Option<&[u8]>,
    _iv: Option<&[u8]>,
) -> TeeResult {
    tee_debug!("mbed_aes_ecb_init: mode: {:?}, key1: {:?}", mode, key1);
    let (key1_ptr, key1_len) = array::get_const_ptr_and_len(key1);

    unsafe { aes_init(&mut ctx.aes_ctx) };

    let mbed_res = match mode {
        TEE_OperationMode::TEE_MODE_ENCRYPT => {
            ctx.mbed_mode = AES_ENCRYPT;
            unsafe { aes_setkey_enc(&mut ctx.aes_ctx, key1_ptr, key1_len as u32 * 8) }
        }
        TEE_OperationMode::TEE_MODE_DECRYPT => {
            ctx.mbed_mode = AES_DECRYPT;
            unsafe { aes_setkey_dec(&mut ctx.aes_ctx, key1_ptr, key1_len as u32 * 8) }
        }
        _ => {
            return Err(TEE_ERROR_BAD_PARAMETERS);
        }
    };

    if mbed_res != 0 {
        return Err(TEE_ERROR_BAD_STATE);
    }

    Ok(())
}

fn mbed_aes_ecb_update(
    ctx: &mut MbedAesEcbCtx,
    _last_block: bool,
    data: Option<&[u8]>,
    dst: Option<&mut [u8]>,
) -> TeeResult {
    let (data_ptr, data_len) = array::get_const_ptr_and_len(data);
    let (dst_ptr, _dst_len) = array::get_mut_ptr_and_len(dst);

    if data_len % TEE_AES_BLOCK_SIZE != 0 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    tee_debug!(
        "mbed_aes_ecb_update: mode: {:?}, data_len: {:?}, dst_len: {:?}",
        ctx.mbed_mode,
        data_len,
        _dst_len
    );

    // AES ECB processes one block (16 bytes) at a time
    let num_blocks = data_len / TEE_AES_BLOCK_SIZE;
    for i in 0..num_blocks {
        let input_block = unsafe { data_ptr.add(i * TEE_AES_BLOCK_SIZE) };
        let output_block = unsafe { dst_ptr.add(i * TEE_AES_BLOCK_SIZE) };

        let mbed_res =
            unsafe { aes_crypt_ecb(&mut ctx.aes_ctx, ctx.mbed_mode, input_block, output_block) };

        if mbed_res != 0 {
            return Err(TEE_ERROR_BAD_STATE);
        }
    }

    Ok(())
}

fn mbed_aes_ecb_final(ctx: &mut MbedAesEcbCtx) {
    unsafe { aes_free(&mut ctx.aes_ctx as *mut aes_context) };
}

// optee_os, the context is allocated by function crypto_aes_ecb_alloc_ctx, so need free it manually.
// in libmbedtls, the context is owned by Box with function alloc_cipher_ctx, freed automatically
// when it goes out of scope.
// So this function is a no-op in this case.
fn mbed_aes_ecb_free_ctx(_ctx: &mut MbedAesEcbCtx) {}

fn mbed_aes_ecb_copy_state(dst_ctx: &mut MbedAesEcbCtx, src_ctx: &MbedAesEcbCtx) {
    dst_ctx.mbed_mode = src_ctx.mbed_mode;
    dst_ctx.aes_ctx = src_ctx.aes_ctx;
}

impl CryptoCipherOps for MbedAesEcbCtx {
    fn init(
        &mut self,
        mode: TEE_OperationMode,
        key1: Option<&[u8]>,
        key2: Option<&[u8]>,
        iv: Option<&[u8]>,
    ) -> TeeResult {
        mbed_aes_ecb_init(self, mode, key1, key2, iv)
    }

    fn update(
        &mut self,
        last_block: bool,
        data: Option<&[u8]>,
        dst: Option<&mut [u8]>,
    ) -> TeeResult {
        mbed_aes_ecb_update(self, last_block, data, dst)
    }

    fn finalize(&mut self) {
        mbed_aes_ecb_final(self)
    }

    fn free_ctx(&mut self) {
        mbed_aes_ecb_free_ctx(self)
    }

    fn copy_state(&self, dst_ctx: &mut MbedAesEcbCtx) {
        mbed_aes_ecb_copy_state(dst_ctx, self);
    }
}

impl CryptoCipherCtx for MbedAesEcbCtx {
    type Context = MbedAesEcbCtx;

    fn alloc_cipher_ctx() -> Result<Box<Self::Context>, TeeResult> {
        let ctx = MbedAesEcbCtx {
            mbed_mode: 0,
            aes_ctx: aes_context::default(),
        };

        Ok(Box::new(ctx))
    }
}

#[cfg(feature = "tee_test")]
pub mod tests_aes_ecb {
    use hashbrown::hash_map::Keys;
    use unittest::{
        test_fn, test_framework::TestDescriptor, test_framework_basic::TestResult, tests_name,
    };

    use super::*;
    use crate::tee::utils::random_bytes;

    test_fn! {
        using TestResult;

        fn test_tee_aes_ecb_init_update_final() {
            // test encrypt
            let plaintext = [1u8; 16];
            let Key = [2u8; 16];
            let mut ciphertext = [0u8; 16];
            let mut ctx = MbedAesEcbCtx::alloc_cipher_ctx().expect("Failed to allocate AES ECB context");
            let _ = ctx.init(TEE_OperationMode::TEE_MODE_ENCRYPT, Some(&Key), None, None).expect("Failed to initialize AES ECB context");
            let _ = ctx.update(true, Some(&plaintext), Some(&mut ciphertext)).expect("Failed to update AES ECB context");
            ctx.finalize();
            tee_debug!("ciphertext: {:?}", slice_fmt(&ciphertext));
            // test decrypt
            let mut decrypted_text = [0u8; 16];
            let _ = ctx.init(TEE_OperationMode::TEE_MODE_DECRYPT, Some(&Key), None, None).expect("Failed to initialize AES ECB context");
            let _ = ctx.update(true, Some(&ciphertext), Some(&mut decrypted_text)).expect("Failed to update AES ECB context");
            ctx.finalize();
            tee_debug!("decrypted_text: {:?}", slice_fmt(&decrypted_text));
            assert_eq!(decrypted_text, plaintext);
        }
    }

    test_fn! {
        using TestResult;

        fn test_tee_aes_ecb_init_update_final_long() {
            // test encrypt
            let mut plaintext = [1u8; 256];
            // randomize the plaintext, using randChacha
            random_bytes(&mut plaintext);

            let Key = [2u8; 16];
            let mut ciphertext = [0u8; 256];
            let mut ctx = MbedAesEcbCtx::alloc_cipher_ctx().expect("Failed to allocate AES ECB context");
            let _ = ctx.init(TEE_OperationMode::TEE_MODE_ENCRYPT, Some(&Key), None, None).expect("Failed to initialize AES ECB context");
            let _ = ctx.update(true, Some(&plaintext), Some(&mut ciphertext)).expect("Failed to update AES ECB context");
            ctx.finalize();
            // tee_debug!("ciphertext: {:?}", slice_fmt(&ciphertext));
            // test decrypt
            let mut decrypted_text = [0u8; 256];
            let _ = ctx.init(TEE_OperationMode::TEE_MODE_DECRYPT, Some(&Key), None, None).expect("Failed to initialize AES ECB context");
            let _ = ctx.update(true, Some(&ciphertext), Some(&mut decrypted_text)).expect("Failed to update AES ECB context");
            ctx.finalize();
            tee_debug!("decrypted_text: {:?}", slice_fmt(&decrypted_text));
            assert_eq!(decrypted_text, plaintext);
        }
    }

    tests_name! {
        TEST_TEE_AES_ECB;
        //------------------------
        aes_ecb;
        test_tee_aes_ecb_init_update_final,
        test_tee_aes_ecb_init_update_final_long,
    }
}

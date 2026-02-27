// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::boxed::Box;

use mbedtls_sys_auto::{
    sm4_context, sm4_crypt_ecb, sm4_free, sm4_init, sm4_setkey_dec, sm4_setkey_enc,
};
use tee_raw_sys::{TEE_ERROR_BAD_PARAMETERS, TEE_ERROR_BAD_STATE, TEE_OperationMode};

use super::crypto_hash_temp::{CryptoCipherCtx, CryptoCipherOps};
use crate::tee::{
    TeeResult, common::array, crypto_temp::aes_ecb::MbedAesEcbCtx, utee_defines::TEE_SM4_BLOCK_SIZE,
};

#[repr(C)]
#[derive(Copy, Clone)]
pub struct MbedSm4EcbCtx {
    mbed_mode: i32, // 1 for encrypt, 0 for decrypt
    sm4_ctx: sm4_context,
}

fn mbed_sm4_ecb_init(
    ctx: &mut MbedSm4EcbCtx,
    mode: TEE_OperationMode,
    key1: Option<&[u8]>,
    _key2: Option<&[u8]>,
    _iv: Option<&[u8]>,
) -> TeeResult {
    tee_debug!("mbed_sm4_ecb_init: mode: {:?}, key1: {:?}", mode, key1);
    let (key1_ptr, key1_len) = array::get_const_ptr_and_len(key1);

    // if key1_len != 16 {
    //     return Err(TEE_ERROR_BAD_PARAMETERS);
    // }

    unsafe { sm4_init(&mut ctx.sm4_ctx) };

    let mbed_res = match mode {
        TEE_OperationMode::TEE_MODE_ENCRYPT => {
            ctx.mbed_mode = 1; // SM4_ENCRYPT
            unsafe { sm4_setkey_enc(&mut ctx.sm4_ctx, key1_ptr, 128) }
        }
        TEE_OperationMode::TEE_MODE_DECRYPT => {
            ctx.mbed_mode = 0; // SM4_DECRYPT
            unsafe { sm4_setkey_dec(&mut ctx.sm4_ctx, key1_ptr, 128) }
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

fn mbed_sm4_ecb_update(
    ctx: &mut MbedSm4EcbCtx,
    _last_block: bool,
    data: Option<&[u8]>,
    dst: Option<&mut [u8]>,
) -> TeeResult {
    let (data_ptr, data_len) = array::get_const_ptr_and_len(data);
    let (dst_ptr, _dst_len) = array::get_mut_ptr_and_len(dst);

    if data_len % TEE_SM4_BLOCK_SIZE != 0 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    tee_debug!(
        "mbed_sm4_ecb_update: mode: {:?}, data_len: {:?}, dst_len: {:?}",
        ctx.mbed_mode,
        data_len,
        _dst_len
    );

    // SM4 ECB processes one block (16 bytes) at a time
    let num_blocks = data_len / TEE_SM4_BLOCK_SIZE;
    for i in 0..num_blocks {
        let input_block = unsafe { data_ptr.add(i * TEE_SM4_BLOCK_SIZE) };
        let output_block = unsafe { dst_ptr.add(i * TEE_SM4_BLOCK_SIZE) };

        let mbed_res =
            unsafe { sm4_crypt_ecb(&mut ctx.sm4_ctx, ctx.mbed_mode, input_block, output_block) };

        if mbed_res != 0 {
            return Err(TEE_ERROR_BAD_STATE);
        }
    }

    Ok(())
}

fn mbed_sm4_ecb_final(ctx: &mut MbedSm4EcbCtx) {
    unsafe { sm4_free(&mut ctx.sm4_ctx as *mut sm4_context) };
}

fn mbed_sm4_ecb_free_ctx(_ctx: &mut MbedSm4EcbCtx) {}

fn mbed_sm4_ecb_copy_state(dst_ctx: &mut MbedSm4EcbCtx, src_ctx: &MbedSm4EcbCtx) {
    dst_ctx.mbed_mode = src_ctx.mbed_mode;
    dst_ctx.sm4_ctx = src_ctx.sm4_ctx;
}

impl CryptoCipherOps for MbedSm4EcbCtx {
    fn init(
        &mut self,
        mode: TEE_OperationMode,
        key1: Option<&[u8]>,
        key2: Option<&[u8]>,
        iv: Option<&[u8]>,
    ) -> TeeResult {
        mbed_sm4_ecb_init(self, mode, key1, key2, iv).inspect_err(|e| {
            error!("mbed_sm4_ecb_init failed: {:X?}", e);
        })
    }

    fn update(
        &mut self,
        last_block: bool,
        data: Option<&[u8]>,
        dst: Option<&mut [u8]>,
    ) -> TeeResult {
        mbed_sm4_ecb_update(self, last_block, data, dst)
    }

    fn finalize(&mut self) {
        mbed_sm4_ecb_final(self)
    }

    fn free_ctx(&mut self) {
        mbed_sm4_ecb_free_ctx(self)
    }

    fn copy_state(&self, _dst_ctx: &mut MbedAesEcbCtx) {
        // TODO: SM4 copy_state not implemented yet
        // This is a placeholder implementation
    }
}

impl CryptoCipherCtx for MbedSm4EcbCtx {
    type Context = MbedSm4EcbCtx;

    fn alloc_cipher_ctx() -> Result<Box<Self::Context>, TeeResult> {
        let ctx = MbedSm4EcbCtx {
            mbed_mode: 0,
            sm4_ctx: sm4_context::default(),
        };

        Ok(Box::new(ctx))
    }
}

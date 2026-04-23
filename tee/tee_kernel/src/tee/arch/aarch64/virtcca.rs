// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtCCA / TSI SMC helpers (AArch64).

use khal::mem::{VirtAddr, v2p};
use mbedtls::hash::{Hkdf, Md, Type as MdType};
use tee_raw_sys::{TEE_ERROR_BAD_PARAMETERS, TEE_ERROR_SHORT_BUFFER};

use crate::tee::{
    TeeResult,
    utils::{random_bytes, slice_fmt},
};

const SMC_TSI_CALL_BASE: u32 = 0xC400_0000;
const IMAGE_KEY_LEN: usize = 32;
const USER_PARAM_LEN: usize = 64;

const fn smc_tsi_fid(x: u32) -> u32 {
    SMC_TSI_CALL_BASE.wrapping_add(x)
}

#[repr(u32)]
#[derive(Debug, Clone, Copy)]
pub enum ImageKeyAlg {
    HMAC_SHA256 = 0,
}

/// HUK derive key (matches `SMC_TSI_FID(0x19B)`).
const SMC_TSI_HUK_DERIVE_KEY: u32 = smc_tsi_fid(0x19B);

/// Invoke an RSI SMC call and return raw `x0`..`x2` after `smc`.
///
/// Register layout matches Linux `arm_smccc_1_1_smc` for the common case:
/// `func` → `x0`, four arguments → `x1`..`x4`, `x5`..`x7` = 0.
pub fn rsi_smc_call(
    func: u32,
    arg0: usize,
    arg1: usize,
    arg2: usize,
    arg3: usize,
) -> (usize, usize, usize) {
    let ret0;
    let ret1;
    let ret2;
    unsafe {
        core::arch::asm!(
            "smc #0",
            inlateout("x0") func as usize => ret0,
            inlateout("x1") arg0 => ret1,
            inlateout("x2") arg1 => ret2,
            in("x3") arg2,
            in("x4") arg3,
            in("x5") 0usize,
            in("x6") 0usize,
            in("x7") 0usize,
        )
    }
    (ret0, ret1, ret2)
}

/// Derive key via TSI HUK SMC; returns SMCCC status in `x0` (same as `arm_smccc_res.a0`).
pub fn smc_image_key(alg: u32, user_param: &[u8], key_buf: &mut [u8]) -> TeeResult<usize> {
    if key_buf.len() != IMAGE_KEY_LEN {
        return Err(TEE_ERROR_SHORT_BUFFER);
    }
    let user_pa = v2p(VirtAddr::from(user_param.as_ptr() as usize)).as_usize();
    let key_pa = v2p(VirtAddr::from(key_buf.as_mut_ptr() as usize)).as_usize();
    let (ret0, ..) = rsi_smc_call(
        SMC_TSI_HUK_DERIVE_KEY,
        alg as usize,
        user_pa,
        user_param.len(),
        key_pa,
    );
    Ok(ret0)
}

pub fn get_huk_key(huk_key: &mut [u8]) -> TeeResult {
    let salt = b"virtcca image key";
    let mut user_param = [0u8; USER_PARAM_LEN];
    let mut image_key = [0u8; IMAGE_KEY_LEN];
    user_param[..salt.len()].copy_from_slice(salt);

    let ret = smc_image_key(ImageKeyAlg::HMAC_SHA256 as u32, &user_param, &mut image_key)?;
    if ret != 0 {
        error!("smc_image_key failed with ret: {}", ret);
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    // warn!("get_huk_key: image_key: {:?}", slice_fmt(&image_key));
    let info = b"KunPeng VirtCCA Sealing Key";
    Hkdf::hkdf(MdType::SM3, info, &image_key, &[], huk_key)
        .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
    // warn!("get_huk_key: huk_key: {:?}", slice_fmt(huk_key));
    Ok(())
}

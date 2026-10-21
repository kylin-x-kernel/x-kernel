// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;

use tee_crypto::mac::Mac;
use tee_raw_sys::TEE_ERROR_BAD_PARAMETERS;

use super::{
    otp_stubs::{TeeHwUniqueKey, tee_otp_get_hw_unique_key},
    utee_defines::HW_UNIQUE_KEY_LENGTH,
};
use crate::tee::TeeResult;

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(super) enum HukSubkeyUsage {
    Rpmb     = 0,
    Ssk      = 1,
    DieId    = 2,
    UniqueTa = 3,
    TaEnc    = 4,
}

pub(super) const HUK_SUBKEY_MAX_LEN: usize = super::utee_defines::TEE_SHA256_HASH_SIZE;

pub(super) fn huk_subkey_derive(
    usage: HukSubkeyUsage,
    const_data: Option<&[u8]>,
    subkey: &mut [u8],
) -> TeeResult {
    let mut huk = TeeHwUniqueKey {
        data: [0; HW_UNIQUE_KEY_LENGTH],
    };

    if subkey.len() > HUK_SUBKEY_MAX_LEN {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    tee_otp_get_hw_unique_key(&mut huk)
        .inspect_err(|_| error!("tee_otp_get_hw_unique_key failed"))?;

    // 打印 HUK 值
    tee_debug!("HUK derived: {:?}", huk.data);

    // 构造输入数据: usage_bytes + const_data
    let mut data_to_hash = Vec::new();
    data_to_hash.extend_from_slice(&(usage as u32).to_le_bytes());
    if let Some(data) = const_data {
        data_to_hash.extend_from_slice(data);
    }

    let mut hmac =
        tee_crypto::mac::HmacSm3::new(&huk.data).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
    hmac.update(&data_to_hash);
    let result = hmac.finalize();
    subkey.copy_from_slice(&result[..subkey.len()]);

    Ok(())
}

unittest::path_mod_test!(tests, "../../ut-tests/tee/huk_subkey.rs");

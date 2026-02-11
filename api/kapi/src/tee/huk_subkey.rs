// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;
use core::{ffi::c_uint, mem::size_of, ptr};

use klogger::info;
use mbedtls::hash::{Hmac, Md, Type as MdType};
use tee_raw_sys::{
    TEE_ALG_HMAC_SHA256, TEE_ALG_SM3, TEE_ERROR_BAD_PARAMETERS, TEE_MODE_MAC, TEE_OperationMode,
    TEE_TYPE_HMAC_SHA256, TEE_TYPE_HMAC_SM3,
};

use super::{
    otp_stubs::{TeeHwUniqueKey, tee_otp_get_hw_unique_key},
    utee_defines::{HW_UNIQUE_KEY_LENGTH, TEE_SHA256_HASH_SIZE},
};
use crate::tee::{
    TeeResult,
    tee_obj::tee_obj_get,
    tee_svc_cryp::{
        TeeCryptObj, syscall_cryp_obj_alloc, syscall_obj_generate_key, tee_cryp_obj_secret_wrapper,
    },
    tee_svc_cryp2::{
        syscall_cryp_state_alloc, syscall_hash_final, syscall_hash_init, syscall_hash_update,
    },
};

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HukSubkeyUsage {
    Rpmb     = 0,
    Ssk      = 1,
    DieId    = 2,
    UniqueTa = 3,
    TaEnc    = 4,
}

pub const HUK_SUBKEY_MAX_LEN: usize = TEE_SHA256_HASH_SIZE;

pub fn huk_subkey_derive(
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

    tee_otp_get_hw_unique_key(&mut huk)?;

    // 打印 HUK 值
    tee_debug!("HUK derived: {:?}", huk.data);

    // 构造输入数据: usage_bytes + const_data
    let mut data_to_hash = Vec::new();
    data_to_hash.extend_from_slice(&(usage as u32).to_le_bytes());
    if let Some(data) = const_data {
        data_to_hash.extend_from_slice(data);
    }

    let mut hmac = Hmac::new(MdType::SM3, &huk.data).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
    hmac.update(&data_to_hash)
        .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
    hmac.finish(subkey).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;

    Ok(())
}

#[cfg(feature = "tee_test")]
pub mod tests_huk_subkey {
    use unittest::{
        test_fn, test_framework::TestDescriptor, test_framework_basic::TestResult, tests_name,
    };

    use super::*;

    test_fn! {
        using TestResult;

        fn test_huk_subkey_derive() {
            // derive with const_data NULL in subkey_1
            let mut subkey_1 = [0; HUK_SUBKEY_MAX_LEN];
            huk_subkey_derive(HukSubkeyUsage::Ssk, None, &mut subkey_1).unwrap();
            assert_eq!(subkey_1.len(), HUK_SUBKEY_MAX_LEN);
            // subkey must  not be all zero
            assert!(!subkey_1.iter().all(|x| *x == 0));

            // derive with const_data NULL in subkey_2
            let mut subkey_2 = [0; HUK_SUBKEY_MAX_LEN];
            huk_subkey_derive(HukSubkeyUsage::Ssk, None, &mut subkey_2).unwrap();
            assert_eq!(subkey_2.len(), HUK_SUBKEY_MAX_LEN);
            // subkey must  not be all zero
            assert!(!subkey_2.iter().all(|x| *x == 0));

            // subkey_1 and subkey_2 must be same
            assert_eq!(subkey_1, subkey_2);

            // derive with const_data in subkey_3
            let const_data = b"test_const_data";
            let mut subkey_3 = [0; HUK_SUBKEY_MAX_LEN];
            huk_subkey_derive(HukSubkeyUsage::Ssk, Some(const_data), &mut subkey_3).unwrap();
            assert_eq!(subkey_3.len(), HUK_SUBKEY_MAX_LEN);
            // subkey must  not be all zero
            assert!(!subkey_3.iter().all(|x| *x == 0));

            // subkey_1 and subkey_3 must be different
            assert!(subkey_1 != subkey_3);
        }
    }

    tests_name! {
        TEST_HUK_SUBKEY_DERIVE;
        huk_subkey_derive;
        //------------------------
        test_huk_subkey_derive,
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[cfg(all(target_arch = "x86_64", feature = "x86_csv"))]
use super::tee_get_sealing_key::vmmcall_get_sealing_key;
use super::utee_defines::HW_UNIQUE_KEY_LENGTH;
use crate::tee::TeeResult;

#[repr(C)]
pub struct TeeHwUniqueKey {
    pub data: [u8; HW_UNIQUE_KEY_LENGTH],
}

// TODO: need to be implement
pub fn tee_otp_get_hw_unique_key(hwkey: &mut TeeHwUniqueKey) -> TeeResult {
    hwkey.data.fill(0xAA);
    #[cfg(all(target_arch = "x86_64", feature = "x86_csv"))]
    let _ = unsafe { vmmcall_get_sealing_key(hwkey.data.as_mut_ptr(), HW_UNIQUE_KEY_LENGTH) };
    Ok(())
}

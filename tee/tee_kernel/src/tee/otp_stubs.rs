// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use cfg_if::cfg_if;

use super::utee_defines::HW_UNIQUE_KEY_LENGTH;
use crate::tee::TeeResult;

#[repr(C)]
pub struct TeeHwUniqueKey {
    pub data: [u8; HW_UNIQUE_KEY_LENGTH],
}

// TODO: need to be implement
pub fn tee_otp_get_hw_unique_key(hwkey: &mut TeeHwUniqueKey) -> TeeResult {
    hwkey.data.fill(0xAA);
    info!("tee_otp_get_hw_unique_key");
    cfg_if! {
        if #[cfg(all(target_arch = "x86_64", feature = "x86_csv"))] {
            use crate::tee::arch::x86_64::hygon_csv::get_huk_key;
            info!("get_huk_key from CSV sealing key");
            get_huk_key(&mut hwkey.data)?;
        }
    }
    Ok(())
}

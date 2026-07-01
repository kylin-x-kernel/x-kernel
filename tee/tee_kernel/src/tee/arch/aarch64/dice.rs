// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use dice_driver::read_raw_handover_data;
use tee_crypto::{hkdf, mac::HmacSm3};
use tee_raw_sys::{TEE_ERROR_BAD_PARAMETERS, TEE_ERROR_BAD_STATE};
use xdice::dice_parse_handover;

use crate::tee::TeeResult;

pub fn get_huk_key(huk_key: &mut [u8]) -> TeeResult {
    if huk_key.is_empty() {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    let handover_data = read_raw_handover_data().map_err(|_| TEE_ERROR_BAD_STATE)?;
    let (_cdi_attest, cdi_seal, _chain) =
        dice_parse_handover(&handover_data).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;

    // warn!("get_huk_key: cdi_seal: {:?}", slice_fmt(&cdi_seal));

    if cdi_seal.len() < 16 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    if cdi_seal.iter().all(|&b| b == 0) {
        warn!("cdi_seal is all zeros");
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    debug!("cdi_seal length: {}", cdi_seal.len());

    let salt = b"DICE CDI Seal";
    let okm = hkdf::hkdf::<HmacSm3>(salt, cdi_seal, &[], huk_key.len())
        .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
    huk_key.copy_from_slice(&okm);
    // warn!("get_huk_key: huk_key: {:?}", slice_fmt(huk_key));
    Ok(())
}

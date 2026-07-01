// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::{
    android::dice_android_handover_main_flow,
    cbor_cert_op::{
        CborOut, DICE_CDI_SIZE, DICE_HASH_SIZE, DICE_INLINE_CONFIG_SIZE, DiceInputValues,
        DiceResult,
    },
    mbedtls_sm2dsa::dice_kdf,
};

pub const K_CDI_ATTEST_LABEL: i64 = 1;
pub const K_CDI_SEAL_LABEL: i64 = 2;
pub const K_CHAIN_LABEL: i64 = 3;

pub const CODE_HASH: [u8; DICE_HASH_SIZE] = [
    0x59, 0x0d, 0x30, 0x26, 0xdb, 0x37, 0xb7, 0x77, 0x98, 0x31, 0xf5, 0xb7, 0x4f, 0xa4, 0x9a, 0xe4,
    0x5d, 0x09, 0xc4, 0x6a, 0x50, 0x71, 0x5a, 0xb0, 0x5e, 0x3d, 0xe2, 0xb6, 0x09, 0xf1, 0x82, 0x79,
];

pub const CONFIG_VALUE: [u8; DICE_INLINE_CONFIG_SIZE] = [
    0x83, 0x7b, 0x07, 0x82, 0xb8, 0x25, 0x61, 0xf3, 0x0a, 0xb3, 0x6f, 0x95, 0x82, 0x93, 0xd5, 0x1d,
    0x44, 0xaf, 0x04, 0x26, 0x94, 0x77, 0x6d, 0x0f, 0x81, 0xee, 0xd7, 0x7d, 0xc3, 0xf6, 0x6a, 0x93,
];
// 0xd4,
// 0x8f, 0x19, 0x7a, 0xad, 0x70, 0xbd, 0x41, 0xfc, 0x20, 0x20, 0x0e,
// 0x29, 0x3e, 0xa9, 0x4d, 0x05, 0x56, 0x96, 0xf3, 0x8c, 0x51, 0x69,
// 0x5b, 0xb0, 0xb6, 0xd3, 0xf2, 0xfe, 0x53, 0x96, 0xd0

pub fn dice_tee_handover_main_flow(buffer: &mut [u8]) -> Result<usize, DiceResult> {
    let mut out = CborOut::new(buffer);
    out.write_map(2);
    out.write_int(K_CDI_ATTEST_LABEL);
    let next_cdi_attest = match out.alloc_bstr(DICE_CDI_SIZE) {
        Some(bstr) => bstr,
        None => return Err(DiceResult::PlatformError(-1)),
    };

    const DATA_CDI: &[u8] = b"CDI_Attest";
    dice_kdf(DATA_CDI, Some(DATA_CDI), Some(DATA_CDI), next_cdi_attest)
        .map_err(|_| DiceResult::PlatformError(-1))?;

    out.write_int(K_CDI_SEAL_LABEL);
    let next_cdi_seal = match out.alloc_bstr(DICE_CDI_SIZE) {
        Some(bstr) => bstr,
        None => return Err(DiceResult::PlatformError(-1)),
    };

    const DATA_CDI_SEAL: &[u8] = b"CDI_Seal";
    dice_kdf(
        DATA_CDI_SEAL,
        Some(DATA_CDI_SEAL),
        Some(DATA_CDI_SEAL),
        next_cdi_seal,
    )
    .map_err(|_| DiceResult::PlatformError(-1))?;

    Ok(out.size())
}

pub fn dice_tee_handover_main_flow_chain_origin(buffer: &mut [u8]) -> Result<usize, DiceResult> {
    let mut handover = [0u8; 128];
    let handover_size: usize = handover.len();

    let mut input_values = DiceInputValues::new_zero();

    input_values.code_hash.copy_from_slice(&CODE_HASH);
    input_values.config_value.copy_from_slice(&CONFIG_VALUE);

    let _handover_actual_size = dice_tee_handover_main_flow(&mut handover)?;

    let actual_handover = &handover[..handover_size];

    dice_android_handover_main_flow(&mut 0, actual_handover, &input_values, buffer)
}

/// Corresponds to `DiceTeeHandoverMainFlowChain` in C.
/// * `handover` – slice containing the upstream handover data.
/// * `buffer` – output buffer where the Android flow will write its result.
pub fn dice_tee_handover_main_flow_chain(
    handover: &[u8],
    buffer: &mut [u8],
) -> Result<usize, DiceResult> {
    let mut input_values = DiceInputValues::new_zero();
    input_values.code_hash.copy_from_slice(&CODE_HASH);
    input_values.config_value.copy_from_slice(&CONFIG_VALUE);
    // authority_hash is already zero from new_zero()

    dice_android_handover_main_flow(&mut 0, handover, &input_values, buffer)
}

/// Corresponds to `DiceTeeHandoverMainFlowChainCodeHash` in C.
/// This variant allows the caller to specify a custom code hash.
pub fn dice_tee_handover_main_flow_chain_code_hash(
    handover: &[u8],
    code_hash: &[u8],
    buffer: &mut [u8],
) -> Result<usize, DiceResult> {
    let mut input_values = DiceInputValues::new_zero();
    // copy at most the size of the destination
    let len = core::cmp::min(code_hash.len(), input_values.code_hash.len());
    input_values.code_hash[..len].copy_from_slice(&code_hash[..len]);
    input_values.config_value.copy_from_slice(&CONFIG_VALUE);
    // authority_hash remains zero

    dice_android_handover_main_flow(&mut 0, handover, &input_values, buffer)
}

pub fn dice_init() {
    // correspond to C's DiceTeeHandoverMainFlowChainOrigin(256, NULL, NULL)
    // allocate a dummy buffer and ignore the result
    let mut buf = [0u8; 256];
    let _ = dice_tee_handover_main_flow_chain_origin(&mut buf);
}

#[cfg(unittest)]
mod tests {
    use super::*;
    use crate::test_data::test_constants::HANDOVER;

    const CODEHASH: &[u8] = &[
        0x14, 0xde, 0xb7, 0x0d, 0x3e, 0xe1, 0x9d, 0x5a, 0x8b, 0x54, 0xac, 0x1a, 0xe4, 0xa0, 0x9b,
        0x51, 0x25, 0x42, 0x26, 0x36, 0x34, 0x14, 0xa3, 0xc3, 0x6a, 0x0e, 0x50, 0x19, 0x08, 0x99,
        0x09, 0xdc,
    ];

    #[unittest::def_test]
    fn test_dice_tee_handover_main_flow_chain_code_hash() {
        let mut buffer = [0x0u8; 4096];

        let mut input_check = DiceInputValues::new_zero();
        input_check.code_hash[..32].copy_from_slice(&CODEHASH[..32]);
        input_check.config_value.copy_from_slice(&CONFIG_VALUE);

        // panic!("code_hash: {:02x?} , config_value: {:02x?}",input_check.code_hash, input_check.config_value);

        match dice_tee_handover_main_flow_chain_code_hash(HANDOVER, CODEHASH, &mut buffer) {
            Ok(_size) => {}
            Err(e) => {
                panic!("function failed for valid input: {:?}", e);
            }
        }
    }
}

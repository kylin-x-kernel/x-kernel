// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use xdice::{DiceInputValues, tee_dice::*};

use crate::test_data::test_constants::HANDOVER;

const CODEHASH: &[u8] = &[
    0x14, 0xde, 0xb7, 0x0d, 0x3e, 0xe1, 0x9d, 0x5a, 0x8b, 0x54, 0xac, 0x1a, 0xe4, 0xa0, 0x9b, 0x51,
    0x25, 0x42, 0x26, 0x36, 0x34, 0x14, 0xa3, 0xc3, 0x6a, 0x0e, 0x50, 0x19, 0x08, 0x99, 0x09, 0xdc,
];

#[test]
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

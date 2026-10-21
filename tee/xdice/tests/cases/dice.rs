// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use xdice::{
    cbor_cert_op::{
        DICE_CDI_SIZE, DICE_HASH_SIZE, DICE_HIDDEN_SIZE, DICE_INLINE_CONFIG_SIZE, DiceInputValues,
    },
    dice::*,
};

use crate::cbor_cert_op::{DiceConfigType, DiceMode, DiceResult};

#[test]
fn dice_main_flow_descriptor_does_not_crash() {
    let mut context = 0u8;
    let current_cdi_attest = [0u8; DICE_CDI_SIZE];
    let current_cdi_seal = [0u8; DICE_CDI_SIZE];
    let mut next_cdi_attest = [0u8; DICE_CDI_SIZE];
    let mut next_cdi_seal = [0u8; DICE_CDI_SIZE];

    let input_values = DiceInputValues {
        code_hash: [0xAA; DICE_HASH_SIZE],
        code_descriptor: &[],
        config_type: DiceConfigType::Descriptor,
        config_value: [0u8; DICE_INLINE_CONFIG_SIZE],
        config_descriptor: b"test-config",
        authority_hash: [0xBB; DICE_HASH_SIZE],
        authority_descriptor: &[],
        mode: DiceMode::Normal,
        hidden: [0xCC; DICE_HIDDEN_SIZE],
    };

    let result = dice_main_flow(
        &mut context,
        &current_cdi_attest,
        &current_cdi_seal,
        &input_values,
        None,
        &mut next_cdi_attest,
        &mut next_cdi_seal,
    );

    assert!(result.is_ok());
    assert_ne!(next_cdi_attest, [0u8; DICE_CDI_SIZE]);
    assert_ne!(next_cdi_seal, [0u8; DICE_CDI_SIZE]);
}

#[test]
fn test_dice_main_flow_empty_buffer_size_calculation() {
    let mut context = 0u8;
    let current_cdi_attest = [0u8; DICE_CDI_SIZE];
    let current_cdi_seal = [0u8; DICE_CDI_SIZE];
    let mut next_cdi_attest = [0u8; DICE_CDI_SIZE];
    let mut next_cdi_seal = [0u8; DICE_CDI_SIZE];

    let input_values = DiceInputValues {
        code_hash: [0xAA; DICE_HASH_SIZE],
        code_descriptor: &[],
        config_type: DiceConfigType::Inline,
        config_value: [0u8; DICE_INLINE_CONFIG_SIZE],
        config_descriptor: &[],
        authority_hash: [0xBB; DICE_HASH_SIZE],
        authority_descriptor: &[],
        mode: DiceMode::Normal,
        hidden: [0xCC; DICE_HIDDEN_SIZE],
    };

    let empty_buffer: &mut [u8] = &mut [];

    match dice_main_flow(
        &mut context,
        &current_cdi_attest,
        &current_cdi_seal,
        &input_values,
        Some(empty_buffer),
        &mut next_cdi_attest,
        &mut next_cdi_seal,
    ) {
        Err(DiceResult::BufferTooSmall(required_size)) => {
            assert!(required_size > 0, "should return the required size");
        }
        Ok(_) => {
            panic!("should return BufferTooSmall");
        }
        Err(e) => {
            panic!("unexpected error: {:?}", e);
        }
    }
}

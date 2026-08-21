// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::{
    cbor_cert_op::{
        DICE_CDI_SIZE, DICE_HASH_SIZE, DICE_HIDDEN_SIZE, DICE_ID_SIZE, DICE_INLINE_CONFIG_SIZE,
        DiceConfigType, DiceInputValues, DiceResult, dice_clear_memory, dice_generate_certificate,
    },
    mbedtls_sm2dsa::{dice_hash, dice_kdf},
};

pub const DICE_CODE_SIZE: usize = DICE_HASH_SIZE;
pub const DICE_CONFIG_SIZE: usize = DICE_INLINE_CONFIG_SIZE;
pub const DICE_AUTHORITY_SIZE: usize = DICE_HASH_SIZE;
pub const DICE_MODE_SIZE: usize = 1;
pub const DICE_PRIVATE_KEY_SEED_SIZE: usize = 32;

pub const K_ASYM_SALT: [u8; 64] = [
    0x63, 0xB6, 0xA0, 0x4D, 0x2C, 0x07, 0x7F, 0xC1, 0x0F, 0x63, 0x9F, 0x21, 0xDA, 0x79, 0x38, 0x44,
    0x35, 0x6C, 0xC2, 0xB0, 0xB4, 0x41, 0xB3, 0xA7, 0x71, 0x24, 0x03, 0x5C, 0x03, 0xF8, 0xE1, 0xBE,
    0x60, 0x35, 0xD3, 0x1F, 0x28, 0x28, 0x21, 0xA7, 0x45, 0x0A, 0x02, 0x22, 0x2A, 0xB1, 0xB3, 0xCF,
    0xF1, 0x67, 0x9B, 0x05, 0xAB, 0x1C, 0xA5, 0xD1, 0xAF, 0xFB, 0x78, 0x9C, 0xCD, 0x2B, 0x0B, 0x3B,
];

pub const K_ID_SALT: [u8; 64] = [
    0xDB, 0xDB, 0xAE, 0xBC, 0x80, 0x20, 0xDA, 0x9F, 0xF0, 0xDD, 0x5A, 0x24, 0xC8, 0x3A, 0xA5, 0xA5,
    0x42, 0x86, 0xDF, 0xC2, 0x63, 0x03, 0x1E, 0x32, 0x9B, 0x4D, 0xA1, 0x48, 0x43, 0x06, 0x59, 0xFE,
    0x62, 0xCD, 0xB5, 0xB7, 0xE1, 0xE0, 0x0F, 0xC6, 0x80, 0x30, 0x67, 0x11, 0xEB, 0x44, 0x4A, 0xF7,
    0x72, 0x09, 0x35, 0x94, 0x96, 0xFC, 0xFF, 0x1D, 0xB9, 0x52, 0x0B, 0xA5, 0x1C, 0x7B, 0x29, 0xEA,
];

pub fn dice_derive_cdi_private_key_seed(
    _context: &mut u8,
    cdi_attest: &[u8; DICE_CDI_SIZE],
    cdi_private_key_seed: &mut [u8; DICE_PRIVATE_KEY_SEED_SIZE],
) -> Result<(), DiceResult> {
    const ID_INFO: &[u8] = b"Key Pair";

    dice_kdf(
        cdi_attest,
        Some(&K_ASYM_SALT),
        Some(ID_INFO),
        cdi_private_key_seed,
    )
    .map_err(|_| DiceResult::PlatformError(301))?;

    Ok(())
}

pub fn dice_derive_cdi_certificate_id(
    _context: &mut u8,
    cdi_public_key: &[u8],
    id: &mut [u8; DICE_ID_SIZE],
) -> Result<(), DiceResult> {
    const ID_INFO: &[u8] = b"ID";

    dice_kdf(cdi_public_key, Some(&K_ID_SALT), Some(ID_INFO), id)
        .map_err(|_| DiceResult::PlatformError(-1))?;

    id[0] &= 0x7F;

    Ok(())
}
pub fn dice_main_flow(
    context: &mut u8,
    current_cdi_attest: &[u8; DICE_CDI_SIZE],
    current_cdi_seal: &[u8; DICE_CDI_SIZE],
    input_values: &DiceInputValues,
    next_cdi_certificate: Option<&mut [u8]>,
    next_cdi_attest: &mut [u8; DICE_CDI_SIZE],
    next_cdi_seal: &mut [u8; DICE_CDI_SIZE],
) -> Result<usize, DiceResult> {
    // | Code Input | Config Input | Authority Input | Mode Input | Hidden Input |
    const K_CODE_OFFSET: usize = 0;
    const K_CONFIG_OFFSET: usize = K_CODE_OFFSET + DICE_CODE_SIZE;
    const K_AUTHORITY_OFFSET: usize = K_CONFIG_OFFSET + DICE_CONFIG_SIZE;
    const K_MODE_OFFSET: usize = K_AUTHORITY_OFFSET + DICE_AUTHORITY_SIZE;
    const K_HIDDEN_OFFSET: usize = K_MODE_OFFSET + DICE_MODE_SIZE;
    const INPUT_BUFFER_SIZE: usize =
        DICE_CODE_SIZE + DICE_CONFIG_SIZE + DICE_AUTHORITY_SIZE + DICE_MODE_SIZE + DICE_HIDDEN_SIZE;

    let mut input_buffer = [0u8; INPUT_BUFFER_SIZE];

    input_buffer[K_CODE_OFFSET..K_CODE_OFFSET + DICE_CODE_SIZE]
        .copy_from_slice(&input_values.code_hash);

    if input_values.config_type == DiceConfigType::Inline {
        input_buffer[K_CONFIG_OFFSET..K_CONFIG_OFFSET + DICE_CONFIG_SIZE]
            .copy_from_slice(&input_values.config_value);
    } else if input_values.config_descriptor.is_empty() {
        return Err(DiceResult::InvalidInput);
    } else {
        let mut config_hash = [0u8; DICE_HASH_SIZE];
        dice_hash(context, input_values.config_descriptor, &mut config_hash)
            .map_err(|_| DiceResult::PlatformError(-1))?;

        input_buffer[K_CONFIG_OFFSET..K_CONFIG_OFFSET + DICE_HASH_SIZE]
            .copy_from_slice(&config_hash);
        input_buffer[K_CONFIG_OFFSET + DICE_HASH_SIZE..K_CONFIG_OFFSET + DICE_CONFIG_SIZE].fill(0);
    }

    input_buffer[K_AUTHORITY_OFFSET..K_AUTHORITY_OFFSET + DICE_AUTHORITY_SIZE]
        .copy_from_slice(&input_values.authority_hash);

    input_buffer[K_MODE_OFFSET] = input_values.mode as u8;

    input_buffer[K_HIDDEN_OFFSET..K_HIDDEN_OFFSET + DICE_HIDDEN_SIZE]
        .copy_from_slice(&input_values.hidden);

    let mut attest_input_hash = [0u8; DICE_HASH_SIZE];
    dice_hash(context, &input_buffer, &mut attest_input_hash)
        .map_err(|_| DiceResult::PlatformError(-1))?;

    let mut seal_input_hash = [0u8; DICE_HASH_SIZE];
    dice_hash(
        context,
        &input_buffer[K_AUTHORITY_OFFSET
            ..K_AUTHORITY_OFFSET + DICE_AUTHORITY_SIZE + DICE_MODE_SIZE + DICE_HIDDEN_SIZE],
        &mut seal_input_hash,
    )
    .map_err(|_| DiceResult::PlatformError(-1))?;

    dice_kdf(
        current_cdi_attest,
        Some(&attest_input_hash),
        Some(b"CDI_Attest"),
        next_cdi_attest,
    )
    .map_err(|_| DiceResult::PlatformError(-1))?;

    dice_kdf(
        current_cdi_seal,
        Some(&seal_input_hash),
        Some(b"CDI_Seal"),
        next_cdi_seal,
    )
    .map_err(|_| DiceResult::PlatformError(-1))?;

    let certificate_size = if let Some(cert_buffer) = next_cdi_certificate {
        let mut current_cdi_private_key_seed = [0u8; DICE_PRIVATE_KEY_SEED_SIZE];
        dice_derive_cdi_private_key_seed(
            context,
            current_cdi_attest,
            &mut current_cdi_private_key_seed,
        )?;

        let mut next_cdi_private_key_seed = [0u8; DICE_PRIVATE_KEY_SEED_SIZE];
        dice_derive_cdi_private_key_seed(context, next_cdi_attest, &mut next_cdi_private_key_seed)?;

        let cert_result = dice_generate_certificate(
            context,
            &next_cdi_private_key_seed,
            &current_cdi_private_key_seed,
            input_values,
            cert_buffer,
        );

        dice_clear_memory(context, &mut current_cdi_private_key_seed);
        dice_clear_memory(context, &mut next_cdi_private_key_seed);

        match cert_result {
            Ok(size) => size,
            Err(DiceResult::BufferTooSmall(required_size)) if required_size > 0 => {
                return Err(DiceResult::BufferTooSmall(required_size));
            }
            Err(e) => return Err(e),
        }
    } else {
        0
    };

    dice_clear_memory(context, &mut input_buffer);
    dice_clear_memory(context, &mut attest_input_hash);
    dice_clear_memory(context, &mut seal_input_hash);

    Ok(certificate_size)
}

#[cfg(test)]
mod tests {
    use super::*;
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
}

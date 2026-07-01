// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod android;
pub mod cbor_cert_op;
pub mod cbor_reader;
pub mod dice;
pub mod mbedtls_sm2dsa;
pub mod tee_dice;
pub mod test_data;

pub use android::dice_android_handover_parse;
pub use cbor_cert_op::{DiceConfigType, DiceInputValues, DiceMode, DiceResult};
pub use mbedtls_sm2dsa::{sm2_keypair_from_seed, sm2_sign, sm2_verify};
pub use tee_dice::{
    dice_init, dice_tee_handover_main_flow_chain, dice_tee_handover_main_flow_chain_code_hash,
};

/// Parse the Android handover buffer and extract CDI values and optional chain.
/// * `buffer` – the handover buffer to parse.
///   Returns a tuple of (cdi_attest, cdi_seal, chain) slices, or a DiceResult error.
#[allow(clippy::type_complexity)]
pub fn dice_parse_handover(buffer: &[u8]) -> Result<(&[u8], &[u8], &[u8]), DiceResult> {
    let (cdi_attest, cdi_seal, chain) = dice_android_handover_parse(buffer)?;
    let chain_slice = chain.unwrap_or(&[]);
    Ok((cdi_attest.as_slice(), cdi_seal.as_slice(), chain_slice))
}

pub fn dice_main_flow_chain<'a>(
    handover: &[u8],
    buffer: &'a mut [u8],
) -> Result<&'a [u8], DiceResult> {
    let actual_size = dice_tee_handover_main_flow_chain(handover, buffer)?;
    Ok(&buffer[..actual_size])
}

pub fn dice_main_flow_chain_codehash<'a>(
    handover: &[u8],
    codehash: &[u8],
    buffer: &'a mut [u8],
) -> Result<&'a [u8], DiceResult> {
    let actual_size = dice_tee_handover_main_flow_chain_code_hash(handover, codehash, buffer)?;
    Ok(&buffer[..actual_size])
}

#[cfg(unittest)]
#[unittest::def_test]
fn test_dice_generate_certificate_platform_error() {
    use crate::cbor_cert_op::{
        DiceConfigType, DiceInputValues, DiceMode, dice_generate_certificate,
    };

    let subject_seed = [0u8; 32];
    let authority_seed = [0u8; 32];
    let input_values = DiceInputValues {
        code_hash: [0u8; 32],
        code_descriptor: &[],
        config_type: DiceConfigType::Inline,
        config_value: [0u8; 32],
        config_descriptor: &[],
        authority_hash: [0u8; 32],
        authority_descriptor: &[],
        mode: DiceMode::Normal,
        hidden: [0u8; 64],
    };
    let mut certificate = [0u8; 4096];

    match dice_generate_certificate(
        &mut 0,
        &subject_seed,
        &authority_seed,
        &input_values,
        &mut certificate,
    ) {
        Ok(size) => {
            assert!(size > 0, "Certificate size should be greater than 0");
        }
        Err(e) => {
            assert!(
                !matches!(e, crate::cbor_cert_op::DiceResult::PlatformError(-1)),
                "Should not return PlatformError(-1)"
            );
        }
    }
}

#[cfg(unittest)]
mod lib_tests {
    use super::*;
    use crate::test_data::test_constants::HANDOVER;

    #[unittest::def_test]
    fn test_dice_main_flow_chain_codehash() {
        let codehash = &[
            0x14, 0xde, 0xb7, 0x0d, 0x3e, 0xe1, 0x9d, 0x5a, 0x8b, 0x54, 0xac, 0x1a, 0xe4, 0xa0,
            0x9b, 0x51, 0x25, 0x42, 0x26, 0x36, 0x34, 0x14, 0xa3, 0xc3, 0x6a, 0x0e, 0x50, 0x19,
            0x08, 0x99, 0x09, 0xdc,
        ];
        let mut buffer = [0x0u8; 4096];

        match dice_main_flow_chain_codehash(HANDOVER, codehash, &mut buffer) {
            Ok(out_buffer) => {
                assert!(!out_buffer.is_empty(), "Output buffer should not be empty");
                // panic!("out_buffer: {:02x?}, size: {}", out_buffer, out_buffer.len());
            }
            Err(e) => {
                panic!("dice_main_flow_chain_codehash failed: {:?}", e);
            }
        }
    }

    #[unittest::def_test]
    fn test_dice_main_flow_chain() {
        let mut buffer = [0x0u8; 4096];

        match dice_main_flow_chain(HANDOVER, &mut buffer) {
            Ok(out_buffer) => {
                assert!(!out_buffer.is_empty(), "Output buffer should not be empty");
                assert!(
                    !out_buffer.is_empty(),
                    "Output buffer length should be greater than 0"
                );
            }
            Err(e) => {
                panic!("dice_main_flow_chain failed: {:?}", e);
            }
        }
    }

    #[unittest::def_test]
    fn test_dice_parse_handover() {
        let buffer = crate::test_data::test_constants::HANDOVER;

        match dice_parse_handover(buffer) {
            Ok((cdi_attest, cdi_seal, _chain)) => {
                assert!(!cdi_attest.is_empty(), "CDI attest should not be empty");
                assert!(!cdi_seal.is_empty(), "CDI seal should not be empty");
                assert_eq!(cdi_attest.len(), 32, "CDI attest should be 32 bytes");
                assert_eq!(cdi_seal.len(), 32, "CDI seal should be 32 bytes");
                // if let Some(chain_data) = Some(chain) {
                //     assert!(!chain_data.is_empty(), "Chain should not be empty");
                // }
            }
            Err(e) => {
                panic!("dice_parse_handover failed: {:?}", e);
            }
        }
    }
}

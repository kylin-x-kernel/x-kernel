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

pub use android::dice_android_handover_parse;
pub use cbor_cert_op::{DiceConfigType, DiceInputValues, DiceMode, DiceResult};
pub use mbedtls_sm2dsa::{sm2_keypair_from_seed, sm2_sign, sm2_verify};
pub use tee_dice::{
    dice_init, dice_tee_handover_main_flow_chain, dice_tee_handover_main_flow_chain_code_hash,
};

/// CDI_Attest, CDI_Seal and DICE chain slices borrowed from a handover buffer.
pub type HandoverSlices<'a> = (&'a [u8], &'a [u8], &'a [u8]);

/// Parse the Android handover buffer and extract CDI values and optional chain.
/// * `buffer` – the handover buffer to parse.
///   Returns a tuple of (cdi_attest, cdi_seal, chain) slices, or a DiceResult error.
pub fn dice_parse_handover(buffer: &[u8]) -> Result<HandoverSlices<'_>, DiceResult> {
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

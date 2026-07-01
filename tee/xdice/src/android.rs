// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::{
    cbor_cert_op::{
        CborOut, DICE_CDI_SIZE, DiceInputValues, DiceResult, Principal, dice_clear_memory,
        dice_cose_encode_public_key,
    },
    cbor_reader::CborIn,
    dice::{DICE_PRIVATE_KEY_SEED_SIZE, dice_derive_cdi_private_key_seed, dice_main_flow},
    mbedtls_sm2dsa::dice_keypair_from_seed,
};

pub const DICE_PUBLIC_KEY_BUFFER_SIZE: usize = 64;
pub const DICE_PRIVATE_KEY_BUFFER_SIZE: usize = 32;
pub const DICE_SIGNATURE_BUFFER_SIZE: usize = 64;

pub struct DiceAndroidConfigValues<'a> {
    pub configs: u32,
    pub component_name: &'a str,
    pub component_version: u64,
    pub security_version: u64,
}

pub const DICE_ANDROID_CONFIG_COMPONENT_NAME: u32 = 1 << 0;
pub const DICE_ANDROID_CONFIG_COMPONENT_VERSION: u32 = 1 << 1;
pub const DICE_ANDROID_CONFIG_RESETTABLE: u32 = 1 << 2;
pub const DICE_ANDROID_CONFIG_SECURITY_VERSION: u32 = 1 << 3;
pub const DICE_ANDROID_CONFIG_RKP_VM_MARKER: u32 = 1 << 4;

fn population_count(n: u32) -> usize {
    n.count_ones() as usize
}

pub fn dice_android_format_config_descriptor(
    config_values: &DiceAndroidConfigValues,
    buffer: &mut [u8],
) -> Result<usize, DiceResult> {
    const K_COMPONENT_NAME_LABEL: i64 = -70002;
    const K_COMPONENT_VERSION_LABEL: i64 = -70003;
    const K_RESETTABLE_LABEL: i64 = -70004;
    const K_SECURITY_VERSION_LABEL: i64 = -70005;
    const K_RKP_VM_MARKER_LABEL: i64 = -70006;

    let mut out = CborOut::new(buffer);
    out.write_map(population_count(config_values.configs));
    if config_values.configs & DICE_ANDROID_CONFIG_COMPONENT_NAME != 0
        && !config_values.component_name.is_empty()
    {
        out.write_int(K_COMPONENT_NAME_LABEL);
        out.write_tstr(config_values.component_name);
    }

    if config_values.configs & DICE_ANDROID_CONFIG_COMPONENT_VERSION != 0 {
        out.write_int(K_COMPONENT_VERSION_LABEL);
        out.write_uint(config_values.component_version);
    }

    if config_values.configs & DICE_ANDROID_CONFIG_RESETTABLE != 0 {
        out.write_int(K_RESETTABLE_LABEL);
        out.write_null();
    }

    if config_values.configs & DICE_ANDROID_CONFIG_SECURITY_VERSION != 0 {
        out.write_int(K_SECURITY_VERSION_LABEL);
        out.write_uint(config_values.security_version);
    }

    if config_values.configs & DICE_ANDROID_CONFIG_RKP_VM_MARKER != 0 {
        out.write_int(K_RKP_VM_MARKER_LABEL);
        out.write_null();
    }

    if out.is_overflowed() {
        Err(DiceResult::BufferTooSmall(-1))
    } else {
        Ok(out.size())
    }
}

#[allow(clippy::too_many_arguments)]
pub fn dice_android_main_flow(
    context: &mut u8,
    current_cdi_attest: &[u8; DICE_CDI_SIZE],
    current_cdi_seal: &[u8; DICE_CDI_SIZE],
    chain: &[u8],
    input_values: &DiceInputValues,
    mut buffer: &mut [u8],
    next_cdi_attest: &mut [u8; DICE_CDI_SIZE],
    next_cdi_seal: &mut [u8; DICE_CDI_SIZE],
) -> Result<usize, DiceResult> {
    let mut input = CborIn::new(chain);
    let chain_item_count = input.read_array().map_err(|_| DiceResult::InvalidInput)?;

    if chain_item_count < 2 || chain_item_count == usize::MAX {
        return Err(DiceResult::InvalidInput);
    }

    let chain_items_offset = input.offset();
    for _ in 0..chain_item_count {
        input.read_skip().map_err(|_| DiceResult::InvalidInput)?;
    }

    let chain_items_size = input.offset() - chain_items_offset;
    let mut out = CborOut::new(buffer);
    out.write_array(chain_item_count + 1);
    let new_chain_prefix_size = out.size();
    if out.is_overflowed() || chain_items_size > buffer.len().saturating_sub(new_chain_prefix_size)
    {
        // Continue with an empty buffer to measure the required size.
        buffer = &mut [];
    } else {
        // Copy the chain items to the buffer.
        let src_slice = &chain[chain_items_offset..chain_items_offset + chain_items_size];
        let dest_slice =
            &mut buffer[new_chain_prefix_size..new_chain_prefix_size + chain_items_size];
        dest_slice.copy_from_slice(src_slice);
        buffer = &mut buffer[new_chain_prefix_size + chain_items_size..];
    }

    let certificate_size = match dice_main_flow(
        context,
        current_cdi_attest,
        current_cdi_seal,
        input_values,
        Some(buffer),
        next_cdi_attest,
        next_cdi_seal,
    ) {
        Ok(size) => size,
        Err(DiceResult::BufferTooSmall(cert_required_size)) => {
            let total_required =
                new_chain_prefix_size + chain_items_size + cert_required_size as usize;
            return Err(DiceResult::BufferTooSmall(total_required as i32));
        }
        Err(e) => return Err(e),
    };

    let actual_size = new_chain_prefix_size + chain_items_size + certificate_size;

    Ok(actual_size)
}

fn dice_android_main_flow_with_new_dice_chain(
    context: &mut u8,
    current_cdi_attest: &[u8; DICE_CDI_SIZE],
    current_cdi_seal: &[u8; DICE_CDI_SIZE],
    input_values: &DiceInputValues,
    mut buffer: &mut [u8],
    next_cdi_attest: &mut [u8; DICE_CDI_SIZE],
    next_cdi_seal: &mut [u8; DICE_CDI_SIZE],
) -> Result<usize, DiceResult> {
    let mut current_cdi_private_key_seed = [0u8; DICE_PRIVATE_KEY_SEED_SIZE];
    let mut attestation_public_key = [0u8; DICE_PUBLIC_KEY_BUFFER_SIZE];
    let mut attestation_private_key = [0u8; DICE_PRIVATE_KEY_BUFFER_SIZE];

    if let Err(e) = dice_derive_cdi_private_key_seed(
        context,
        current_cdi_attest,
        &mut current_cdi_private_key_seed,
    ) {
        dice_clear_memory(context, &mut current_cdi_private_key_seed);
        return Err(e);
    }

    match dice_keypair_from_seed(&current_cdi_private_key_seed) {
        Ok((pub_key, priv_key)) => {
            attestation_public_key[..pub_key.len()].copy_from_slice(&pub_key);
            attestation_private_key[..priv_key.len()].copy_from_slice(&priv_key);
        }
        Err(e) => {
            dice_clear_memory(context, &mut current_cdi_private_key_seed);
            dice_clear_memory(context, &mut attestation_private_key);
            return Err(e);
        }
    }

    let mut out = CborOut::new(buffer);
    out.write_array(2);
    let encoded_size_used = out.size();
    if out.is_overflowed() {
        buffer = &mut [];
    } else if buffer.len() > encoded_size_used {
        buffer = &mut buffer[encoded_size_used..];
    } else {
        buffer = &mut [];
    }

    let mut encoded_pub_key_size: usize = 0;
    match dice_cose_encode_public_key(
        context,
        Principal::Authority,
        &attestation_public_key,
        buffer,
    ) {
        Ok(size) => {
            encoded_pub_key_size = size;
            if size > 0 && buffer.len() > size {
                buffer = &mut buffer[size..];
            } else {
                buffer = &mut [];
            }
        }

        Err(DiceResult::BufferTooSmall(-1)) => {
            buffer = &mut [];
        }

        Err(e) => {
            dice_clear_memory(context, &mut current_cdi_private_key_seed);
            dice_clear_memory(context, &mut attestation_private_key);
            return Err(e);
        }
    };

    let chain_size = match dice_main_flow(
        context,
        current_cdi_attest,
        current_cdi_seal,
        input_values,
        if buffer.is_empty() {
            None
        } else {
            Some(buffer)
        },
        next_cdi_attest,
        next_cdi_seal,
    ) {
        Ok(size) => size,
        Err(DiceResult::BufferTooSmall(cert_required_size)) => {
            let total_required =
                encoded_size_used + encoded_pub_key_size + cert_required_size as usize;
            dice_clear_memory(context, &mut current_cdi_private_key_seed);
            dice_clear_memory(context, &mut attestation_private_key);
            return Err(DiceResult::BufferTooSmall(total_required as i32));
        }
        Err(e) => {
            dice_clear_memory(context, &mut current_cdi_private_key_seed);
            dice_clear_memory(context, &mut attestation_private_key);
            return Err(e);
        }
    };

    dice_clear_memory(context, &mut current_cdi_private_key_seed);
    dice_clear_memory(context, &mut attestation_private_key);

    Ok(chain_size + encoded_size_used + encoded_pub_key_size)
}

const K_CDI_ATTEST_LABEL: i64 = 1;
const K_CDI_SEAL_LABEL: i64 = 2;
const K_DICE_CHAIN_LABEL: i64 = 3;

pub fn dice_android_handover_main_flow(
    context: &mut u8,
    handover: &[u8],
    input_values: &DiceInputValues,
    buffer: &mut [u8],
) -> Result<usize, DiceResult> {
    let (current_cdi_attest, current_cdi_seal, chain) = dice_android_handover_parse(handover)?;

    let mut out = CborOut::new(buffer);
    out.write_map(3);
    out.write_int(K_CDI_ATTEST_LABEL);
    let next_cdi_attest_written = out.alloc_bstr(DICE_CDI_SIZE).is_some();
    let next_cdi_attest_offset = out.size() - DICE_CDI_SIZE;
    out.write_int(K_CDI_SEAL_LABEL);
    let next_cdi_seal_written = out.alloc_bstr(DICE_CDI_SIZE).is_some();
    let next_cdi_seal_offset = out.size() - DICE_CDI_SIZE;
    out.write_int(K_DICE_CHAIN_LABEL);

    let mut ignored_cdi_attest = [0u8; DICE_CDI_SIZE];
    let mut ignored_cdi_seal = [0u8; DICE_CDI_SIZE];
    let mut actual_next_cdi_attest = [0u8; DICE_CDI_SIZE];
    let mut actual_next_cdi_seal = [0u8; DICE_CDI_SIZE];

    let out_size = out.size();
    let out_overflowed = out.is_overflowed();
    let _ = out;

    let (next_cdi_attest, next_cdi_seal, remaining_buffer) = if out_overflowed {
        (&mut ignored_cdi_attest, &mut ignored_cdi_seal, &mut [][..])
    } else {
        if buffer.len() > out_size {
            (
                &mut actual_next_cdi_attest,
                &mut actual_next_cdi_seal,
                &mut buffer[out_size..],
            )
        } else {
            (&mut ignored_cdi_attest, &mut ignored_cdi_seal, &mut [][..])
        }
    };

    let chain_size = match if let Some(chain_data) = chain {
        dice_android_main_flow(
            context,
            current_cdi_attest,
            current_cdi_seal,
            chain_data,
            input_values,
            remaining_buffer,
            next_cdi_attest,
            next_cdi_seal,
        )
    } else {
        dice_android_main_flow_with_new_dice_chain(
            context,
            current_cdi_attest,
            current_cdi_seal,
            input_values,
            remaining_buffer,
            next_cdi_attest,
            next_cdi_seal,
        )
    } {
        Ok(size) => size,
        Err(DiceResult::BufferTooSmall(chain_required_size)) => {
            let total_required = out_size + chain_required_size as usize;
            return Err(DiceResult::BufferTooSmall(total_required as i32));
        }
        Err(e) => return Err(e),
    };

    if !out_overflowed {
        if next_cdi_attest_written {
            buffer[next_cdi_attest_offset..next_cdi_attest_offset + DICE_CDI_SIZE]
                .copy_from_slice(&actual_next_cdi_attest);
        }
        if next_cdi_seal_written {
            buffer[next_cdi_seal_offset..next_cdi_seal_offset + DICE_CDI_SIZE]
                .copy_from_slice(&actual_next_cdi_seal);
        }
    }

    Ok(out_size + chain_size)
}

#[allow(clippy::type_complexity)]
pub fn dice_android_handover_parse(
    handover: &[u8],
) -> Result<(&[u8; DICE_CDI_SIZE], &[u8; DICE_CDI_SIZE], Option<&[u8]>), DiceResult> {
    let mut input = CborIn::new(handover);

    let num_pairs = input.read_map().map_err(|_| DiceResult::InvalidInput)?;
    if num_pairs < 2 {
        return Err(DiceResult::InvalidInput);
    }

    let label = input.read_int().map_err(|_| DiceResult::InvalidInput)?;
    if label != K_CDI_ATTEST_LABEL {
        return Err(DiceResult::InvalidInput);
    }
    let cdi_attest = input.read_bstr().map_err(|_| DiceResult::InvalidInput)?;
    if cdi_attest.len() != DICE_CDI_SIZE {
        return Err(DiceResult::InvalidInput);
    }

    let label = input.read_int().map_err(|_| DiceResult::InvalidInput)?;
    if label != K_CDI_SEAL_LABEL {
        return Err(DiceResult::InvalidInput);
    }
    let cdi_seal = input.read_bstr().map_err(|_| DiceResult::InvalidInput)?;
    if cdi_seal.len() != DICE_CDI_SIZE {
        return Err(DiceResult::InvalidInput);
    }

    let mut dice_chain = None;
    if num_pairs >= 3
        && let Ok(label) = input.read_int()
        && label == K_DICE_CHAIN_LABEL
    {
        let start = input.offset();
        input.read_skip().map_err(|_| DiceResult::InvalidInput)?;
        let end = input.offset();
        dice_chain = Some(&handover[start..end]);
    }

    let cdi_attest_array: &[u8; DICE_CDI_SIZE] = cdi_attest
        .try_into()
        .map_err(|_| DiceResult::InvalidInput)?;
    let cdi_seal_array: &[u8; DICE_CDI_SIZE] =
        cdi_seal.try_into().map_err(|_| DiceResult::InvalidInput)?;

    Ok((cdi_attest_array, cdi_seal_array, dice_chain))
}

#[cfg(unittest)]
mod tests {
    use core::panic;

    use super::*;
    use crate::test_data::test_constants::{CODEHASH, HANDOVER};

    #[unittest::def_test]
    fn test_dice_android_handover_parse() {
        match dice_android_handover_parse(HANDOVER) {
            Ok((cdi_attest_array, cdi_seal_array, dice_chain)) => {
                assert_eq!(
                    cdi_attest_array.len(),
                    DICE_CDI_SIZE,
                    "cdi_attest should be DICE_CDI_SIZE"
                );
                assert_eq!(
                    cdi_seal_array.len(),
                    DICE_CDI_SIZE,
                    "cdi_seal should be DICE_CDI_SIZE"
                );

                if let Some(chain) = dice_chain {
                    assert!(!chain.is_empty(), "dice_chain should not be empty");
                }

                // panic!("cdi_attest_array: {:02x?}, cdi_seal_array: {:02x?}, dice_chain: {:02x?}",
                //        cdi_attest_array, cdi_seal_array, dice_chain);
            }
            Err(e) => {
                panic!("Failed to parse handover: {:?}", e);
            }
        }
    }

    #[unittest::def_test]
    fn test_dice_android_handover_parse_find_err() {
        use crate::dice_main_flow_chain_codehash;
        let mut input = [0u8; 4096];
        let buffer = match dice_main_flow_chain_codehash(HANDOVER, CODEHASH, &mut input) {
            Ok(buffer) => buffer,
            Err(_) => panic!("Failed to parse handover"),
        };

        let mut input = CborIn::new(buffer);

        let num_pairs = match input.read_map().map_err(|_| DiceResult::InvalidInput) {
            Ok(num_pairs) => {
                if num_pairs < 2 {
                    panic!(
                        "Invalid input: Map must have at least 2 pairs, got {}",
                        num_pairs
                    );
                }
                num_pairs
            }
            Err(_) => panic!("Failed to read Map header"),
        };

        // panic!("Map header: num_pairs={}, current offset={}, remaining bytes: {:02x?}",
        //        num_pairs, input.offset(), &buffer[input.offset()..input.offset().min(input.offset()+20)]);

        let label = match input.read_int().map_err(|_| DiceResult::InvalidInput) {
            Ok(label) => label,
            Err(_) => panic!("Failed to read Attestation CDI Label"),
        };
        if label != K_CDI_ATTEST_LABEL {
            panic!(
                "Invalid input: Attestation CDI Label must be K_CDI_ATTEST_LABEL (1), got {}",
                label
            );
        }

        // panic!("After reading Label 1: current offset={}, remaining bytes: {:02x?}",
        //        input.offset(), &buffer[input.offset()..input.offset().min(input.offset()+20)]);

        let cdi_attest = match input.read_bstr().map_err(|_| DiceResult::InvalidInput) {
            Ok(data) => data,
            Err(_) => {
                let pos = input.offset();
                let remaining = buffer.len().saturating_sub(pos);
                if remaining > 0 {
                    panic!(
                        "Failed to read Attestation CDI data at offset {}, remaining {} bytes, \
                         next bytes: {:02x?}",
                        pos,
                        remaining,
                        &buffer[pos..(pos + 10).min(buffer.len())]
                    )
                } else {
                    panic!(
                        "Failed to read Attestation CDI data at offset {}, no more data available",
                        pos
                    )
                }
            }
        };
        if cdi_attest.len() != DICE_CDI_SIZE {
            panic!(
                "Invalid input: Attestation CDI size must be {}, got {}",
                DICE_CDI_SIZE,
                cdi_attest.len()
            );
        }

        let label = match input.read_int().map_err(|_| DiceResult::InvalidInput) {
            Ok(label) => label,
            Err(_) => panic!("Failed to read Sealing CDI Label"),
        };
        if label != K_CDI_SEAL_LABEL {
            panic!(
                "Invalid input: Sealing CDI Label must be K_CDI_SEAL_LABEL (2), got {}",
                label
            );
        }

        let cdi_seal = match input.read_bstr().map_err(|_| DiceResult::InvalidInput) {
            Ok(data) => data,
            Err(_) => panic!("Failed to read Sealing CDI data"),
        };
        if cdi_seal.len() != DICE_CDI_SIZE {
            panic!(
                "Invalid input: Sealing CDI size must be {}, got {}",
                DICE_CDI_SIZE,
                cdi_seal.len()
            );
        }

        if num_pairs >= 3 {
            match input.read_int() {
                Ok(label) => {
                    if label == K_DICE_CHAIN_LABEL {
                        let start = input.offset();
                        match input.read_skip().map_err(|_| DiceResult::InvalidInput) {
                            Ok(_) => {
                                let end = input.offset();
                                let _dice_chain = &buffer[start..end];
                            }
                            Err(_) => panic!("Failed to skip Dice Chain data"),
                        }
                    } else {
                        panic!(
                            "Invalid input: Third label must be K_DICE_CHAIN_LABEL (3), got {}",
                            label
                        );
                    }
                }
                Err(_) => {
                    panic!("Failed to read Dice Chain Label, but num_pairs >= 3");
                }
            }
        }

        // panic!("All fields parsed successfully!");
    }

    #[unittest::def_test]
    fn test_dice_android_handover_main_flow() {
        let mut input_value = DiceInputValues::new_zero();
        input_value.code_hash = [
            0x14, 0xde, 0xb7, 0x0d, 0x3e, 0xe1, 0x9d, 0x5a, 0x8b, 0x54, 0xac, 0x1a, 0xe4, 0xa0,
            0x9b, 0x51, 0x25, 0x42, 0x26, 0x36, 0x34, 0x14, 0xa3, 0xc3, 0x6a, 0x0e, 0x50, 0x19,
            0x08, 0x99, 0x09, 0xdc,
        ];
        input_value.config_value = crate::tee_dice::CONFIG_VALUE;

        let mut buffer = [0x0u8; 4096];

        let mut out = CborOut::new(&mut buffer);
        out.write_map(3);
        out.write_int(K_CDI_ATTEST_LABEL);
        out.write_bstr(&[0u8; DICE_CDI_SIZE]);
        out.write_int(K_CDI_SEAL_LABEL);
        out.write_bstr(&[0u8; DICE_CDI_SIZE]);
        out.write_int(K_DICE_CHAIN_LABEL);

        let out_size = out.size();
        let out_overflowed = out.is_overflowed();
        let _ = out;
        if out_overflowed {
            // panic!("CBOR output overflowed with size: {}", out_size);
        } else {
            assert!(
                out_size < buffer.len(),
                "CBOR output size should be less than buffer size"
            );
            // panic!("CBOR output size: {}", out_size);
        }

        match dice_android_handover_main_flow(&mut 0, HANDOVER, &input_value, &mut buffer) {
            Ok(size) => {
                assert_ne!(size, 0, "returned size must not be zero");
                // panic!("size: {:?}", size);
            }
            Err(e) => {
                panic!("Failed to parse handover: {:?}", e);
            }
        }
    }

    #[unittest::def_test]
    fn test_dice_android_main_flow() {
        let current_cdi_attest = [
            0x83, 0x6a, 0x04, 0xc7, 0x80, 0x4b, 0xe1, 0xf5, 0xe6, 0x5a, 0x1d, 0x95, 0xc2, 0x52,
            0xc2, 0x59, 0x41, 0xa6, 0xac, 0x6b, 0x46, 0x54, 0x0b, 0x9f, 0x8e, 0x3c, 0x54, 0xb2,
            0xef, 0xcc, 0xae, 0x7f,
        ];
        let current_cdi_seal = [
            0xb6, 0xb8, 0xaa, 0x8b, 0x5f, 0xf0, 0xc7, 0xa7, 0xe7, 0x04, 0xd1, 0x10, 0x27, 0x6d,
            0x65, 0xdf, 0x2f, 0xab, 0xdb, 0x88, 0xa0, 0x4c, 0x2f, 0xe5, 0x15, 0x12, 0x17, 0xfa,
            0x5d, 0x27, 0x01, 0x4d,
        ];
        let chain = [
            0x84, 0xa5, 0x01, 0x01, 0x03, 0x27, 0x04, 0x81, 0x02, 0x20, 0x06, 0x21, 0x58, 0x20,
            0x72, 0x0e, 0x96, 0x83, 0x20, 0xf6, 0xd3, 0x24, 0xd2, 0x94, 0x23, 0xd5, 0x46, 0x52,
            0x4c, 0x7a, 0xcb, 0xb5, 0x49, 0xc1, 0x2a, 0x49, 0xe0, 0x59, 0xdb, 0xc5, 0x08, 0xc5,
            0x60, 0x99, 0xf8, 0x2e, 0x84, 0x43, 0xa1, 0x01, 0x27, 0xa0, 0x59, 0x01, 0x88, 0xa9,
            0x01, 0x78, 0x28, 0x30, 0x32, 0x38, 0x36, 0x30, 0x61, 0x33, 0x30, 0x35, 0x33, 0x63,
            0x32, 0x62, 0x62, 0x63, 0x39, 0x66, 0x63, 0x36, 0x39, 0x62, 0x62, 0x38, 0x32, 0x35,
            0x66, 0x31, 0x34, 0x65, 0x31, 0x34, 0x33, 0x31, 0x36, 0x39, 0x39, 0x65, 0x32, 0x39,
            0x30, 0x02, 0x78, 0x28, 0x37, 0x62, 0x66, 0x38, 0x65, 0x62, 0x32, 0x38, 0x66, 0x39,
            0x39, 0x34, 0x32, 0x38, 0x30, 0x37, 0x30, 0x62, 0x36, 0x38, 0x62, 0x34, 0x62, 0x63,
            0x35, 0x65, 0x34, 0x32, 0x31, 0x63, 0x37, 0x31, 0x66, 0x31, 0x37, 0x65, 0x30, 0x61,
            0x66, 0x31, 0x3a, 0x00, 0x47, 0x44, 0x50, 0x58, 0x40, 0x0b, 0xde, 0x5e, 0x13, 0x6f,
            0xce, 0xb2, 0xc6, 0xfd, 0xed, 0xa5, 0x3e, 0xc8, 0xfa, 0xac, 0x59, 0x5e, 0x88, 0xf4,
            0x6c, 0x5b, 0x5a, 0x93, 0xfe, 0x03, 0xa3, 0xd1, 0x7f, 0x76, 0xf5, 0x75, 0x3c, 0xe9,
            0x4d, 0x57, 0x48, 0xd2, 0xf8, 0x70, 0x9a, 0xc5, 0x46, 0x58, 0x5d, 0xa5, 0xaa, 0x1c,
            0xc1, 0x3b, 0xdb, 0xa7, 0xb0, 0x13, 0xe0, 0x0f, 0xe6, 0xb4, 0xd1, 0x64, 0x6e, 0xb0,
            0x54, 0x23, 0xbf, 0x3a, 0x00, 0x47, 0x44, 0x53, 0x54, 0xa2, 0x3a, 0x00, 0x01, 0x11,
            0x71, 0x63, 0x41, 0x56, 0x42, 0x3a, 0x00, 0x01, 0x11, 0x72, 0x1a, 0x1a, 0x00, 0x01,
            0x73, 0x3a, 0x00, 0x47, 0x44, 0x52, 0x58, 0x40, 0xa6, 0x79, 0x3f, 0x45, 0x09, 0x5c,
            0x98, 0xd1, 0x2b, 0x19, 0x7f, 0xd2, 0x55, 0xc2, 0x40, 0x1c, 0x88, 0x5d, 0x04, 0x2d,
            0xf3, 0x4c, 0x25, 0x77, 0x9f, 0xeb, 0xef, 0xbb, 0xd1, 0x79, 0xe7, 0xf3, 0xfc, 0xe6,
            0xb0, 0x39, 0x35, 0x26, 0xa5, 0x7e, 0x76, 0x37, 0xa3, 0xa7, 0x88, 0x4d, 0x78, 0xe7,
            0x04, 0x7c, 0x15, 0x2d, 0x64, 0xfc, 0xc0, 0x19, 0x28, 0xdb, 0xc5, 0x7a, 0x00, 0xe2,
            0x5f, 0x6d, 0x3a, 0x00, 0x47, 0x44, 0x54, 0x58, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x3a, 0x00, 0x47, 0x44, 0x56, 0x41, 0x02, 0x3a, 0x00, 0x47, 0x44,
            0x57, 0x58, 0x2d, 0xa5, 0x01, 0x01, 0x03, 0x27, 0x04, 0x81, 0x02, 0x20, 0x06, 0x21,
            0x58, 0x20, 0x2c, 0xe5, 0x04, 0x23, 0x62, 0x88, 0x5a, 0x95, 0xd0, 0x89, 0x7c, 0x03,
            0x33, 0x59, 0x51, 0x6f, 0xd8, 0xe9, 0x8c, 0xd2, 0xf5, 0xb8, 0xe3, 0x05, 0x2d, 0xd0,
            0xa1, 0xcd, 0x54, 0x0d, 0xdb, 0x28, 0x3a, 0x00, 0x47, 0x44, 0x58, 0x41, 0x20, 0x58,
            0x40, 0x83, 0x99, 0xc9, 0x44, 0x82, 0x42, 0x7c, 0x98, 0x31, 0x77, 0x60, 0x73, 0xc2,
            0xe5, 0xc3, 0xb7, 0x3f, 0x4f, 0x8a, 0x65, 0x96, 0x01, 0x60, 0x6f, 0x3c, 0x56, 0xaa,
            0xb6, 0xb2, 0x7c, 0x68, 0x54, 0x39, 0x48, 0xcb, 0x57, 0x8a, 0x1c, 0x7b, 0x17, 0xf1,
            0x78, 0xac, 0x20, 0x35, 0x46, 0xd6, 0x9f, 0x61, 0x74, 0x44, 0x3a, 0x88, 0x54, 0x48,
            0xe8, 0x37, 0x16, 0x59, 0x78, 0x81, 0x62, 0xa4, 0x00, 0x84, 0x43, 0xa1, 0x01, 0x27,
            0xa0, 0x59, 0x01, 0x91, 0xa9, 0x01, 0x78, 0x28, 0x37, 0x62, 0x66, 0x38, 0x65, 0x62,
            0x32, 0x38, 0x66, 0x39, 0x39, 0x34, 0x32, 0x38, 0x30, 0x37, 0x30, 0x62, 0x36, 0x38,
            0x62, 0x34, 0x62, 0x63, 0x35, 0x65, 0x34, 0x32, 0x31, 0x63, 0x37, 0x31, 0x66, 0x31,
            0x37, 0x65, 0x30, 0x61, 0x66, 0x31, 0x02, 0x78, 0x28, 0x34, 0x36, 0x63, 0x37, 0x63,
            0x34, 0x31, 0x63, 0x63, 0x36, 0x38, 0x36, 0x63, 0x62, 0x39, 0x32, 0x36, 0x32, 0x66,
            0x64, 0x64, 0x64, 0x31, 0x37, 0x63, 0x38, 0x30, 0x63, 0x37, 0x30, 0x35, 0x33, 0x38,
            0x63, 0x34, 0x64, 0x39, 0x61, 0x33, 0x38, 0x3a, 0x00, 0x47, 0x44, 0x50, 0x58, 0x40,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x3a, 0x00, 0x47, 0x44, 0x53, 0x58,
            0x1c, 0xa1, 0x3a, 0x00, 0x01, 0x11, 0x71, 0x75, 0x50, 0x72, 0x6f, 0x74, 0x65, 0x63,
            0x74, 0x65, 0x64, 0x20, 0x56, 0x4d, 0x20, 0x66, 0x69, 0x72, 0x6d, 0x77, 0x61, 0x72,
            0x65, 0x3a, 0x00, 0x47, 0x44, 0x52, 0x58, 0x40, 0xae, 0xcc, 0xb2, 0xcd, 0xc9, 0x40,
            0x67, 0x6f, 0x71, 0x05, 0x8a, 0x5f, 0xae, 0xb3, 0xd1, 0x00, 0x80, 0x36, 0x81, 0xd7,
            0x45, 0x99, 0xb6, 0x30, 0xb8, 0x08, 0x37, 0x65, 0xc7, 0x4d, 0x69, 0x8a, 0xd4, 0x05,
            0xae, 0x90, 0x0b, 0x08, 0x4f, 0xd7, 0xc0, 0x8f, 0x56, 0x15, 0x13, 0x69, 0xee, 0x05,
            0xc1, 0xe3, 0x33, 0x96, 0x88, 0x48, 0x13, 0xc8, 0xad, 0x23, 0x9e, 0x99, 0x2a, 0x45,
            0xff, 0x23, 0x3a, 0x00, 0x47, 0x44, 0x54, 0x58, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x3a, 0x00, 0x47, 0x44, 0x56, 0x41, 0x01, 0x3a, 0x00, 0x47, 0x44,
            0x57, 0x58, 0x2d, 0xa5, 0x01, 0x01, 0x03, 0x27, 0x04, 0x81, 0x02, 0x20, 0x06, 0x21,
            0x58, 0x20, 0x6e, 0x3e, 0xea, 0xc0, 0x17, 0xd6, 0x6d, 0x86, 0xb9, 0x78, 0x44, 0xbb,
            0x6d, 0xe6, 0xfb, 0x36, 0xe7, 0x83, 0x8a, 0x58, 0x88, 0xe9, 0xda, 0x98, 0xc2, 0x5f,
            0xb0, 0xc4, 0xf7, 0x59, 0x41, 0xa6, 0x3a, 0x00, 0x47, 0x44, 0x58, 0x41, 0x20, 0x58,
            0x40, 0xfc, 0xf3, 0xa8, 0xe1, 0xdb, 0xc9, 0x29, 0xb7, 0xb9, 0x5a, 0xff, 0xfb, 0xc4,
            0x7c, 0x10, 0x48, 0x6f, 0x28, 0x1e, 0x29, 0x7d, 0xe3, 0xb5, 0x5a, 0x35, 0xae, 0x4d,
            0x04, 0xa1, 0x07, 0xf6, 0x07, 0x73, 0xe3, 0x98, 0x2e, 0xb1, 0x5d, 0xfd, 0xa6, 0x55,
            0x86, 0x71, 0x15, 0x16, 0x31, 0x30, 0x6e, 0x72, 0xc8, 0x19, 0x2a, 0xf8, 0xf6, 0x3b,
            0x26, 0x57, 0xa0, 0xb1, 0x0e, 0x74, 0x52, 0x28, 0x04, 0x84, 0x43, 0xa1, 0x01, 0x27,
            0xa0, 0x59, 0x02, 0x0f, 0xaa, 0x01, 0x78, 0x28, 0x34, 0x36, 0x63, 0x37, 0x63, 0x34,
            0x31, 0x63, 0x63, 0x36, 0x38, 0x36, 0x63, 0x62, 0x39, 0x32, 0x36, 0x32, 0x66, 0x64,
            0x64, 0x64, 0x31, 0x37, 0x63, 0x38, 0x30, 0x63, 0x37, 0x30, 0x35, 0x33, 0x38, 0x63,
            0x34, 0x64, 0x39, 0x61, 0x33, 0x38, 0x02, 0x78, 0x28, 0x33, 0x66, 0x31, 0x62, 0x32,
            0x63, 0x65, 0x38, 0x39, 0x63, 0x65, 0x63, 0x30, 0x65, 0x64, 0x63, 0x65, 0x65, 0x31,
            0x65, 0x37, 0x64, 0x63, 0x66, 0x64, 0x38, 0x32, 0x66, 0x33, 0x32, 0x31, 0x36, 0x61,
            0x66, 0x38, 0x37, 0x33, 0x64, 0x38, 0x33, 0x3a, 0x00, 0x47, 0x44, 0x50, 0x58, 0x40,
            0xe0, 0x90, 0xfe, 0x99, 0x0a, 0xee, 0x66, 0x24, 0x8d, 0xd2, 0xb7, 0xae, 0xfa, 0x73,
            0xd4, 0x76, 0x02, 0xab, 0x4d, 0x6b, 0x1c, 0xe9, 0x80, 0x7e, 0x26, 0x77, 0x1d, 0x99,
            0xce, 0x87, 0x46, 0xbc, 0x47, 0x62, 0xc7, 0x6e, 0x11, 0xf5, 0x21, 0x8e, 0x1b, 0xaa,
            0xfd, 0x25, 0x4b, 0xaf, 0x6e, 0x63, 0x57, 0x90, 0x38, 0xf1, 0x9f, 0xbd, 0xf5, 0xdb,
            0xe8, 0xbe, 0x20, 0xaa, 0xad, 0x21, 0x6f, 0xc7, 0x3a, 0x00, 0x47, 0x44, 0x53, 0x58,
            0x5c, 0xa3, 0x3a, 0x00, 0x01, 0x11, 0x71, 0x68, 0x76, 0x6d, 0x5f, 0x65, 0x6e, 0x74,
            0x72, 0x79, 0x3a, 0x00, 0x01, 0x11, 0x74, 0x00, 0x3a, 0x00, 0x01, 0x15, 0x5a, 0x58,
            0x40, 0xf3, 0x7e, 0xb9, 0xad, 0xcf, 0x63, 0x42, 0x03, 0x89, 0x3e, 0xd7, 0x28, 0x13,
            0x39, 0x86, 0x90, 0xc2, 0xf5, 0x18, 0x85, 0x85, 0x35, 0xba, 0x96, 0x5f, 0xd7, 0xd2,
            0x67, 0x24, 0x27, 0xc6, 0x21, 0xe2, 0x2b, 0xea, 0x26, 0x5d, 0xb6, 0x4d, 0xe3, 0xf0,
            0x6e, 0x6e, 0x2d, 0x75, 0x3f, 0xda, 0xd7, 0x3d, 0xda, 0xc5, 0x71, 0x1e, 0x1d, 0xd8,
            0x40, 0x14, 0x4c, 0xb0, 0x94, 0x7d, 0x6e, 0x2f, 0x05, 0x3a, 0x00, 0x47, 0x44, 0x52,
            0x58, 0x40, 0x65, 0x95, 0xf4, 0x2b, 0xa7, 0x92, 0xab, 0xcb, 0xf4, 0x95, 0x5e, 0x3e,
            0x6d, 0xfe, 0xf8, 0xe2, 0x7e, 0x3a, 0x34, 0x3d, 0x17, 0x85, 0xf8, 0x80, 0x27, 0x19,
            0x34, 0xc1, 0x16, 0xbc, 0xae, 0x52, 0xc2, 0xf7, 0x33, 0xfb, 0x42, 0x7f, 0xb8, 0xf5,
            0x23, 0x13, 0xd0, 0xcc, 0xe2, 0x86, 0x81, 0xb8, 0x9b, 0x29, 0x46, 0x78, 0x07, 0x35,
            0x5e, 0x24, 0x53, 0xa0, 0x6b, 0x7d, 0x24, 0x40, 0x4e, 0xbd, 0x3a, 0x00, 0x47, 0x44,
            0x54, 0x58, 0x40, 0x86, 0xfa, 0x63, 0x59, 0xb3, 0xf5, 0xed, 0x49, 0x89, 0xf9, 0xc4,
            0xe7, 0x7b, 0x12, 0xd1, 0x87, 0x6e, 0x2e, 0x75, 0x85, 0x88, 0x07, 0xdd, 0xa2, 0xe7,
            0x71, 0xc5, 0x17, 0xd4, 0xa0, 0xc5, 0xb8, 0x40, 0x1c, 0xec, 0x5f, 0x40, 0x05, 0x84,
            0x87, 0x57, 0x69, 0xb8, 0x71, 0xda, 0x7c, 0xf7, 0x28, 0x9f, 0xb3, 0xa1, 0xc5, 0x09,
            0x34, 0xc6, 0x7c, 0x90, 0x0d, 0xf2, 0xa5, 0x04, 0xac, 0x0f, 0x59, 0x3a, 0x00, 0x47,
            0x44, 0x56, 0x41, 0x01, 0x3a, 0x00, 0x47, 0x44, 0x57, 0x58, 0x51, 0xa6, 0x01, 0x02,
            0x03, 0x38, 0xf7, 0x04, 0x81, 0x02, 0x20, 0x09, 0x21, 0x58, 0x20, 0x5a, 0xd8, 0xba,
            0x38, 0xb5, 0x67, 0xd9, 0x71, 0x49, 0x1f, 0x3e, 0x67, 0xe1, 0xca, 0xb5, 0xf6, 0x9c,
            0x53, 0xe1, 0x40, 0x41, 0xae, 0xb4, 0xd5, 0xb9, 0x78, 0xd0, 0x93, 0xae, 0x93, 0x0c,
            0x7a, 0x22, 0x58, 0x20, 0xba, 0x48, 0x81, 0x0b, 0xcb, 0xaf, 0xf3, 0xbf, 0x7e, 0xa1,
            0xdb, 0xd5, 0x6a, 0x36, 0x04, 0x46, 0xbd, 0x69, 0x56, 0xe4, 0xeb, 0x5f, 0xbb, 0x20,
            0x6e, 0x85, 0x84, 0xa7, 0xe3, 0x46, 0x7e, 0x90, 0x3a, 0x00, 0x47, 0x44, 0x58, 0x41,
            0x20, 0x3a, 0x00, 0x47, 0x44, 0x59, 0x74, 0x6f, 0x70, 0x65, 0x6e, 0x64, 0x69, 0x63,
            0x65, 0x2e, 0x65, 0x78, 0x61, 0x6d, 0x70, 0x6c, 0x65, 0x2e, 0x73, 0x6d, 0x32, 0x58,
            0x40, 0x24, 0x36, 0x6e, 0x5b, 0xf4, 0xb4, 0x58, 0x69, 0xdc, 0x88, 0xcb, 0x6c, 0x8d,
            0x5c, 0x52, 0x56, 0x4d, 0x55, 0x36, 0x36, 0x5d, 0x69, 0xe5, 0x99, 0xca, 0x24, 0x1d,
            0xa7, 0x0b, 0xb4, 0x53, 0xc5, 0xf3, 0x3e, 0x6d, 0x94, 0x25, 0xc2, 0x33, 0x68, 0x79,
            0xa9, 0xa0, 0x42, 0xc2, 0xcf, 0x5b, 0x03, 0x6e, 0x73, 0xd5, 0x8b, 0x53, 0x59, 0xf8,
            0x8c, 0x54, 0x87, 0x7f, 0x4e, 0x85, 0xe1, 0xd7, 0x0d,
        ];
        let mut input_values = DiceInputValues::new_zero();
        input_values.code_hash = [
            0x14, 0xde, 0xb7, 0x0d, 0x3e, 0xe1, 0x9d, 0x5a, 0x8b, 0x54, 0xac, 0x1a, 0xe4, 0xa0,
            0x9b, 0x51, 0x25, 0x42, 0x26, 0x36, 0x34, 0x14, 0xa3, 0xc3, 0x6a, 0x0e, 0x50, 0x19,
            0x08, 0x99, 0x09, 0xdc,
        ];
        input_values.config_value = crate::tee_dice::CONFIG_VALUE;
        let mut buffer = [0x0u8; 1999];
        let buff_test = &mut buffer[..];
        let mut actual_next_cdi_attest = [0u8; DICE_CDI_SIZE];
        let mut actual_next_cdi_seal = [0u8; DICE_CDI_SIZE];

        match dice_android_main_flow(
            &mut 0,
            &current_cdi_attest,
            &current_cdi_seal,
            &chain,
            &input_values,
            buff_test,
            &mut actual_next_cdi_attest,
            &mut actual_next_cdi_seal,
        ) {
            Ok(actual_size) => {
                assert_ne!(actual_size, 0, "Expected non-zero output size");
            }
            Err(DiceResult::BufferTooSmall(required_size)) => {
                // Buffer too small, should return consistent required_size regardless of buffer size
                // Total required: new_chain_prefix(1) + chain_items(1590) + certificate(408) = 1999
                assert_eq!(required_size, 1999, "Expected required_size to be 1999");
            }
            Err(e) => {
                panic!("Failed to execute main flow: {:?}", e);
            }
        }
    }

    #[unittest::def_test]
    fn test_dice_android_format_config_descriptor() {
        // Test 1: Empty config (no flags set)
        let config_values = DiceAndroidConfigValues {
            configs: 0,
            component_name: "",
            component_version: 0,
            security_version: 0,
        };
        let mut buffer = [0u8; 256];
        match dice_android_format_config_descriptor(&config_values, &mut buffer) {
            Ok(size) => {
                assert_eq!(size, 1, "Empty config should produce 1 byte (empty map)");
            }
            Err(e) => {
                panic!("Failed to format empty config: {:?}", e);
            }
        }

        // Test 2: Component name only
        let config_values = DiceAndroidConfigValues {
            configs: DICE_ANDROID_CONFIG_COMPONENT_NAME,
            component_name: "test_component",
            component_version: 0,
            security_version: 0,
        };
        let mut buffer = [0u8; 256];
        match dice_android_format_config_descriptor(&config_values, &mut buffer) {
            Ok(size) => {
                assert!(
                    size > 1,
                    "Component name config should produce more than 1 byte"
                );
            }
            Err(e) => {
                panic!("Failed to format component name config: {:?}", e);
            }
        }

        // Test 3: Component version only
        let config_values = DiceAndroidConfigValues {
            configs: DICE_ANDROID_CONFIG_COMPONENT_VERSION,
            component_name: "",
            component_version: 42,
            security_version: 0,
        };
        let mut buffer = [0u8; 256];
        match dice_android_format_config_descriptor(&config_values, &mut buffer) {
            Ok(size) => {
                assert!(
                    size > 1,
                    "Component version config should produce more than 1 byte"
                );
            }
            Err(e) => {
                panic!("Failed to format component version config: {:?}", e);
            }
        }

        // Test 4: Resettable flag
        let config_values = DiceAndroidConfigValues {
            configs: DICE_ANDROID_CONFIG_RESETTABLE,
            component_name: "",
            component_version: 0,
            security_version: 0,
        };
        let mut buffer = [0u8; 256];
        match dice_android_format_config_descriptor(&config_values, &mut buffer) {
            Ok(size) => {
                assert!(
                    size > 1,
                    "Resettable config should produce more than 1 byte"
                );
            }
            Err(e) => {
                panic!("Failed to format resettable config: {:?}", e);
            }
        }

        // Test 5: Security version only
        let config_values = DiceAndroidConfigValues {
            configs: DICE_ANDROID_CONFIG_SECURITY_VERSION,
            component_name: "",
            component_version: 0,
            security_version: 100,
        };
        let mut buffer = [0u8; 256];
        match dice_android_format_config_descriptor(&config_values, &mut buffer) {
            Ok(size) => {
                assert!(
                    size > 1,
                    "Security version config should produce more than 1 byte"
                );
            }
            Err(e) => {
                panic!("Failed to format security version config: {:?}", e);
            }
        }

        // Test 6: RKP VM marker flag
        let config_values = DiceAndroidConfigValues {
            configs: DICE_ANDROID_CONFIG_RKP_VM_MARKER,
            component_name: "",
            component_version: 0,
            security_version: 0,
        };
        let mut buffer = [0u8; 256];
        match dice_android_format_config_descriptor(&config_values, &mut buffer) {
            Ok(size) => {
                assert!(
                    size > 1,
                    "RKP VM marker config should produce more than 1 byte"
                );
            }
            Err(e) => {
                panic!("Failed to format RKP VM marker config: {:?}", e);
            }
        }

        // Test 7: All flags set
        let config_values = DiceAndroidConfigValues {
            configs: DICE_ANDROID_CONFIG_COMPONENT_NAME
                | DICE_ANDROID_CONFIG_COMPONENT_VERSION
                | DICE_ANDROID_CONFIG_RESETTABLE
                | DICE_ANDROID_CONFIG_SECURITY_VERSION
                | DICE_ANDROID_CONFIG_RKP_VM_MARKER,
            component_name: "full_test_component",
            component_version: 123,
            security_version: 456,
        };
        let mut buffer = [0u8; 256];
        match dice_android_format_config_descriptor(&config_values, &mut buffer) {
            Ok(size) => {
                assert!(size > 10, "Full config should produce more than 10 bytes");
            }
            Err(e) => {
                panic!("Failed to format full config: {:?}", e);
            }
        }

        // Test 8: Buffer too small
        let config_values = DiceAndroidConfigValues {
            configs: DICE_ANDROID_CONFIG_COMPONENT_NAME
                | DICE_ANDROID_CONFIG_COMPONENT_VERSION
                | DICE_ANDROID_CONFIG_RESETTABLE
                | DICE_ANDROID_CONFIG_SECURITY_VERSION
                | DICE_ANDROID_CONFIG_RKP_VM_MARKER,
            component_name: "full_test_component",
            component_version: 123,
            security_version: 456,
        };
        let mut small_buffer = [0u8; 1];
        match dice_android_format_config_descriptor(&config_values, &mut small_buffer) {
            Ok(_) => {
                panic!("Should have failed with buffer too small");
            }
            Err(DiceResult::BufferTooSmall(_)) => {
                // Expected
            }
            Err(e) => {
                panic!("Unexpected error: {:?}", e);
            }
        }
    }
}

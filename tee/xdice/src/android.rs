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

#[expect(
    clippy::too_many_arguments,
    reason = "CG-024-lint-exceptions: clippy::too_many_arguments: mirrors the Open DICE \
              DiceAndroidMainFlow ABI: context, both current CDIs, chain, inputs, output buffer \
              and both next CDIs are distinct caller-owned values"
)]
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

/// CDI_Attest, CDI_Seal and the optional DICE chain borrowed from an Android
/// handover buffer.
pub type AndroidHandover<'a> = (
    &'a [u8; DICE_CDI_SIZE],
    &'a [u8; DICE_CDI_SIZE],
    Option<&'a [u8]>,
);

pub fn dice_android_handover_parse(handover: &[u8]) -> Result<AndroidHandover<'_>, DiceResult> {
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

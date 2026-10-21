// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use xdice::cbor_cert_op::*;

#[test]
fn test_empty_buffer_size_calculation() {
    let protected_attributes = b"test";
    let aad = b"aad";
    let payload_size = 100usize;

    let empty_buffer: &mut [u8] = &mut [];

    match encode_cose_tbs(empty_buffer, protected_attributes, payload_size, aad) {
        Err((DiceResult::BufferTooSmall(_), required_size)) => {
            assert!(required_size > 0, "should return the required size");
        }
        Ok(_) => {
            panic!("should return BufferTooSmall");
        }
        Err((e, _)) => {
            panic!("unexpected error: {:?}", e);
        }
    }
}

#[test]
fn test_cbor_out_empty_buffer() {
    let mut out = CborOut::new(&mut []);
    out.write_array(4);
    out.write_tstr("Signature1");
    out.write_bstr(b"test");
    out.alloc_str(CborType::ByteString, 100);

    assert!(out.is_overflowed(), "should be marked overflowed");
    assert!(out.size() > 0, "cursor should be greater than zero");
}

#[test]
fn test_encode_cwt_empty_buffer() {
    let input_values = DiceInputValues {
        code_hash: [0u8; DICE_HASH_SIZE],
        code_descriptor: &[],
        config_type: DiceConfigType::Inline,
        config_value: [0u8; DICE_INLINE_CONFIG_SIZE],
        config_descriptor: &[],
        authority_hash: [0u8; DICE_HASH_SIZE],
        authority_descriptor: &[],
        mode: DiceMode::Normal,
        hidden: [0u8; DICE_HIDDEN_SIZE],
    };
    let authority_id_hex = "test_authority_id_hex_string_123";
    let subject_id_hex = "test_subject_id_hex_string_4567";
    let encoded_public_key = b"test_public_key_data";

    let empty_buffer: &mut [u8] = &mut [];

    match encode_cwt(
        &mut 0,
        &input_values,
        authority_id_hex,
        subject_id_hex,
        encoded_public_key,
        empty_buffer,
    ) {
        Err((DiceResult::BufferTooSmall(_), required_size)) => {
            assert!(required_size > 0, "should return the required size");
        }
        Ok(_) => {
            panic!("should return BufferTooSmall");
        }
        Err((e, _)) => {
            panic!("unexpected error: {:?}", e);
        }
    }
}

#[test]
fn test_dice_generate_certificate_empty_buffer() {
    let subject_seed = [0u8; DICE_PRIVATE_KEY_SEED_SIZE];
    let authority_seed = [0u8; DICE_PRIVATE_KEY_SEED_SIZE];
    let input_values = DiceInputValues {
        code_hash: [0u8; DICE_HASH_SIZE],
        code_descriptor: &[],
        config_type: DiceConfigType::Inline,
        config_value: [0u8; DICE_INLINE_CONFIG_SIZE],
        config_descriptor: &[],
        authority_hash: [0u8; DICE_HASH_SIZE],
        authority_descriptor: &[],
        mode: DiceMode::Normal,
        hidden: [0u8; DICE_HIDDEN_SIZE],
    };

    let empty_buffer: &mut [u8] = &mut [];

    match dice_generate_certificate(
        &mut 0,
        &subject_seed,
        &authority_seed,
        &input_values,
        empty_buffer,
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

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use zeroize::Zeroize;

use crate::{
    dice::dice_derive_cdi_certificate_id,
    mbedtls_sm2dsa::{dice_hash, dice_keypair_from_seed, dice_sign},
};

pub const DICE_PUBLIC_KEY_BUFFER_SIZE: usize = 64;
pub const DICE_MAX_PUBLIC_KEY_SIZE: usize = 96;
pub const COSE_KEY_KTY_EC2: i64 = 2;
pub const COSE_ALG_SM2: i64 = -248;
pub const COSE_CRV_SM2: i64 = 9;
pub const DICE_SIGNATURE_BUFFER_SIZE: usize = 64;
pub const DICE_PROFILE_NAME: &str = "opendice.example.sm2";
pub const DICE_PRIVATE_KEY_BUFFER_SIZE: usize = 32;
pub const DICE_MAX_PROTECTED_ATTRIBUTES_SIZE: usize = 16;

pub const DICE_CDI_SIZE: usize = 32;
pub const DICE_HASH_SIZE: usize = 32;
pub const DICE_HIDDEN_SIZE: usize = 64;
pub const DICE_INLINE_CONFIG_SIZE: usize = 32; //64
pub const DICE_PRIVATE_KEY_SEED_SIZE: usize = 32;
pub const DICE_ID_SIZE: usize = 20;

pub const K_COSE_KEY_KTY_LABEL: i64 = 1;
pub const K_COSE_KEY_ALG_LABEL: i64 = 3;
pub const K_COSE_KEY_OPS_LABEL: i64 = 4;
pub const K_COSE_KEY_CRV_LABEL: i64 = -1;
pub const K_COSE_KEY_X_LABEL: i64 = -2;
pub const K_COSE_KEY_Y_LABEL: i64 = -3;

// Key Types (KTY)
pub const K_COSE_KEY_KTY_OKP: i64 = 1;
pub const K_COSE_KEY_KTY_EC2: i64 = 2;

// Operations
pub const K_COSE_KEY_OPS_VERIFY: i64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Principal {
    Authority,
    Subject,
}

// impl Principal {
//     pub fn is_authority(&self) -> bool {
//         matches!(self, Self::Authority)
//     }
// }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyParam<'a> {
    pub profile_name: &'a str,

    pub public_key_size: usize,

    pub signature_size: usize,

    pub cose_key_type: i64,
    pub cose_key_algorithm: i64,
    pub cose_key_curve: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiceResult {
    Ok,
    InvalidInput,
    BufferTooSmall(i32),
    PlatformError(i32),
}

#[repr(u8)]
#[derive(PartialEq)]
pub enum CborType {
    UnsignedInt = 0,
    NegativeInt = 1,
    ByteString  = 2,
    TextString  = 3,
    Array       = 4,
    Map         = 5,
    Tag         = 6,
    Simple      = 7,
}

pub struct CborOut<'a> {
    buffer: &'a mut [u8],
    cursor: usize,
    overflowed: bool,
}

impl<'a> CborOut<'a> {
    pub fn new(buffer: &'a mut [u8]) -> Self {
        Self {
            buffer,
            cursor: 0,
            overflowed: false,
        }
    }

    pub fn write_int(&mut self, val: i64) {
        if val >= 0 {
            self.write_type(CborType::UnsignedInt, val as u64);
        } else {
            self.write_type(CborType::NegativeInt, (-1 - val) as u64);
        }
    }

    pub fn write_uint(&mut self, val: u64) {
        self.write_type(CborType::UnsignedInt, val);
    }

    pub fn write_array(&mut self, num_elements: usize) {
        self.write_type(CborType::Array, num_elements as u64);
    }

    pub fn write_map(&mut self, num_pairs: usize) {
        self.write_type(CborType::Map, num_pairs as u64);
    }

    pub fn write_tag(&mut self, tag: u64) {
        self.write_type(CborType::Tag, tag);
    }

    pub fn write_false(&mut self) {
        self.write_type(CborType::Simple, 20);
    }

    pub fn write_true(&mut self) {
        self.write_type(CborType::Simple, 21);
    }

    pub fn write_null(&mut self) {
        self.write_type(CborType::Simple, 22);
    }

    pub fn alloc_str(&mut self, cbor_type: CborType, data_size: usize) -> Option<&mut [u8]> {
        self.write_type(cbor_type, data_size as u64);

        let next = match self.cursor.checked_add(data_size) {
            Some(n) => n,
            None => {
                self.cursor = usize::MAX;
                return None;
            }
        };

        let fits_in_buffer =
            self.cursor <= self.buffer.len() && data_size <= self.buffer.len() - self.cursor;

        let ptr = if fits_in_buffer {
            let start = self.cursor;
            Some(&mut self.buffer[start..start + data_size])
        } else {
            None
        };

        self.cursor = next;
        ptr
    }

    pub fn alloc_bstr(&mut self, data_size: usize) -> Option<&mut [u8]> {
        self.alloc_str(CborType::ByteString, data_size)
    }

    pub fn alloc_tstr(&mut self, data_size: usize) -> Option<&mut [u8]> {
        self.alloc_str(CborType::TextString, data_size)
    }

    fn write_str(&mut self, cbor_type: CborType, data: &[u8]) {
        if let Some(dest) = self.alloc_str(cbor_type, data.len())
            && !data.is_empty()
        {
            dest.copy_from_slice(data);
        }
    }

    pub fn write_bstr(&mut self, data: &[u8]) {
        self.write_str(CborType::ByteString, data);
    }

    pub fn write_tstr(&mut self, text: &str) {
        self.write_str(CborType::TextString, text.as_bytes());
    }

    fn write_type(&mut self, cbor_type: CborType, val: u64) {
        let (size, info) = if val <= 23 {
            (1, val as u8)
        } else if val <= 0xff {
            (2, 24)
        } else if val <= 0xffff {
            (3, 25)
        } else if val <= 0xffffffff {
            (5, 26)
        } else {
            (9, 27)
        };

        let next = match self.cursor.checked_add(size) {
            Some(n) => n,
            None => {
                self.cursor = usize::MAX;
                return;
            }
        };

        let fits_in_buffer =
            self.cursor <= self.buffer.len() && size <= self.buffer.len() - self.cursor;

        if fits_in_buffer {
            let type_code = cbor_type as u8;
            let current = self.cursor;

            self.buffer[current] = (type_code << 5) | info;

            match size {
                2 => self.buffer[current + 1] = val as u8,
                3 => self.buffer[current + 1..current + 3]
                    .copy_from_slice(&(val as u16).to_be_bytes()),
                5 => self.buffer[current + 1..current + 5]
                    .copy_from_slice(&(val as u32).to_be_bytes()),
                9 => self.buffer[current + 1..current + 9].copy_from_slice(&val.to_be_bytes()),
                _ => {}
            }
        }

        self.cursor = next;
    }

    pub fn is_overflowed(&self) -> bool {
        self.overflowed || self.cursor > self.buffer.len()
    }

    pub fn size(&mut self) -> usize {
        self.cursor
    }
}

pub fn encode_protected_attributes(
    context: &mut u8,
    principal: Principal,
    buffer: &mut [u8],
) -> Result<usize, DiceResult> {
    const COSE_HEADER_ALG_LABEL: i64 = 1;

    let mut out = CborOut::new(buffer);

    out.write_map(1);

    let key_param = dice_get_key_param(context, principal)?;

    out.write_int(COSE_HEADER_ALG_LABEL);
    out.write_int(key_param.cose_key_algorithm);

    if out.is_overflowed() {
        Err(DiceResult::BufferTooSmall(-1))
    } else {
        Ok(out.size())
    }
}

pub fn dice_get_key_param(
    _context: &mut u8,
    _principal: Principal,
) -> Result<KeyParam<'static>, DiceResult> {
    Ok(KeyParam {
        profile_name: DICE_PROFILE_NAME,
        public_key_size: DICE_PUBLIC_KEY_BUFFER_SIZE,
        signature_size: DICE_SIGNATURE_BUFFER_SIZE,

        cose_key_type: COSE_KEY_KTY_EC2,
        cose_key_algorithm: COSE_ALG_SM2,
        cose_key_curve: COSE_CRV_SM2,
    })
}

pub struct TbsResult {
    pub encoded_size: usize,
    pub payload_offset: usize,
}

pub fn encode_cose_tbs(
    buffer: &mut [u8],
    protected_attributes: &[u8],
    payload_size: usize,
    aad: &[u8],
) -> Result<TbsResult, (DiceResult, usize)> {
    let (encoded_size, overflowed) = {
        let mut out = CborOut::new(buffer);
        out.write_array(4);
        out.write_tstr("Signature1");
        out.write_bstr(protected_attributes);
        out.write_bstr(aad);
        out.alloc_str(CborType::ByteString, payload_size);

        (out.size(), out.is_overflowed())
    };

    if overflowed {
        return Err((DiceResult::BufferTooSmall(-1), encoded_size));
    }

    let start = encoded_size
        .checked_sub(payload_size)
        .ok_or((DiceResult::PlatformError(-1), 0))?;

    Ok(TbsResult {
        encoded_size,
        payload_offset: start,
    })
}

pub fn encode_cose_sign1(
    context: &mut u8,
    protected_attributes: &[u8],
    payload: &[u8],
    move_payload: bool,
    signature: &[u8; DICE_SIGNATURE_BUFFER_SIZE],
    buffer: &mut [u8],
) -> Result<usize, (DiceResult, usize)> {
    let payload_offset = if move_payload {
        buffer
            .windows(payload.len())
            .position(|candidate| core::ptr::eq(candidate.as_ptr(), payload.as_ptr()))
    } else {
        None
    };
    let mut out = CborOut::new(buffer);

    out.write_array(4);

    out.write_bstr(protected_attributes);

    out.write_map(0);

    if move_payload {
        let payload_size = payload.len();
        if let Some(src_offset) = payload_offset {
            if let Some(dest) = out.alloc_bstr(payload_size) {
                let dest_offset = dest.as_ptr() as usize - out.buffer.as_ptr() as usize;
                if src_offset < dest_offset {
                    return Err((DiceResult::PlatformError(-1), 0));
                }

                out.buffer
                    .copy_within(src_offset..src_offset + payload_size, dest_offset);
            }
        } else {
            out.write_bstr(payload);
        }
    } else {
        out.write_bstr(payload);
    }

    let key_param = dice_get_key_param(context, Principal::Authority).map_err(|e| (e, 0usize))?;
    out.write_bstr(&signature[..key_param.signature_size]);

    let encoded_size = out.size();
    if out.is_overflowed() {
        return Err((DiceResult::BufferTooSmall(-1), encoded_size));
    }

    Ok(encoded_size)
}

pub fn dice_cose_sign_and_encode_sign1(
    context: &mut u8,
    payload: &[u8],
    aad: &[u8],
    private_key: &[u8; DICE_PRIVATE_KEY_BUFFER_SIZE],
    buffer: &mut [u8],
) -> Result<usize, DiceResult> {
    let mut protected_attributes = [0u8; DICE_MAX_PROTECTED_ATTRIBUTES_SIZE];

    let prot_attrs_size =
        encode_protected_attributes(context, Principal::Authority, &mut protected_attributes)
            .map_err(|_| DiceResult::PlatformError(-1))?;

    let prot_attrs_slice = &protected_attributes[..prot_attrs_size];

    let tbs_res = match encode_cose_tbs(buffer, prot_attrs_slice, payload.len(), aad) {
        Ok(res) => res,
        Err((DiceResult::BufferTooSmall(-1), _tbs_size)) => {
            return Err(DiceResult::BufferTooSmall(-1));
        }
        Err((e, _)) => return Err(e),
    };
    let payload_offset = tbs_res.payload_offset;
    let payload_end = payload_offset + payload.len();
    if payload_end > buffer.len() {
        return Err(DiceResult::PlatformError(-1));
    }
    buffer[payload_offset..payload_end].copy_from_slice(payload);

    let signature = dice_sign(context, &buffer[..tbs_res.encoded_size], private_key)
        .map_err(|_| DiceResult::PlatformError(-1))?;
    match encode_cose_sign1(
        context,
        prot_attrs_slice,
        payload,
        false,
        &signature,
        buffer,
    ) {
        Ok(size) => Ok(size),
        Err((e, _)) => Err(e),
    }
}

#[derive(Debug, Clone, Copy)]
pub enum DiceMode {
    NotInitialized,
    Normal,
    Debug,
    Maintenance,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiceConfigType {
    Inline,
    Descriptor,
}

#[derive(Debug)]
pub struct DiceInputValues<'a> {
    pub code_hash: [u8; DICE_HASH_SIZE],
    pub code_descriptor: &'a [u8],
    pub config_type: DiceConfigType,
    pub config_value: [u8; DICE_INLINE_CONFIG_SIZE],
    pub config_descriptor: &'a [u8],
    pub authority_hash: [u8; DICE_HASH_SIZE],
    pub authority_descriptor: &'a [u8],
    pub mode: DiceMode,
    pub hidden: [u8; DICE_HIDDEN_SIZE],
}

// Provide a convenient way to create an all-zero/default value.
// Implementing `Default` allows callers to use `DiceInputValues::default()`
// or `DiceInputValues { ..Default::default() }` and keeps the layout
// compatible with the C-style zero initialization semantics.
impl<'a> Default for DiceInputValues<'a> {
    fn default() -> Self {
        Self {
            code_hash: [0u8; DICE_HASH_SIZE],
            code_descriptor: &[],
            config_type: DiceConfigType::Inline,
            config_value: [0u8; DICE_INLINE_CONFIG_SIZE],
            config_descriptor: &[],
            authority_hash: [0u8; DICE_HASH_SIZE],
            authority_descriptor: &[],
            mode: DiceMode::NotInitialized,
            hidden: [0u8; DICE_HIDDEN_SIZE],
        }
    }
}

impl<'a> DiceInputValues<'a> {
    /// Convenience constructor identical to `Default::default()`.
    pub fn new_zero() -> Self {
        Default::default()
    }
}

pub fn encode_cwt(
    context: &mut u8,
    input_values: &DiceInputValues,
    authority_id_hex: &str,
    subject_id_hex: &str,
    encoded_public_key: &[u8],
    buffer: &mut [u8],
) -> Result<usize, (DiceResult, usize)> {
    const CWT_ISSUER_LABEL: i64 = 1;
    const CWT_SUBJECT_LABEL: i64 = 2;
    const CODE_HASH_LABEL: i64 = -4670545;
    const CODE_DESCRIPTOR_LABEL: i64 = -4670546;
    const CONFIG_HASH_LABEL: i64 = -4670547;
    const CONFIG_DESCRIPTOR_LABEL: i64 = -4670548;
    const AUTHORITY_HASH_LABEL: i64 = -4670549;
    const AUTHORITY_DESCRIPTOR_LABEL: i64 = -4670550;
    const MODE_LABEL: i64 = -4670551;
    const SUBJECT_PUBLIC_KEY_LABEL: i64 = -4670552;
    const KEY_USAGE_LABEL: i64 = -4670553;
    const PROFILE_NAME_LABEL: i64 = -4670554;
    const KEY_USAGE_CERT_SIGN: u8 = 32;

    let mut map_pairs = 7;
    if !input_values.code_descriptor.is_empty() {
        map_pairs += 1;
    }
    if input_values.config_type == DiceConfigType::Descriptor {
        map_pairs += 2;
    } else {
        map_pairs += 1;
    }
    if !input_values.authority_descriptor.is_empty() {
        map_pairs += 1;
    }

    let key_param = dice_get_key_param(context, Principal::Subject).map_err(|e| (e, 0usize))?;

    if !key_param.profile_name.is_empty() {
        map_pairs += 1;
    }

    let mut out = CborOut::new(buffer);
    out.write_map(map_pairs);

    out.write_int(CWT_ISSUER_LABEL);
    out.write_tstr(authority_id_hex);

    out.write_int(CWT_SUBJECT_LABEL);
    out.write_tstr(subject_id_hex);

    out.write_int(CODE_HASH_LABEL);
    out.write_bstr(&input_values.code_hash);

    if !input_values.code_descriptor.is_empty() {
        out.write_int(CODE_DESCRIPTOR_LABEL);
        out.write_bstr(input_values.code_descriptor);
    }

    if input_values.config_type == DiceConfigType::Descriptor {
        let mut config_descriptor_hash = [0u8; DICE_HASH_SIZE];
        if !out.is_overflowed() {
            dice_hash(
                context,
                input_values.config_descriptor,
                &mut config_descriptor_hash,
            )
            .map_err(|_| (DiceResult::PlatformError(-1), 0usize))?;
        }
        out.write_int(CONFIG_DESCRIPTOR_LABEL);
        out.write_bstr(input_values.config_descriptor);
        out.write_int(CONFIG_HASH_LABEL);
        out.write_bstr(&config_descriptor_hash);
    } else if input_values.config_type == DiceConfigType::Inline {
        out.write_int(CONFIG_DESCRIPTOR_LABEL);
        out.write_bstr(&input_values.config_value);
    }

    out.write_int(AUTHORITY_HASH_LABEL);
    out.write_bstr(&input_values.authority_hash);

    if !input_values.authority_descriptor.is_empty() {
        out.write_int(AUTHORITY_DESCRIPTOR_LABEL);
        out.write_bstr(input_values.authority_descriptor);
    }

    let mode_byte = [input_values.mode as u8];
    out.write_int(MODE_LABEL);
    out.write_bstr(&mode_byte);

    out.write_int(SUBJECT_PUBLIC_KEY_LABEL);
    out.write_bstr(encoded_public_key);

    let key_usage = [KEY_USAGE_CERT_SIGN];
    out.write_int(KEY_USAGE_LABEL);
    out.write_bstr(&key_usage);

    if !key_param.profile_name.is_empty() {
        out.write_int(PROFILE_NAME_LABEL);
        out.write_tstr(key_param.profile_name);
    }

    let encoded_size = out.size();
    if out.is_overflowed() {
        Err((DiceResult::BufferTooSmall(-1), encoded_size))
    } else {
        Ok(encoded_size)
    }
}

// no_std compatible hex encoding function
pub fn dice_hex_encode(input: &[u8], output: &mut [u8]) {
    const HEX_MAP: &[u8; 16] = b"0123456789abcdef";

    let mut out_pos = 0;
    for &byte in input {
        if out_pos < output.len() {
            output[out_pos] = HEX_MAP[(byte >> 4) as usize];
            out_pos += 1;
        }
        if out_pos < output.len() {
            output[out_pos] = HEX_MAP[(byte & 0x0F) as usize];
            out_pos += 1;
        }
    }
}

pub fn dice_clear_memory(_context: &mut u8, buffer: &mut [u8]) {
    buffer.zeroize();
}

pub fn dice_generate_certificate(
    context: &mut u8,
    subject_seed: &[u8; DICE_PRIVATE_KEY_SEED_SIZE],
    authority_seed: &[u8; DICE_PRIVATE_KEY_SEED_SIZE],
    input_values: &DiceInputValues,
    certificate: &mut [u8],
) -> Result<usize, DiceResult> {
    if input_values.config_type != DiceConfigType::Descriptor
        && input_values.config_type != DiceConfigType::Inline
    {
        return Err(DiceResult::InvalidInput);
    }

    let (subject_public, subject_private) =
        dice_keypair_from_seed(subject_seed).map_err(|_| DiceResult::PlatformError(-1))?;
    let mut subject_private_buf = subject_private;

    let mut subject_id = [0u8; DICE_ID_SIZE];
    let _subj_key_param = dice_get_key_param(context, Principal::Subject)?;
    dice_derive_cdi_certificate_id(context, &subject_public, &mut subject_id)?;

    let mut subject_id_hex = [0u8; 41];
    dice_hex_encode(&subject_id, &mut subject_id_hex);
    let subject_id_str =
        core::str::from_utf8(&subject_id_hex[..40]).map_err(|_| DiceResult::PlatformError(-1))?;

    let (authority_public, authority_private) =
        dice_keypair_from_seed(authority_seed).map_err(|_| {
            dice_clear_memory(context, &mut subject_private_buf);
            DiceResult::PlatformError(-1)
        })?;
    let mut authority_private_buf = authority_private;

    let mut authority_id = [0u8; DICE_ID_SIZE];
    let _auth_key_param = dice_get_key_param(context, Principal::Authority)?;
    dice_derive_cdi_certificate_id(context, &authority_public, &mut authority_id)?;

    let mut authority_id_hex = [0u8; 41];
    dice_hex_encode(&authority_id, &mut authority_id_hex);
    let authority_id_str =
        core::str::from_utf8(&authority_id_hex[..40]).map_err(|_| DiceResult::PlatformError(-1))?;

    let mut encoded_pub_key = [0u8; DICE_MAX_PUBLIC_KEY_SIZE];
    let encoded_pub_key_size = dice_cose_encode_public_key(
        context,
        Principal::Subject,
        &subject_public,
        &mut encoded_pub_key,
    )
    .map_err(|_| {
        dice_clear_memory(context, &mut subject_private_buf);
        dice_clear_memory(context, &mut authority_private_buf);
        DiceResult::PlatformError(-1)
    })?;
    let encoded_pub_key_slice = &encoded_pub_key[..encoded_pub_key_size];

    let mut protected_attrs = [0u8; DICE_MAX_PROTECTED_ATTRIBUTES_SIZE];
    let prot_attrs_size =
        encode_protected_attributes(context, Principal::Authority, &mut protected_attrs).map_err(
            |_| {
                dice_clear_memory(context, &mut subject_private_buf);
                dice_clear_memory(context, &mut authority_private_buf);
                DiceResult::PlatformError(-1)
            },
        )?;
    let prot_attrs_slice = &protected_attrs[..prot_attrs_size];

    let cwt_size = match encode_cwt(
        context,
        input_values,
        authority_id_str,
        subject_id_str,
        encoded_pub_key_slice,
        &mut [][..],
    ) {
        Ok(s) => s,
        Err((_, required_size)) => required_size,
    };

    let tbs_res = match encode_cose_tbs(certificate, prot_attrs_slice, cwt_size, &[]) {
        Ok(r) => r,
        Err((_, _tbs_size)) => {
            dice_clear_memory(context, &mut subject_private_buf);
            dice_clear_memory(context, &mut authority_private_buf);
            // TBS = array(4) + "Signature1" + protected_attrs + aad + payload
            // COSE_Sign1 = array(4) + protected_attrs + empty_map + payload + signature
            let key_param = dice_get_key_param(context, Principal::Authority)
                .map_err(|_| DiceResult::PlatformError(-1))?;
            let mut temp_out = CborOut::new(&mut []);
            temp_out.write_array(4);
            temp_out.write_bstr(prot_attrs_slice);
            temp_out.write_map(0);
            temp_out.write_bstr(&[]);
            temp_out.write_bstr(&[0u8; DICE_SIGNATURE_BUFFER_SIZE][..key_param.signature_size]);
            let cert_size = temp_out.size() - 1
                + if cwt_size <= 23 {
                    1 + cwt_size
                } else if cwt_size <= 0xff {
                    2 + cwt_size
                } else if cwt_size <= 0xffff {
                    3 + cwt_size
                } else {
                    5 + cwt_size
                };
            return Err(DiceResult::BufferTooSmall(cert_size as i32));
        }
    };

    let payload_offset = tbs_res.payload_offset;
    let payload_end = payload_offset.checked_add(cwt_size).ok_or_else(|| {
        dice_clear_memory(context, &mut subject_private_buf);
        dice_clear_memory(context, &mut authority_private_buf);
        DiceResult::PlatformError(-1)
    })?;

    if payload_end > certificate.len() {
        dice_clear_memory(context, &mut subject_private_buf);
        dice_clear_memory(context, &mut authority_private_buf);
        let mut temp_out = CborOut::new(&mut []);
        temp_out.write_array(4);
        temp_out.write_bstr(prot_attrs_slice);
        temp_out.write_map(0);
        temp_out.write_bstr(&[]);
        let key_param = dice_get_key_param(context, Principal::Authority)
            .map_err(|_| DiceResult::PlatformError(-1))?;
        temp_out.write_bstr(&[0u8; DICE_SIGNATURE_BUFFER_SIZE][..key_param.signature_size]);
        let header_size = temp_out.size();
        let payload_encoded_size = if cwt_size <= 23 {
            1 + cwt_size
        } else if cwt_size <= 0xff {
            2 + cwt_size
        } else if cwt_size <= 0xffff {
            3 + cwt_size
        } else {
            5 + cwt_size
        };
        let required_size = header_size - 1 + payload_encoded_size;
        return Err(DiceResult::BufferTooSmall(required_size as i32));
    }

    let payload_buf = &mut certificate[payload_offset..payload_end];
    let final_cwt_size = match encode_cwt(
        context,
        input_values,
        authority_id_str,
        subject_id_str,
        encoded_pub_key_slice,
        payload_buf,
    ) {
        Ok(s) => s,
        Err((..)) => {
            dice_clear_memory(context, &mut subject_private_buf);
            dice_clear_memory(context, &mut authority_private_buf);
            return Err(DiceResult::PlatformError(-1));
        }
    };

    if final_cwt_size != cwt_size {
        dice_clear_memory(context, &mut subject_private_buf);
        dice_clear_memory(context, &mut authority_private_buf);
        return Err(DiceResult::PlatformError(-1));
    }

    let signature = dice_sign(
        context,
        &certificate[..tbs_res.encoded_size],
        &authority_private_buf,
    )
    .map_err(|err_code: i32| {
        dice_clear_memory(context, &mut subject_private_buf);
        dice_clear_memory(context, &mut authority_private_buf);
        DiceResult::PlatformError(err_code)
    })?;

    let mut payload_copy = [0u8; 4096];
    let payload_len = payload_end - payload_offset;
    if payload_len > payload_copy.len() {
        dice_clear_memory(context, &mut subject_private_buf);
        dice_clear_memory(context, &mut authority_private_buf);
        return Err(DiceResult::BufferTooSmall(-2));
    }
    payload_copy[..payload_len].copy_from_slice(&certificate[payload_offset..payload_end]);
    let cert_size = match encode_cose_sign1(
        context,
        prot_attrs_slice,
        &payload_copy[..payload_len],
        true,
        &signature,
        certificate,
    ) {
        Ok(size) => size,
        Err((_, required_size)) => {
            dice_clear_memory(context, &mut subject_private_buf);
            dice_clear_memory(context, &mut authority_private_buf);
            return Err(DiceResult::BufferTooSmall(required_size as i32));
        }
    };

    dice_clear_memory(context, &mut subject_private_buf);
    dice_clear_memory(context, &mut authority_private_buf);

    Ok(cert_size)
}

pub fn dice_cose_encode_public_key(
    context: &mut u8,
    principal: Principal,
    public_key: &[u8; DICE_PUBLIC_KEY_BUFFER_SIZE],
    buffer: &mut [u8],
) -> Result<usize, DiceResult> {
    let key_param = dice_get_key_param(context, principal)?;
    let mut out = CborOut::new(buffer);

    match key_param.cose_key_type {
        K_COSE_KEY_KTY_OKP => out.write_map(5),
        K_COSE_KEY_KTY_EC2 => out.write_map(6),
        _ => return Err(DiceResult::InvalidInput),
    }

    out.write_int(K_COSE_KEY_KTY_LABEL);
    out.write_int(key_param.cose_key_type);

    out.write_int(K_COSE_KEY_ALG_LABEL);
    out.write_int(key_param.cose_key_algorithm);

    out.write_int(K_COSE_KEY_OPS_LABEL);
    out.write_array(1);
    out.write_int(K_COSE_KEY_OPS_VERIFY);

    out.write_int(K_COSE_KEY_CRV_LABEL);
    out.write_int(key_param.cose_key_curve);

    match key_param.cose_key_type {
        K_COSE_KEY_KTY_OKP => {
            out.write_int(K_COSE_KEY_X_LABEL);
            out.write_bstr(&public_key[..key_param.public_key_size]);
        }
        K_COSE_KEY_KTY_EC2 => {
            let xy_size = key_param.public_key_size / 2;

            out.write_int(K_COSE_KEY_X_LABEL);
            out.write_bstr(&public_key[..xy_size]);

            out.write_int(K_COSE_KEY_Y_LABEL);
            out.write_bstr(&public_key[xy_size..(xy_size * 2)]);
        }
        _ => unreachable!(),
    }

    if out.is_overflowed() {
        Err(DiceResult::BufferTooSmall(-1))
    } else {
        Ok(out.size())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

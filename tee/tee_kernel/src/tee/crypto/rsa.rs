// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec;

use mbedtls::{
    error::{Error as MbedError, HiError},
    hash::Type as MdType,
    pk::{Pk, Type},
};
use tee_raw_sys::{
    TEE_ALG_MD5, TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA1, TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA224,
    TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA256, TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA384,
    TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA512, TEE_ALG_RSAES_PKCS1_V1_5,
    TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA1, TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA224,
    TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA256, TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA384,
    TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA512, TEE_ALG_RSASSA_PKCS1_V1_5_MD5, TEE_ALG_SHA1,
    TEE_ALG_SHA224, TEE_ALG_SHA256, TEE_ALG_SHA384, TEE_ALG_SHA512, TEE_ERROR_BAD_PARAMETERS,
    TEE_ERROR_BAD_STATE, TEE_ERROR_NOT_SUPPORTED, TEE_ERROR_SHORT_BUFFER,
};

use crate::tee::{
    TEE_ALG_RSAES_PKCS1_OAEP_MGF1_MD5, TEE_ALG_RSASSA_PKCS1_PSS_MGF1_MD5, TeeResult,
    libmbedtls::rsa::get_tee_result, libutee::utee_defines::tee_internal_hash_to_algo,
    rng_software::TeeSoftwareRng,
};

/// Map mbedtls PK/RSA operation errors to GP TEE status codes.
pub(crate) fn map_pk_op_err(e: MbedError) -> u32 {
    if matches!(
        e.high_level(),
        Some(HiError::PkSigLenMismatch) | Some(HiError::RsaOutputTooLarge)
    ) {
        return TEE_ERROR_SHORT_BUFFER;
    }
    match get_tee_result(e.to_int()) {
        Err(code) => code,
        Ok(()) => TEE_ERROR_BAD_STATE,
    }
}

pub(crate) fn rsa_modulus_byte_len(pk: &Pk) -> usize {
    pk.rsa_modulus_byte_len().unwrap_or_else(|_| pk.len() / 8)
}

fn tee_hash_algo_to_md_type(algo: u32) -> Option<MdType> {
    match algo {
        TEE_ALG_MD5
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_MD5
        | TEE_ALG_RSASSA_PKCS1_V1_5_MD5
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_MD5 => Some(MdType::Md5),
        TEE_ALG_SHA1 | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA1 | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA1 => {
            Some(MdType::Sha1)
        }
        TEE_ALG_SHA224
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA224
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA224 => Some(MdType::Sha224),
        TEE_ALG_SHA256
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA256
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA256 => Some(MdType::Sha256),
        TEE_ALG_SHA384
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA384
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA384 => Some(MdType::Sha384),
        TEE_ALG_SHA512
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA512
        | TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA512 => Some(MdType::Sha512),
        _ => None,
    }
}

/// OP-TEE rejects OAEP when MGF1 hash differs from the operation's main hash.
pub(crate) fn rsaes_oaep_check_mgf(algo: u32, mgf_algo: u32) -> TeeResult {
    if algo == TEE_ALG_RSAES_PKCS1_V1_5 {
        return Ok(());
    }
    let Some(algo_md) = tee_hash_algo_to_md_type(algo) else {
        return Err(TEE_ERROR_NOT_SUPPORTED);
    };
    let Some(mgf_md) = tee_hash_algo_to_md_type(mgf_algo) else {
        return Err(TEE_ERROR_NOT_SUPPORTED);
    };
    if algo_md != mgf_md {
        return Err(TEE_ERROR_NOT_SUPPORTED);
    }
    Ok(())
}

pub(crate) fn resolve_rsaes_mgf_algo(algo: u32, mgf_algo: Option<u32>) -> u32 {
    mgf_algo.unwrap_or_else(|| tee_internal_hash_to_algo(algo))
}

/// RSAES decrypt aligned with OP-TEE `sw_crypto_acipher_rsaes_decrypt` (temp buffer + modulus input).
pub(crate) fn rsaes_decrypt(
    pk: &mut Pk,
    algo: u32,
    mgf_algo: u32,
    input: &[u8],
    output: &mut [u8],
    label: &[u8],
    required: &mut usize,
) -> TeeResult<usize> {
    rsaes_oaep_check_mgf(algo, mgf_algo)?;

    let mod_size = rsa_modulus_byte_len(pk);
    if mod_size == 0 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    let mut rng = TeeSoftwareRng::new();

    match algo {
        TEE_ALG_RSAES_PKCS1_V1_5 => {
            if input.len() > mod_size {
                return Err(TEE_ERROR_BAD_PARAMETERS);
            }
            let upper = mod_size.saturating_sub(11);
            let mut buf = vec![0u8; upper];
            let len = pk
                .decrypt_extend(input, &mut buf, &mut rng, None)
                .map_err(map_pk_op_err)?;
            *required = len;
            if output.len() < len {
                return Err(TEE_ERROR_SHORT_BUFFER);
            }
            output[..len].copy_from_slice(&buf[..len]);
            Ok(len)
        }
        TEE_ALG_RSAES_PKCS1_OAEP_MGF1_MD5
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA1
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA224
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA256
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA384
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA512 => {
            if input.len() != mod_size {
                return Err(TEE_ERROR_BAD_PARAMETERS);
            }
            let mut buf = vec![0u8; input.len()];
            let len = if label.is_empty() {
                pk.decrypt(input, &mut buf, &mut rng)
            } else {
                pk.decrypt_with_label(input, &mut buf, &mut rng, label)
            }
            .map_err(map_pk_op_err)?;
            *required = len;
            if output.len() < len {
                return Err(TEE_ERROR_SHORT_BUFFER);
            }
            output[..len].copy_from_slice(&buf[..len]);
            Ok(len)
        }
        _ => Err(TEE_ERROR_BAD_PARAMETERS),
    }
}

pub(crate) fn check_rsa_cipher_output(
    pk: &Pk,
    output_len: usize,
    required: &mut usize,
) -> TeeResult {
    let mod_size = rsa_modulus_byte_len(pk);
    *required = mod_size;
    if output_len < mod_size {
        return Err(TEE_ERROR_SHORT_BUFFER);
    }
    Ok(())
}

pub(crate) fn check_pk_sign_output(pk: &Pk, output_len: usize, required: &mut usize) -> TeeResult {
    match pk.pk_type() {
        Type::Rsa | Type::RsaAlt | Type::RsassaPss => {
            check_rsa_cipher_output(pk, output_len, required)
        }
        Type::Eckey | Type::Ecdsa => super::ecc::check_ecdsa_sign_output(pk, output_len, required),
        Type::SM2 => super::ecc::check_sm2_sign_output(output_len, required),
        _ => Err(TEE_ERROR_BAD_PARAMETERS),
    }
}

/// Leading zero bytes stripped from a raw RSA block (OP-TEE `rsanopad` semantics).
fn rsa_nopad_out_len(buf: &[u8]) -> usize {
    let mod_size = buf.len();
    let mut offset = 0usize;
    while offset < mod_size.saturating_sub(1) && buf[offset] == 0 {
        offset += 1;
    }
    mod_size - offset
}

/// Raw RSA public operation with zero padding, matching OP-TEE `crypto_acipher_rsanopad_encrypt`.
pub(crate) fn rsa_nopad_encrypt(
    pk: &Pk,
    src: &[u8],
    dst: &mut [u8],
    required: &mut usize,
) -> TeeResult<usize> {
    if pk.pk_type() != Type::Rsa {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    let mod_size = rsa_modulus_byte_len(pk);
    if mod_size == 0 || src.len() > mod_size {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    let mut buf = vec![0u8; mod_size];
    buf[mod_size - src.len()..].copy_from_slice(src);

    pk.rsa_nopad_public(&mut buf).map_err(map_pk_op_err)?;

    let offset = mod_size - rsa_nopad_out_len(&buf);
    let out_len = mod_size - offset;
    *required = out_len;
    if dst.len() < out_len {
        return Err(TEE_ERROR_SHORT_BUFFER);
    }
    dst[..out_len].copy_from_slice(&buf[offset..offset + out_len]);
    Ok(out_len)
}

/// Raw RSA private operation with zero padding, matching OP-TEE `crypto_acipher_rsanopad_decrypt`.
pub(crate) fn rsa_nopad_decrypt(
    pk: &mut Pk,
    src: &[u8],
    dst: &mut [u8],
    required: &mut usize,
) -> TeeResult<usize> {
    if pk.pk_type() != Type::Rsa {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    let mod_size = rsa_modulus_byte_len(pk);
    if mod_size == 0 || src.len() > mod_size {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    let mut buf = vec![0u8; mod_size];
    buf[mod_size - src.len()..].copy_from_slice(src);

    let mut rng = TeeSoftwareRng::new();
    pk.rsa_nopad_private(&mut buf, &mut rng)
        .map_err(map_pk_op_err)?;

    let offset = mod_size - rsa_nopad_out_len(&buf);
    let out_len = mod_size - offset;
    *required = out_len;
    if dst.len() < out_len {
        return Err(TEE_ERROR_SHORT_BUFFER);
    }
    dst[..out_len].copy_from_slice(&buf[offset..offset + out_len]);
    Ok(out_len)
}

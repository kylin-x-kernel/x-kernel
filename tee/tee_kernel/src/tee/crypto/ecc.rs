// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use mbedtls::{
    error::Error as MbedError,
    hash::Type as MdType,
    pk::{Pk, SM2_RAW_SIG_LEN, Type},
    rng::RngCallback,
};
use tee_raw_sys::{TEE_ERROR_BAD_PARAMETERS, TEE_ERROR_SHORT_BUFFER};

use crate::tee::TeeResult;

pub(crate) fn check_sm2_sign_output(output_len: usize, required: &mut usize) -> TeeResult {
    *required = SM2_RAW_SIG_LEN;
    if output_len < SM2_RAW_SIG_LEN {
        Err(TEE_ERROR_SHORT_BUFFER)
    } else {
        Ok(())
    }
}

/// OP-TEE `sm2_mbedtls_dsa_verify`: `digest` is precomputed e, sig is 64-byte R||S.
pub(crate) fn sm2_verify_digest_raw(
    pk: &mut Pk,
    digest: &[u8],
    sig: &[u8],
) -> Result<(), MbedError> {
    pk.sm2_verify_digest_raw(MdType::SM3, digest, sig)
}

/// OP-TEE `sm2_mbedtls_dsa_sign`: `digest` is precomputed e, output is 64-byte R||S.
pub(crate) fn sm2_sign_digest_raw<F: RngCallback>(
    pk: &mut Pk,
    digest: &[u8],
    sig: &mut [u8],
    required: &mut usize,
    rng: &mut F,
) -> Result<usize, MbedError> {
    *required = SM2_RAW_SIG_LEN;
    pk.sm2_sign_digest_raw(MdType::SM3, digest, sig, rng)
}

pub(crate) fn check_ecdsa_sign_output(
    pk: &Pk,
    output_len: usize,
    required: &mut usize,
) -> TeeResult {
    match pk.pk_type() {
        Type::Eckey | Type::Ecdsa => {}
        _ => return Err(TEE_ERROR_BAD_PARAMETERS),
    }
    let part_len = pk
        .ecdsa_raw_signature_part_len()
        .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
    let sig_len = part_len * 2;
    *required = sig_len;
    if output_len < sig_len {
        return Err(TEE_ERROR_SHORT_BUFFER);
    }
    Ok(())
}

/// GP/OP-TEE ECDSA verify: fixed-width R||S (2 × curve byte length), not ASN.1 DER.
pub(crate) fn ecdsa_verify_raw(pk: &mut Pk, hash: &[u8], sig: &[u8]) -> Result<(), MbedError> {
    pk.ecdsa_verify_raw(hash, sig)
}

/// GP/OP-TEE ECDSA sign: fixed-width R||S with each component right-aligned in its half.
pub(crate) fn ecdsa_sign_raw<F: RngCallback>(
    pk: &mut Pk,
    hash: &[u8],
    sig: &mut [u8],
    required: &mut usize,
    rng: &mut F,
) -> Result<usize, MbedError> {
    let len = pk.ecdsa_sign_raw(hash, sig, rng)?;
    *required = len;
    Ok(len)
}

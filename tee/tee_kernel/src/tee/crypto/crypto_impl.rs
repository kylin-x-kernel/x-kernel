// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

// for source:
// 	- core/include/crypto/crypto.h
use core::marker::PhantomData;

use tee_crypto::{
    bignum::TeeBigNum,
    tee_ops::{ecc, ecc::EccCurve, rsa as rsa_ops},
};
use tee_raw_sys::{
    TEE_ECC_CURVE_NIST_P192, TEE_ECC_CURVE_NIST_P224, TEE_ECC_CURVE_NIST_P256,
    TEE_ECC_CURVE_NIST_P384, TEE_ECC_CURVE_NIST_P521, TEE_ECC_CURVE_SM2, TEE_ERROR_BAD_PARAMETERS,
    TEE_ERROR_NOT_SUPPORTED,
};

use crate::tee::{
    TeeResult,
    crypto::{bignum::BigNum, crypto::EccKeypair},
    rng_software::GlobalSoftwareRng,
};

/// GP: `crypto_ecc_keypair_ops_generate`
pub trait CryptoEccKeypairOpsGenerate {
    fn generate(&mut self, key_size_bits: usize) -> TeeResult;
}

pub trait EccKeyPairCanGenerate {}

pub struct EccComKeyPair;
pub struct Sm2PkeKeyPair;
pub struct Sm2DsaKeyPair;
pub struct Sm2KepKeyPair;

impl EccKeyPairCanGenerate for EccComKeyPair {}
impl EccKeyPairCanGenerate for Sm2DsaKeyPair {}
impl EccKeyPairCanGenerate for Sm2KepKeyPair {}
impl EccKeyPairCanGenerate for Sm2PkeKeyPair {}

/// GP: `struct ecc_keypair` (typed ops context for key generation)
pub struct EccKeypairOpsCtx<'a, A> {
    pub inner: &'a mut EccKeypair,
    pub _marker: PhantomData<A>,
}

impl<'a, A> EccKeypairOpsCtx<'a, A> {
    pub fn new(inner: &'a mut EccKeypair) -> Self {
        Self {
            inner,
            _marker: PhantomData,
        }
    }
}

fn ecc_get_keysize(
    curve: u32,
    key_size_bytes: &mut usize,
    key_size_bits: &mut usize,
) -> TeeResult<()> {
    match curve {
        TEE_ECC_CURVE_NIST_P192 => {
            *key_size_bits = 192;
            *key_size_bytes = 24;
        }
        TEE_ECC_CURVE_NIST_P224 => {
            *key_size_bits = 224;
            *key_size_bytes = 28;
        }
        TEE_ECC_CURVE_NIST_P256 => {
            *key_size_bits = 256;
            *key_size_bytes = 32;
        }
        TEE_ECC_CURVE_NIST_P384 => {
            *key_size_bits = 384;
            *key_size_bytes = 48;
        }
        TEE_ECC_CURVE_NIST_P521 => {
            *key_size_bits = 521;
            *key_size_bytes = 66;
        }
        TEE_ECC_CURVE_SM2 => {
            *key_size_bits = 256;
            *key_size_bytes = 32;
        }
        _ => {
            *key_size_bits = 0;
            *key_size_bytes = 0;
            return Err(TEE_ERROR_NOT_SUPPORTED);
        }
    }
    Ok(())
}

fn tee_curve_to_ecc_curve(curve: u32) -> TeeResult<EccCurve> {
    match curve {
        TEE_ECC_CURVE_NIST_P192 => Ok(EccCurve::P192),
        TEE_ECC_CURVE_NIST_P224 => Ok(EccCurve::P224),
        TEE_ECC_CURVE_NIST_P256 => Ok(EccCurve::P256),
        TEE_ECC_CURVE_NIST_P384 => Ok(EccCurve::P384),
        TEE_ECC_CURVE_NIST_P521 => Ok(EccCurve::P521),
        TEE_ECC_CURVE_SM2 => Ok(EccCurve::Sm2),
        _ => Err(TEE_ERROR_NOT_SUPPORTED),
    }
}

fn populate_ecc_keypair(key: &mut EccKeypair, kp: ecc::EccKeypairBytes) -> TeeResult {
    key.d = BigNum(
        TeeBigNum::from_bytes(kp.private_key.expose_secret())
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?,
    );
    key.x = BigNum(
        TeeBigNum::from_bytes(kp.public_x.as_bytes()).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?,
    );
    key.y = BigNum(
        TeeBigNum::from_bytes(kp.public_y.as_bytes()).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?,
    );
    Ok(())
}

impl<A: EccKeyPairCanGenerate> CryptoEccKeypairOpsGenerate for EccKeypairOpsCtx<'_, A> {
    fn generate(&mut self, key_size: usize) -> TeeResult {
        let mut key_size_bytes: usize = 0;
        let mut key_size_bits: usize = 0;

        ecc_get_keysize(self.inner.curve, &mut key_size_bytes, &mut key_size_bits)?;

        tee_debug!(
            "key_size: {}, key_size_bytes: {}, key_size_bits: {}",
            key_size,
            key_size_bytes,
            key_size_bits
        );

        if key_size != key_size_bits {
            return Err(TEE_ERROR_BAD_PARAMETERS);
        }

        let curve = tee_curve_to_ecc_curve(self.inner.curve)?;
        let mut rng = GlobalSoftwareRng;
        let kp = ecc::ecc_keygen_bytes(curve, &mut rng).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        populate_ecc_keypair(self.inner, kp)
    }
}

fn bn_from_bytes(bytes: &[u8]) -> TeeResult<BigNum> {
    BigNum::from_bytes(bytes)
}

pub fn crypto_acipher_gen_rsa_key(
    key: &mut crate::tee::crypto::crypto::RsaKeypair,
    key_size: usize,
) -> TeeResult {
    let e_bytes = key.e.to_bytes().map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
    let mut e_buf = [0u8; 4];
    let offset = 4 - e_bytes.len().min(4);
    e_buf[offset..].copy_from_slice(&e_bytes[..e_bytes.len().min(4)]);
    let e = u32::from_be_bytes(e_buf);

    let mut rng = GlobalSoftwareRng;
    let rsa_key =
        rsa_ops::rsa_keygen(&mut rng, key_size, e).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;

    let n = rsa_ops::rsa_get_n(&rsa_key);
    let e_out = rsa_ops::rsa_get_e(&rsa_key);
    let d = rsa_ops::rsa_get_d(&rsa_key);
    let primes = rsa_ops::rsa_get_primes(&rsa_key);
    let dp = rsa_ops::rsa_get_dp(&rsa_key);
    let dq = rsa_ops::rsa_get_dq(&rsa_key);
    let qi = rsa_ops::rsa_get_qinv(&rsa_key);

    if primes.len() < 2 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    key.e = bn_from_bytes(&e_out)?;
    key.d = bn_from_bytes(d.expose_secret())?;
    key.n = bn_from_bytes(&n)?;
    key.p = bn_from_bytes(primes[0].expose_secret())?;
    key.q = bn_from_bytes(primes[1].expose_secret())?;
    key.dp = bn_from_bytes(dp.expose_secret())?;
    key.dq = bn_from_bytes(dq.expose_secret())?;
    key.qp = bn_from_bytes(qi.expose_secret())?;

    Ok(())
}

#[unittest::mod_test]
pub mod tests_tee_crypto_impl {
    use unittest::assert;

    use super::*;

    #[unittest::def_test]
    fn test_crypto_ecc_keypair_ops_generate() {
        let mut keypair = EccKeypair {
            curve: TEE_ECC_CURVE_SM2,
            ..Default::default()
        };
        let key_size = 256;
        let result = EccKeypairOpsCtx::<Sm2DsaKeyPair>::new(&mut keypair).generate(key_size);
        info!("Generated ECC key: {:X?}", result);
        assert!(result.is_ok());

        let d = keypair.d.to_bytes().expect("d to_bytes");
        let x = keypair.x.to_bytes().expect("x to_bytes");
        let y = keypair.y.to_bytes().expect("y to_bytes");
        assert!(!d.is_empty());
        assert!(!x.is_empty());
        assert!(!y.is_empty());
    }
}

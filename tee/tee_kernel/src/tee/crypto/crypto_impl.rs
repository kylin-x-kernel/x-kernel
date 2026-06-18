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
    crypto::{
        bignum::BigNum,
        crypto::{ecc_keypair, ecc_public_key},
    },
    rng_software::GlobalSoftwareRng,
};

pub trait crypto_ecc_keypair_ops {
    fn generate(&mut self, key_size_bits: usize) -> TeeResult<()>;
    fn sign(&mut self, algo: u32, msg: &[u8], sig: &mut [u8], sig_len: &mut usize)
    -> TeeResult<()>;
    fn shared_secret(
        &mut self,
        public_key: &mut ecc_public_key,
        secret: &mut [u8],
        secret_len: &mut usize,
    ) -> TeeResult<()>;
    fn decrypt(&mut self, src: &[u8], dst: &mut [u8], dst_len: &mut usize) -> TeeResult<()>;
}

/// traits for ecc keypair operations, using crypto_ecc_keypair_ops
pub trait crypto_ecc_keypair_ops_generate {
    fn generate(&mut self, key_size_bits: usize) -> TeeResult;
}

pub trait crypto_ecc_keypair_ops_sign {
    fn sign(&mut self, algo: u32, msg: &[u8], sig: &mut [u8], sig_len: &mut usize);
}

pub trait crypto_ecc_keypair_ops_sign_impl {
    fn sign_impl(key: &mut ecc_keypair, algo: u32, msg: &[u8], sig: &mut [u8], sig_len: &mut usize);
}

pub trait crypto_ecc_keypair_ops_shared_secret {
    fn shared_secret(
        &mut self,
        public_key: &mut ecc_public_key,
        secret: &mut [u8],
        secret_len: &mut usize,
    ) -> TeeResult<()>;
}

pub trait crypto_ecc_keypair_ops_decrypt {
    fn decrypt(&mut self, src: &[u8], dst: &mut [u8], dst_len: &mut usize) -> TeeResult<()>;
}

/// traits for ecc keypair abilities
pub trait EccKeyPairCanGenerate {}

pub trait EccKeyPairCanSign {}

pub trait EccKeyPairCanSharedSecret {}

pub trait EccKeyPairCanDecrypt {}

pub enum EccAlgoKeyPair {
    EccCom,
    Sm2Pke,
    Sm2Dsa,
    Sm2Kep,
}

pub struct EccComKeyPair;
pub struct Sm2PkeKeyPair;
pub struct Sm2DsaKeyPair;
pub struct Sm2KepKeyPair;

/// Ecc Common Key Pair Operations
/// - Generate
/// - Sign
/// - Shared Secret
impl EccKeyPairCanGenerate for EccComKeyPair {}
impl EccKeyPairCanSign for EccComKeyPair {}
impl EccKeyPairCanSharedSecret for EccComKeyPair {}

impl crypto_ecc_keypair_ops_sign_impl for EccComKeyPair {
    fn sign_impl(
        _key: &mut ecc_keypair,
        _algo: u32,
        _msg: &[u8],
        _sig: &mut [u8],
        _sig_len: &mut usize,
    ) {
        todo!()
    }
}

/// Sm2 Dsa Key Pair Operations
/// - Generate
/// - Sign
impl EccKeyPairCanGenerate for Sm2DsaKeyPair {}
impl EccKeyPairCanSign for Sm2DsaKeyPair {}

impl crypto_ecc_keypair_ops_sign_impl for Sm2DsaKeyPair {
    fn sign_impl(
        _key: &mut ecc_keypair,
        _algo: u32,
        _msg: &[u8],
        _sig: &mut [u8],
        _sig_len: &mut usize,
    ) {
        todo!()
    }
}

/// Sm2 Kep Key Pair Operations
/// - Generate
impl EccKeyPairCanGenerate for Sm2KepKeyPair {}

/// Sm2 Pke Key Pair Operations
/// - Generate
/// - Decrypt
impl EccKeyPairCanGenerate for Sm2PkeKeyPair {}

impl EccKeyPairCanDecrypt for Sm2PkeKeyPair {}

/// Ecc Key Pair
///
/// inner: ecc_keypair, the data for ecc keypair
/// _marker: PhantomData<A>, A implements the ecc keypair operations
pub struct EccKeypair<'a, A> {
    pub inner: &'a mut ecc_keypair,
    pub _marker: PhantomData<A>,
}

impl<'a, A> EccKeypair<'a, A> {
    /// constructor, pass a mutable reference of ecc_keypair
    pub fn new(inner: &'a mut ecc_keypair) -> Self {
        Self {
            inner,
            _marker: PhantomData,
        }
    }
}

/// Get the key size in bits and bytes for a given TEE ECC curve identifier.
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

fn populate_ecc_keypair(key: &mut ecc_keypair, kp: ecc::EccKeypairBytes) -> TeeResult {
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

impl<A: EccKeyPairCanGenerate> crypto_ecc_keypair_ops_generate for EccKeypair<'_, A> {
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

impl<A> crypto_ecc_keypair_ops_sign for EccKeypair<'_, A>
where
    A: EccKeyPairCanSign + crypto_ecc_keypair_ops_sign_impl,
{
    fn sign(&mut self, algo: u32, msg: &[u8], sig: &mut [u8], sig_len: &mut usize) {
        A::sign_impl(self.inner, algo, msg, sig, sig_len);
    }
}

impl<A: EccKeyPairCanSharedSecret> crypto_ecc_keypair_ops_shared_secret for EccKeypair<'_, A> {
    fn shared_secret(
        &mut self,
        _public_key: &mut ecc_public_key,
        _secret: &mut [u8],
        _secret_len: &mut usize,
    ) -> TeeResult<()> {
        todo!()
    }
}

impl<A: EccKeyPairCanDecrypt> crypto_ecc_keypair_ops_decrypt for EccKeypair<'_, A> {
    fn decrypt(&mut self, _src: &[u8], _dst: &mut [u8], _dst_len: &mut usize) -> TeeResult<()> {
        todo!()
    }
}

pub trait crypto_ecc_public_ops_free {
    fn free(&mut self) -> TeeResult;
}

pub struct Sm2DsaPubKey;
pub struct Sm2PkePubKey;

pub trait EccPublicKeyCanFree {}

impl EccPublicKeyCanFree for Sm2DsaPubKey {}

impl EccPublicKeyCanFree for Sm2PkePubKey {}

pub struct EccPublicKey<A> {
    inner: ecc_public_key,
    _marker: PhantomData<A>,
}

impl<A: EccPublicKeyCanFree> crypto_ecc_public_ops_free for EccPublicKey<A> {
    fn free(&mut self) -> TeeResult {
        todo!()
    }
}

fn bn_from_bytes(bytes: &[u8]) -> TeeResult<BigNum> {
    BigNum::from_bytes(bytes)
}

pub fn crypto_acipher_gen_rsa_key(
    key: &mut crate::tee::crypto::crypto::rsa_keypair,
    key_size: usize,
) -> TeeResult {
    let e_bytes = key.e.to_bytes().map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
    // Pad to 4 bytes for u32 conversion (BigNum strips leading zeros)
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
        let mut keypair = ecc_keypair {
            curve: TEE_ECC_CURVE_SM2,
            ..Default::default()
        };
        let key_size = 256;
        let result = EccKeypair::<Sm2DsaKeyPair>::new(&mut keypair).generate(key_size);
        info!("Generated ECC key: {:X?}", result);
        assert!(result.is_ok());

        // Verify that d, x, y were populated and can be serialized
        let d = keypair.d.to_bytes().expect("d to_bytes");
        let x = keypair.x.to_bytes().expect("x to_bytes");
        let y = keypair.y.to_bytes().expect("y to_bytes");
        assert!(!d.is_empty());
        assert!(!x.is_empty());
        assert!(!y.is_empty());
    }
}

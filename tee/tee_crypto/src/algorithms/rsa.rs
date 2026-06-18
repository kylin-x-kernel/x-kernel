// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RSA implementation wrapping the `rsa` crate.
//!
//! Provides key generation, sign/verify (PKCS#1 v1.5), and
//! encrypt/decrypt (OAEP) using SHA-256 as the default hash.

use digest::Digest;

use crate::{
    asymmetric::{
        Decryptor, Encryptor, Keypair, PublicKeyComponents, RsaPublicComponents, Signer, Verifier,
    },
    bytes::PlaintextBytes,
    error::{BackendError, CryptoError, Result},
    material::{
        CiphertextAlgorithm, CiphertextBytes, SignatureAlgorithm, SignatureBytes, SignatureEncoding,
    },
    rng::CryptoRng,
};

/// RSA private key wrapper.
pub struct RsaKeypair {
    inner: rsa::RsaPrivateKey,
}

/// RSA public key wrapper.
pub struct RsaPublic {
    inner: rsa::RsaPublicKey,
}

impl RsaKeypair {
    /// Wrap an existing `rsa::RsaPrivateKey`.
    pub(crate) fn from_private_key(key: rsa::RsaPrivateKey) -> Self {
        Self { inner: key }
    }

    /// Access the inner key.
    pub(crate) fn as_inner(&self) -> &rsa::RsaPrivateKey {
        &self.inner
    }

    /// Derive the corresponding public key.
    pub fn to_public_key(&self) -> RsaPublic {
        RsaPublic {
            inner: self.inner.to_public_key(),
        }
    }

    /// Encode this key as PKCS#1 DER.
    pub fn to_pkcs1_der(&self) -> Result<pkcs8::SecretDocument> {
        use pkcs1::EncodeRsaPrivateKey;
        self.inner
            .to_pkcs1_der()
            .map_err(|_| CryptoError::Backend(BackendError::RsaParseKey))
    }

    /// Encode this key as PKCS#8 DER.
    pub fn to_pkcs8_der(&self) -> Result<pkcs8::SecretDocument> {
        use pkcs8::EncodePrivateKey;
        self.inner
            .to_pkcs8_der()
            .map_err(|_| CryptoError::Backend(BackendError::RsaParseKey))
    }
}

impl RsaPublic {
    /// Wrap an existing `rsa::RsaPublicKey`.
    pub(crate) fn from_public_key(key: rsa::RsaPublicKey) -> Self {
        Self { inner: key }
    }

    /// Access the inner key.
    pub(crate) fn as_inner(&self) -> &rsa::RsaPublicKey {
        &self.inner
    }
}

impl Keypair for RsaKeypair {
    fn generate(rng: &mut dyn CryptoRng, key_size_bits: usize) -> Result<Self> {
        let private_key = rsa::RsaPrivateKey::new(rng, key_size_bits)
            .map_err(|_| CryptoError::Backend(BackendError::RsaKeygen))?;
        Ok(Self { inner: private_key })
    }

    fn to_public_components(&self) -> Result<PublicKeyComponents> {
        use rsa::traits::PublicKeyParts;
        let n_bytes = self.inner.n().to_be_bytes();
        let e_bytes = self.inner.e().to_be_bytes();
        Ok(PublicKeyComponents::Rsa(
            RsaPublicComponents::from_be_bytes(
                n_bytes.as_ref().to_vec(),
                e_bytes.as_ref().to_vec(),
            ),
        ))
    }
}

impl Signer for RsaKeypair {
    fn sign(&self, msg: &[u8], rng: &mut dyn CryptoRng) -> Result<SignatureBytes> {
        let scheme = rsa::pkcs1v15::Pkcs1v15Sign::new::<sha2::Sha256>();
        let digest = sha2::Sha256::digest(msg);
        let sig = self
            .inner
            .sign_with_rng(rng, scheme, &digest)
            .map_err(|_| CryptoError::Backend(BackendError::RsaSign))?;
        Ok(SignatureBytes::new(
            sig,
            SignatureAlgorithm::RsaPkcs1v15,
            SignatureEncoding::Raw,
        ))
    }
}

impl Verifier for RsaPublic {
    fn verify(&self, msg: &[u8], signature: &SignatureBytes) -> Result<()> {
        if signature.algorithm() != SignatureAlgorithm::RsaPkcs1v15 {
            return Err(CryptoError::InvalidInput);
        }
        if signature.encoding() != SignatureEncoding::Raw {
            return Err(CryptoError::InvalidInput);
        }
        let scheme = rsa::pkcs1v15::Pkcs1v15Sign::new::<sha2::Sha256>();
        let digest = sha2::Sha256::digest(msg);
        self.inner
            .verify(scheme, &digest, signature.as_bytes())
            .map_err(|_| CryptoError::VerificationFailed)
    }
}

impl Encryptor for RsaPublic {
    fn encrypt(&self, msg: &[u8], rng: &mut dyn CryptoRng) -> Result<CiphertextBytes> {
        let padding = rsa::oaep::Oaep::<sha2::Sha256>::new();
        let ciphertext = self
            .inner
            .encrypt(rng, padding, msg)
            .map_err(|_| CryptoError::Backend(BackendError::RsaEncrypt))?;
        Ok(CiphertextBytes::new(
            ciphertext,
            CiphertextAlgorithm::RsaOaep,
        ))
    }
}

impl Decryptor for RsaKeypair {
    fn decrypt(&self, ciphertext: &CiphertextBytes) -> Result<PlaintextBytes> {
        if ciphertext.algorithm() != CiphertextAlgorithm::RsaOaep {
            return Err(CryptoError::InvalidInput);
        }
        let padding = rsa::oaep::Oaep::<sha2::Sha256>::new();
        let plaintext = self
            .inner
            .decrypt(padding, ciphertext.as_bytes())
            .map_err(|_| CryptoError::Backend(BackendError::RsaDecrypt))?;
        Ok(PlaintextBytes::new(plaintext))
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Error types for tee_crypto.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptoError {
    InvalidKey,
    InvalidInput,
    InvalidLength,
    AlgorithmMismatch,
    EncodingMismatch,
    InvalidDigestAlgorithm,
    InvalidSignatureEncoding,
    InvalidCiphertextAlgorithm,
    BufferTooSmall,
    DivideByZero,
    InvalidModulus,
    InvalidExponent,
    ArithmeticOverflow,
    UnsupportedAlgorithm,
    VerificationFailed,
    Backend(BackendError),
    InternalError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendError {
    InvalidEncoding,
    InvalidExponent,
    RsaKeygen,
    RsaKeyConstruction,
    RsaPublicKey,
    RsaParseKey,
    RsaSign,
    RsaEncrypt,
    RsaDecrypt,
    RsaRawPrivate,
    RsaRawPublic,
}

impl core::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CryptoError::InvalidKey => write!(f, "invalid key"),
            CryptoError::InvalidInput => write!(f, "invalid input"),
            CryptoError::InvalidLength => write!(f, "invalid length"),
            CryptoError::AlgorithmMismatch => write!(f, "algorithm mismatch"),
            CryptoError::EncodingMismatch => write!(f, "encoding mismatch"),
            CryptoError::InvalidDigestAlgorithm => write!(f, "invalid digest algorithm"),
            CryptoError::InvalidSignatureEncoding => write!(f, "invalid signature encoding"),
            CryptoError::InvalidCiphertextAlgorithm => write!(f, "invalid ciphertext algorithm"),
            CryptoError::BufferTooSmall => write!(f, "buffer too small"),
            CryptoError::DivideByZero => write!(f, "divide by zero"),
            CryptoError::InvalidModulus => write!(f, "invalid modulus"),
            CryptoError::InvalidExponent => write!(f, "invalid exponent"),
            CryptoError::ArithmeticOverflow => write!(f, "arithmetic overflow"),
            CryptoError::UnsupportedAlgorithm => write!(f, "unsupported algorithm"),
            CryptoError::VerificationFailed => write!(f, "verification failed"),
            CryptoError::Backend(err) => write!(f, "backend error: {}", err),
            CryptoError::InternalError => write!(f, "internal error"),
        }
    }
}

impl core::fmt::Display for BackendError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BackendError::InvalidEncoding => write!(f, "invalid encoding"),
            BackendError::InvalidExponent => write!(f, "invalid exponent"),
            BackendError::RsaKeygen => write!(f, "rsa key generation"),
            BackendError::RsaKeyConstruction => write!(f, "rsa key construction"),
            BackendError::RsaPublicKey => write!(f, "rsa public key construction"),
            BackendError::RsaParseKey => write!(f, "rsa key parsing"),
            BackendError::RsaSign => write!(f, "rsa signing"),
            BackendError::RsaEncrypt => write!(f, "rsa encryption"),
            BackendError::RsaDecrypt => write!(f, "rsa decryption"),
            BackendError::RsaRawPrivate => write!(f, "rsa raw private operation"),
            BackendError::RsaRawPublic => write!(f, "rsa raw public operation"),
        }
    }
}

pub type Result<T> = core::result::Result<T, CryptoError>;

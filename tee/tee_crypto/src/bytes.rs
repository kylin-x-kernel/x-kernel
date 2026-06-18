// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Semantic byte containers for cryptographic material.

use alloc::vec::Vec;
use core::{fmt, ops::Deref};

use zeroize::Zeroizing;

#[derive(Clone, Default, Eq, PartialEq)]
pub struct PublicBytes(Vec<u8>);

impl PublicBytes {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }
}

impl AsRef<[u8]> for PublicBytes {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl Deref for PublicBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl fmt::Debug for PublicBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("PublicBytes").field(&self.0.len()).finish()
    }
}

impl From<Vec<u8>> for PublicBytes {
    fn from(value: Vec<u8>) -> Self {
        Self::new(value)
    }
}

impl From<PublicBytes> for Vec<u8> {
    fn from(value: PublicBytes) -> Self {
        value.into_vec()
    }
}

#[derive(Clone, Default, Eq, PartialEq)]
pub struct BigEndianBytes(PublicBytes);

impl BigEndianBytes {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(PublicBytes::new(bytes))
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.0.into_vec()
    }
}

impl AsRef<[u8]> for BigEndianBytes {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl Deref for BigEndianBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl fmt::Debug for BigEndianBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("BigEndianBytes")
            .field(&self.as_bytes().len())
            .finish()
    }
}

impl From<Vec<u8>> for BigEndianBytes {
    fn from(value: Vec<u8>) -> Self {
        Self::new(value)
    }
}

impl From<BigEndianBytes> for Vec<u8> {
    fn from(value: BigEndianBytes) -> Self {
        value.into_vec()
    }
}

#[derive(Clone, Default, Eq, PartialEq)]
pub struct SecretBytes(Zeroizing<Vec<u8>>);

impl SecretBytes {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(Zeroizing::new(bytes))
    }

    pub fn expose_secret(&self) -> &[u8] {
        self.0.as_slice()
    }

    pub fn expose_secret_clone(&self) -> Vec<u8> {
        self.0.to_vec()
    }
}

impl fmt::Debug for SecretBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SecretBytes")
            .field(&self.expose_secret().len())
            .finish()
    }
}

impl From<Vec<u8>> for SecretBytes {
    fn from(value: Vec<u8>) -> Self {
        Self::new(value)
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct PlaintextBytes(SecretBytes);

impl PlaintextBytes {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(SecretBytes::new(bytes))
    }

    pub fn expose_secret(&self) -> &[u8] {
        self.0.expose_secret()
    }

    pub fn expose_secret_clone(&self) -> Vec<u8> {
        self.0.expose_secret_clone()
    }
}

impl fmt::Debug for PlaintextBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("PlaintextBytes")
            .field(&self.expose_secret().len())
            .finish()
    }
}

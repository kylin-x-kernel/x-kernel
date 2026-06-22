// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! MD5 hash wrapper backed by RustCrypto.
//!
//! MD5 is provided for GlobalPlatform TEE API compatibility (`TEE_ALG_MD5`,
//! `TEE_ALG_HMAC_MD5`, RSA/ECDSA variants, etc.). It is cryptographically
//! broken (practical collision attacks) and must not be used for new
//! security-sensitive workloads; prefer SHA-256 or SM3 instead.

use digest::Digest as _;

use super::hash::{Digest, DigestBytes, HashAlgorithm};

/// MD5 hash (RFC 1321).
///
/// Supported for legacy TEE interoperability only; not recommended for new use.
#[derive(Clone)]
pub struct Md5 {
    inner: md5::Md5,
}

impl Digest for Md5 {
    fn new() -> Self {
        Self {
            inner: md5::Md5::new(),
        }
    }

    fn update(&mut self, data: &[u8]) {
        self.inner.update(data);
    }

    fn finalize(self) -> DigestBytes {
        DigestBytes::new(self.inner.finalize().to_vec(), Self::algorithm())
    }

    fn algorithm() -> HashAlgorithm {
        HashAlgorithm::Md5
    }

    fn output_size() -> usize {
        16
    }

    fn name() -> &'static str {
        "MD5"
    }
}

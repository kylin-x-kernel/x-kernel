// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Key derivation functions (PBKDF2, Concat KDF). HKDF lives in [`crate::hkdf`].

pub use crate::algorithms::kdf::{
    concat_kdf, concat_kdf_digest, pbkdf2_hmac_sha1, pbkdf2_hmac_sm3,
};

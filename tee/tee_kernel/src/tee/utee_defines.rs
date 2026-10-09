// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

pub(super) type TeeAlg = u32;
pub(super) const HW_UNIQUE_KEY_LENGTH: usize = 16;

pub(super) const TEE_MD5_HASH_SIZE: usize = 16;
pub(super) const TEE_SHA1_HASH_SIZE: usize = 20;
pub(super) const TEE_SHA224_HASH_SIZE: usize = 28;
pub(super) const TEE_SHA256_HASH_SIZE: usize = 32;
pub(super) const TEE_SM3_HASH_SIZE: usize = 32;
pub(super) const TEE_SHA384_HASH_SIZE: usize = 48;
pub(super) const TEE_SHA512_HASH_SIZE: usize = 64;

pub(super) const TEE_AES_BLOCK_SIZE: usize = 16;
pub(super) const TEE_DES_BLOCK_SIZE: usize = 8;

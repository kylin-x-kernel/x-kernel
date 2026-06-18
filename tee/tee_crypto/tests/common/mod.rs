// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use tee_crypto::{
    hash::{Digest, DigestBytes, Sha256},
    rng::DeterministicRng,
};

pub fn seeded_rng(seed: u64) -> DeterministicRng {
    DeterministicRng::seed_from_u64(seed)
}

#[allow(dead_code)]
pub fn sha256_digest(data: &[u8]) -> DigestBytes {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize()
}

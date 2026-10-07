// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! SMCCC firmware TRNG entropy source adapter.

use super::{EntropySource, SourceAvailability, SourceReadError, SourceReadResult, SourceTier};
use crate::smccc_trng;

pub(super) struct SmcccTrngSource;

impl EntropySource for SmcccTrngSource {
    fn name(&self) -> &'static str {
        "smccc-trng"
    }

    fn tier(&self) -> SourceTier {
        SourceTier::Hrng
    }

    fn init(&self) {
        smccc_trng::init();
    }

    fn availability(&self) -> SourceAvailability {
        if smccc_trng::is_available() {
            SourceAvailability::Available
        } else {
            SourceAvailability::Unavailable
        }
    }

    fn read(&self, len: usize) -> SourceReadResult {
        if len == 0 {
            return Err(SourceReadError::EmptyRequest);
        }
        if !self.availability().is_available() {
            return Err(SourceReadError::Unavailable);
        }
        smccc_trng::read(len).map_err(super::map_smccc_trng_error)
    }
}

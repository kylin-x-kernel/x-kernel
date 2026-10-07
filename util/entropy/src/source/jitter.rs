// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Software jitter entropy source adapter.

use super::{EntropySource, SourceAvailability, SourceReadResult, SourceTier};
use crate::jitter;

pub(super) struct JitterSource;

impl EntropySource for JitterSource {
    fn name(&self) -> &'static str {
        "jitter"
    }

    fn tier(&self) -> SourceTier {
        SourceTier::Software
    }

    fn init(&self) {
        jitter::init();
    }

    fn availability(&self) -> SourceAvailability {
        SourceAvailability::Available
    }

    fn read(&self, len: usize) -> SourceReadResult {
        super::read_optional_source(self.availability(), len, jitter::read)
    }
}

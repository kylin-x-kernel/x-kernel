// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU instruction entropy source adapter.

use super::{EntropySource, SourceAvailability, SourceReadResult, SourceTier};
use crate::arch_cpu;

pub(super) struct ArchCpuSource;

impl EntropySource for ArchCpuSource {
    fn name(&self) -> &'static str {
        "cpu-rng"
    }

    fn tier(&self) -> SourceTier {
        SourceTier::Hrng
    }

    fn init(&self) {
        arch_cpu::init();
    }

    fn availability(&self) -> SourceAvailability {
        if arch_cpu::is_available() {
            SourceAvailability::Available
        } else {
            SourceAvailability::Unavailable
        }
    }

    fn read(&self, len: usize) -> SourceReadResult {
        super::read_optional_source(self.availability(), len, arch_cpu::read)
    }
}

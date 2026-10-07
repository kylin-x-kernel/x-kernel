// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO RNG entropy source adapter.

use super::{EntropySource, SourceAvailability, SourceReadError, SourceReadResult, SourceTier};
use crate::virtio;

pub(super) struct VirtioRngSource;

impl EntropySource for VirtioRngSource {
    fn name(&self) -> &'static str {
        "virtio-rng"
    }

    fn tier(&self) -> SourceTier {
        SourceTier::HostTrusted
    }

    fn init(&self) {
        virtio::register_sources();
    }

    fn availability(&self) -> SourceAvailability {
        if virtio::is_present() {
            SourceAvailability::Available
        } else if virtio::is_disabled() {
            SourceAvailability::Failed
        } else {
            SourceAvailability::TemporarilyUnavailable
        }
    }

    fn read(&self, len: usize) -> SourceReadResult {
        if len == 0 {
            return Err(SourceReadError::EmptyRequest);
        }
        if !virtio::is_present() {
            return Err(SourceReadError::Unavailable);
        }
        virtio::request_entropy();
        // The collector mixes completed bytes directly; keep walking other sources.
        Err(SourceReadError::NoEntropy)
    }
}

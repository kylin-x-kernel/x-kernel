// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Entropy source registry and common source trait.

use alloc::vec::Vec;

use crate::{arch_cpu, jitter, smccc_trng, virtio};

/// Trust / quality tier for an entropy source.
///
/// Used in Step 2 to filter sources during multi-source reseed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceTier {
    /// CPU instructions or on-die / SoC HRNG accessed from the kernel.
    Hrng,
    /// VMM-provided entropy (VirtIO RNG); only trusted when the host is trusted.
    HostTrusted,
    /// Software-collected entropy such as timer / interrupt jitter.
    Software,
}

/// A registered hardware or software entropy source.
trait EntropySource: Sync {
    /// Stable identifier used in logs and future observability hooks.
    fn name(&self) -> &'static str;

    /// Trust tier used to decide inclusion during reseed.
    fn tier(&self) -> SourceTier;

    /// Probe or register the source during [`init_all`].
    fn init(&self) {}

    /// Returns whether this source is enabled and currently usable.
    fn is_available(&self) -> bool;

    /// Read up to `len` bytes from the source.
    fn read(&self, len: usize) -> Option<Vec<u8>>;
}

struct ArchCpuSource;

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

    fn is_available(&self) -> bool {
        arch_cpu::is_available()
    }

    fn read(&self, len: usize) -> Option<Vec<u8>> {
        arch_cpu::read(len)
    }
}

struct SmcccTrngSource;

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

    fn is_available(&self) -> bool {
        smccc_trng::is_available()
    }

    fn read(&self, len: usize) -> Option<Vec<u8>> {
        smccc_trng::read(len)
    }
}

struct VirtioRngSource;

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

    fn is_available(&self) -> bool {
        virtio::is_present()
    }

    fn read(&self, len: usize) -> Option<Vec<u8>> {
        virtio::read(len)
    }
}

struct JitterSource;

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

    fn is_available(&self) -> bool {
        jitter::is_available()
    }

    fn read(&self, len: usize) -> Option<Vec<u8>> {
        jitter::read(len)
    }
}

static ARCH_CPU_SOURCE: ArchCpuSource = ArchCpuSource;
static SMCCC_TRNG_SOURCE: SmcccTrngSource = SmcccTrngSource;
static VIRTIO_RNG_SOURCE: VirtioRngSource = VirtioRngSource;
static JITTER_SOURCE: JitterSource = JitterSource;

static SOURCES: [&dyn EntropySource; 4] = [
    &ARCH_CPU_SOURCE,
    &SMCCC_TRNG_SOURCE,
    &VIRTIO_RNG_SOURCE,
    &JITTER_SOURCE,
];

/// Registered entropy sources in probe / mix priority order.
fn sources() -> &'static [&'static dyn EntropySource] {
    &SOURCES
}

/// Probe every registered source.
pub(crate) fn init_all() {
    for source in sources() {
        source.init();
    }
}

/// Returns whether any registered source is currently available.
pub(crate) fn any_available() -> bool {
    sources().iter().any(|source| include_in_reseed(*source))
}

/// Comma-separated list of sources that participate in reseed.
pub(crate) fn available_summary() -> alloc::string::String {
    let mut names = alloc::vec::Vec::new();
    for source in sources() {
        if include_in_reseed(*source) {
            names.push(source.name());
        }
    }

    if names.is_empty() {
        if sources().iter().any(|source| source.is_available()) {
            return "no trusted hardware sources".into();
        }
        return "no hardware sources".into();
    }

    names.join(", ")
}

/// Read from every eager-eligible source that succeeds.
///
/// Eager sources exclude [`SourceTier::HostTrusted`] (VirtIO) so boot does not
/// block in a pre-interrupt virtqueue poll.
pub(crate) fn read_all_eager(len: usize) -> Vec<SourceSample> {
    let mut samples = Vec::new();

    for source in sources() {
        if !include_in_eager_reseed(*source) {
            continue;
        }
        if let Some(data) = source.read(len) {
            samples.push(SourceSample {
                name: source.name(),
                data,
            });
        }
    }

    samples
}

/// Read from every available source that succeeds.
pub(crate) fn read_all_available(len: usize) -> Vec<SourceSample> {
    let mut samples = Vec::new();

    for source in sources() {
        if !include_in_reseed(*source) {
            continue;
        }
        if let Some(data) = source.read(len) {
            samples.push(SourceSample {
                name: source.name(),
                data,
            });
        }
    }

    samples
}

fn include_in_reseed(source: &dyn EntropySource) -> bool {
    if !source.is_available() {
        return false;
    }

    match source.tier() {
        SourceTier::Hrng | SourceTier::Software => true,
        SourceTier::HostTrusted => kbuild_config::KFEAT_ENTROPY_TRUST_HOST,
    }
}

fn include_in_eager_reseed(source: &dyn EntropySource) -> bool {
    if !source.is_available() {
        return false;
    }

    matches!(source.tier(), SourceTier::Hrng | SourceTier::Software)
}

/// A buffer read from a named entropy source.
pub(crate) struct SourceSample {
    pub(crate) name: &'static str,
    pub(crate) data: Vec<u8>,
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert, assert_eq, def_test};

    use super::*;

    #[def_test]
    fn test_registered_source_count() {
        assert_eq!(sources().len(), 4);
    }

    #[def_test]
    fn test_source_names_are_unique() {
        let names: alloc::vec::Vec<_> = sources().iter().map(|s| s.name()).collect();
        for name in &names {
            assert_eq!(names.iter().filter(|other| *other == name).count(), 1);
            assert!(!name.is_empty());
        }
    }

    #[def_test]
    fn test_source_tiers() {
        assert_eq!(ARCH_CPU_SOURCE.tier(), SourceTier::Hrng);
        assert_eq!(SMCCC_TRNG_SOURCE.tier(), SourceTier::Hrng);
        assert_eq!(VIRTIO_RNG_SOURCE.tier(), SourceTier::HostTrusted);
        assert_eq!(JITTER_SOURCE.tier(), SourceTier::Software);
    }

    #[def_test]
    fn test_eager_reseed_excludes_host_trusted() {
        assert!(!include_in_eager_reseed(&VIRTIO_RNG_SOURCE));
        // Even when the HostTrusted source reports available, eager path skips it.
        if VIRTIO_RNG_SOURCE.is_available() {
            assert!(!include_in_eager_reseed(&VIRTIO_RNG_SOURCE));
        }
        if JITTER_SOURCE.is_available() {
            assert!(include_in_eager_reseed(&JITTER_SOURCE));
        }
        if ARCH_CPU_SOURCE.is_available() {
            assert!(include_in_eager_reseed(&ARCH_CPU_SOURCE));
        }
    }

    #[def_test]
    fn test_host_trusted_gated_by_trust_host() {
        if VIRTIO_RNG_SOURCE.is_available() {
            assert_eq!(
                include_in_reseed(&VIRTIO_RNG_SOURCE),
                kbuild_config::KFEAT_ENTROPY_TRUST_HOST
            );
        } else {
            assert!(!include_in_reseed(&VIRTIO_RNG_SOURCE));
        }
    }

    #[def_test]
    fn test_read_all_eager_never_returns_virtio() {
        init_all();
        let samples = read_all_eager(32);
        for sample in &samples {
            assert!(
                sample.name != "virtio-rng",
                "eager reseed must not pull virtio-rng"
            );
            assert!(!sample.data.is_empty());
        }
    }

    #[def_test]
    fn test_available_summary_non_empty_when_sources_exist() {
        init_all();
        let summary = available_summary();
        assert!(!summary.is_empty());
        if any_available() {
            assert!(summary != "no hardware sources");
            assert!(summary != "no trusted hardware sources");
        }
    }

    #[def_test]
    fn test_read_zero_len_from_sources() {
        for source in sources() {
            if !source.is_available() {
                continue;
            }
            // Zero-length requests should not produce samples.
            assert!(source.read(0).is_none());
        }
    }
}

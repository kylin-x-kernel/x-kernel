// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Entropy source registry and common source trait.

use alloc::vec::Vec;

#[cfg(feature = "entropy_arch_cpu")]
mod arch_cpu;
#[cfg(feature = "entropy_arch_cpu")]
use self::arch_cpu::ArchCpuSource;
#[cfg(feature = "entropy_jitter")]
mod jitter;
#[cfg(feature = "entropy_jitter")]
use self::jitter::JitterSource;
#[cfg(all(feature = "entropy_smccc_trng", target_arch = "aarch64"))]
mod smccc_trng;
#[cfg(all(feature = "entropy_smccc_trng", target_arch = "aarch64"))]
use self::smccc_trng::SmcccTrngSource;
#[cfg(feature = "entropy_virtio_rng")]
mod virtio;
#[cfg(feature = "entropy_virtio_rng")]
use self::virtio::VirtioRngSource;

/// Trust / quality tier for an entropy source.
///
/// Used in Step 2 to filter sources during multi-source reseed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceTier {
    /// CPU instructions or on-die / SoC HRNG accessed from the kernel.
    #[cfg(any(
        feature = "entropy_arch_cpu",
        all(feature = "entropy_smccc_trng", target_arch = "aarch64")
    ))]
    Hrng,
    /// VMM-provided entropy (VirtIO RNG); only trusted when the host is trusted.
    #[cfg(feature = "entropy_virtio_rng")]
    HostTrusted,
    /// Software-collected entropy such as timer / interrupt jitter.
    Software,
}

/// Runtime state for a compiled-in entropy source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceAvailability {
    /// The source can currently provide entropy samples.
    Available,
    /// The source may appear later, for example after device registration.
    TemporarilyUnavailable,
    /// The platform does not expose this source.
    #[cfg(any(
        feature = "entropy_arch_cpu",
        all(feature = "entropy_smccc_trng", target_arch = "aarch64")
    ))]
    Unavailable,
    /// The source was present but has been disabled after repeated failures.
    Failed,
}

impl SourceAvailability {
    const fn is_available(self) -> bool {
        matches!(self, Self::Available)
    }
}

/// Failure reason from a source read attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceReadError {
    /// Zero-length reads are ignored by the entropy mixer.
    EmptyRequest,
    /// The source is not currently usable.
    Unavailable,
    /// The source is usable but has no entropy sample right now.
    NoEntropy,
    /// The SMCCC firmware source reported an invalid parameter or failure.
    #[cfg(all(feature = "entropy_smccc_trng", target_arch = "aarch64"))]
    Failed,
}

type SourceReadResult = Result<Vec<u8>, SourceReadError>;

/// A registered hardware or software entropy source.
trait EntropySource: Sync {
    /// Stable identifier used in logs and future observability hooks.
    fn name(&self) -> &'static str;

    /// Trust tier used to decide inclusion during reseed.
    fn tier(&self) -> SourceTier;

    /// Probe or register the source during [`init_all`].
    fn init(&self) {}

    /// Returns the source's current runtime availability.
    fn availability(&self) -> SourceAvailability;

    /// Read up to `len` bytes from the source.
    fn read(&self, len: usize) -> SourceReadResult;
}

#[cfg(feature = "entropy_arch_cpu")]
static ARCH_CPU_SOURCE: ArchCpuSource = ArchCpuSource;
#[cfg(all(feature = "entropy_smccc_trng", target_arch = "aarch64"))]
static SMCCC_TRNG_SOURCE: SmcccTrngSource = SmcccTrngSource;
#[cfg(feature = "entropy_virtio_rng")]
static VIRTIO_RNG_SOURCE: VirtioRngSource = VirtioRngSource;
#[cfg(feature = "entropy_jitter")]
static JITTER_SOURCE: JitterSource = JitterSource;

static SOURCES: &[&dyn EntropySource] = &[
    #[cfg(feature = "entropy_arch_cpu")]
    &ARCH_CPU_SOURCE,
    #[cfg(all(feature = "entropy_smccc_trng", target_arch = "aarch64"))]
    &SMCCC_TRNG_SOURCE,
    #[cfg(feature = "entropy_virtio_rng")]
    &VIRTIO_RNG_SOURCE,
    #[cfg(feature = "entropy_jitter")]
    &JITTER_SOURCE,
];

/// Registered entropy sources in probe / mix priority order.
fn sources() -> &'static [&'static dyn EntropySource] {
    SOURCES
}

fn read_optional_source(
    availability: SourceAvailability,
    len: usize,
    read: impl FnOnce(usize) -> Option<Vec<u8>>,
) -> SourceReadResult {
    if len == 0 {
        return Err(SourceReadError::EmptyRequest);
    }
    if !availability.is_available() {
        return Err(SourceReadError::Unavailable);
    }
    read(len).ok_or(SourceReadError::NoEntropy)
}

#[cfg(all(feature = "entropy_smccc_trng", target_arch = "aarch64"))]
fn map_smccc_trng_error(error: crate::smccc_trng::ReadError) -> SourceReadError {
    match error {
        crate::smccc_trng::ReadError::Unavailable => SourceReadError::Unavailable,
        crate::smccc_trng::ReadError::NoEntropy => SourceReadError::NoEntropy,
        crate::smccc_trng::ReadError::InvalidParameter | crate::smccc_trng::ReadError::Failed => {
            SourceReadError::Failed
        }
    }
}

/// Probe every registered source.
pub(crate) fn init_all() {
    for source in sources() {
        source.init();
    }
}

/// Returns whether any registered source can participate in a reseed.
pub(crate) fn any_reseed_source_available() -> bool {
    sources().iter().any(|source| include_in_reseed(*source))
}

/// Returns whether an eligible source may become available later.
pub(crate) fn any_pending_reseed_source() -> bool {
    sources().iter().any(|source| {
        source.availability() == SourceAvailability::TemporarilyUnavailable
            && source_tier_allowed_for_reseed(*source)
    })
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
        if sources()
            .iter()
            .any(|source| source.availability().is_available())
        {
            return "no trusted hardware sources".into();
        }
        if sources()
            .iter()
            .any(|source| source.availability() == SourceAvailability::TemporarilyUnavailable)
        {
            return "hardware sources not ready".into();
        }
        if sources()
            .iter()
            .any(|source| source.availability() == SourceAvailability::Failed)
        {
            return "hardware sources failed".into();
        }
        return "no hardware sources".into();
    }

    names.join(", ")
}

/// Read from every eager-eligible source that succeeds.
///
/// Eager sources exclude host-trusted sources (VirtIO) so boot does not
/// start the VirtIO collector before interrupts are ready.
pub(crate) fn read_all_eager(len: usize) -> Vec<SourceSample> {
    let mut samples = Vec::new();

    for source in sources() {
        if !include_in_eager_reseed(*source) {
            continue;
        }
        if let Ok(data) = source.read(len) {
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
        if let Ok(data) = source.read(len) {
            samples.push(SourceSample {
                name: source.name(),
                data,
            });
        }
    }

    samples
}

fn include_in_reseed(source: &dyn EntropySource) -> bool {
    if !source.availability().is_available() {
        return false;
    }

    source_tier_allowed_for_reseed(source)
}

fn source_tier_allowed_for_reseed(source: &dyn EntropySource) -> bool {
    match source.tier() {
        #[cfg(any(
            feature = "entropy_arch_cpu",
            all(feature = "entropy_smccc_trng", target_arch = "aarch64")
        ))]
        SourceTier::Hrng => true,
        SourceTier::Software => true,
        #[cfg(feature = "entropy_virtio_rng")]
        SourceTier::HostTrusted => true,
    }
}

fn include_in_eager_reseed(source: &dyn EntropySource) -> bool {
    if !source.availability().is_available() {
        return false;
    }

    match source.tier() {
        #[cfg(any(
            feature = "entropy_arch_cpu",
            all(feature = "entropy_smccc_trng", target_arch = "aarch64")
        ))]
        SourceTier::Hrng => true,
        SourceTier::Software => true,
        #[cfg(feature = "entropy_virtio_rng")]
        SourceTier::HostTrusted => false,
    }
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
        let mut expected = 0;
        #[cfg(feature = "entropy_arch_cpu")]
        {
            expected += 1;
        }
        #[cfg(all(feature = "entropy_smccc_trng", target_arch = "aarch64"))]
        {
            expected += 1;
        }
        #[cfg(feature = "entropy_virtio_rng")]
        {
            expected += 1;
        }
        #[cfg(feature = "entropy_jitter")]
        {
            expected += 1;
        }
        assert_eq!(sources().len(), expected);
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
        #[cfg(feature = "entropy_arch_cpu")]
        assert_eq!(ARCH_CPU_SOURCE.tier(), SourceTier::Hrng);
        #[cfg(all(feature = "entropy_smccc_trng", target_arch = "aarch64"))]
        assert_eq!(SMCCC_TRNG_SOURCE.tier(), SourceTier::Hrng);
        #[cfg(feature = "entropy_virtio_rng")]
        assert_eq!(VIRTIO_RNG_SOURCE.tier(), SourceTier::HostTrusted);
        #[cfg(feature = "entropy_jitter")]
        assert_eq!(JITTER_SOURCE.tier(), SourceTier::Software);
    }

    #[def_test]
    fn test_eager_reseed_excludes_host_trusted() {
        #[cfg(feature = "entropy_virtio_rng")]
        {
            assert!(!include_in_eager_reseed(&VIRTIO_RNG_SOURCE));
            // Even when the HostTrusted source reports available, eager path skips it.
            if VIRTIO_RNG_SOURCE.availability().is_available() {
                assert!(!include_in_eager_reseed(&VIRTIO_RNG_SOURCE));
            }
        }
        #[cfg(feature = "entropy_jitter")]
        if JITTER_SOURCE.availability().is_available() {
            assert!(include_in_eager_reseed(&JITTER_SOURCE));
        }
        #[cfg(feature = "entropy_arch_cpu")]
        if ARCH_CPU_SOURCE.availability().is_available() {
            assert!(include_in_eager_reseed(&ARCH_CPU_SOURCE));
        }
    }

    #[cfg(feature = "entropy_virtio_rng")]
    #[def_test]
    fn test_host_source_reseed_requires_availability() {
        assert_eq!(
            include_in_reseed(&VIRTIO_RNG_SOURCE),
            VIRTIO_RNG_SOURCE.availability().is_available()
        );
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
        if any_reseed_source_available() {
            assert!(summary != "no hardware sources");
            assert!(summary != "no trusted hardware sources");
        }
    }

    #[def_test]
    fn test_read_zero_len_from_sources() {
        for source in sources() {
            if !source.availability().is_available() {
                continue;
            }
            // Zero-length requests should not produce samples.
            assert_eq!(source.read(0).err(), Some(SourceReadError::EmptyRequest));
        }
    }
}

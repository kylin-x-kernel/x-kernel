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

unittest::path_mod_test!(tests, "../ut-tests/source.rs");

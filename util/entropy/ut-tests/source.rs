// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

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

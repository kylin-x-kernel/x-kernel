// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn unsigned_seconds_reject_values_outside_system_time() {
    assert_eq!(
        system_time_from_unsigned_seconds(i64::MAX as u64),
        Some(SystemTime::from_unix_seconds(i64::MAX))
    );
    assert_eq!(system_time_from_unsigned_seconds(u64::MAX), None);
}

#[def_test]
fn persistent_resolution_matches_each_backends_returned_quantum() {
    for (kind, expected) in [
        (RtcKind::Ls7a, TimeSpan::from_millis(1)),
        (RtcKind::Pl031, TimeSpan::from_secs(1)),
        (RtcKind::Goldfish, TimeSpan::from_secs(1)),
        (RtcKind::Cmos, TimeSpan::from_secs(1)),
    ] {
        // Exercise metadata independently of enabled hardware. This private
        // fixture is never passed to read or to a suspend session.
        let clock = RtcClock {
            config: RtcConfig::mmio_mapped(
                kind,
                VirtAddr::from_usize(0),
                RtcSource::PlatformStatic,
            ),
        };
        assert_eq!(clock.resolution(), expected);
    }
}

#[def_test]
fn persistent_binding_rejects_unmapped_or_null_transport() {
    let unmapped = RtcConfig::mmio(RtcKind::Pl031, PhysAddr::from_usize(0x1000), 0x1000, RtcSource::PlatformStatic);
    // SAFETY: unmapped configurations are rejected without register access.
    let binding = unsafe { RtcClock::from_config(unmapped) };
    assert!(binding.is_none());
    let null = RtcConfig::mmio_mapped(RtcKind::Pl031, VirtAddr::from_usize(0), RtcSource::PlatformStatic);
    // SAFETY: the null mapping is rejected without register access.
    let binding = unsafe { RtcClock::from_config(null) };
    assert!(binding.is_none());
}

#[cfg(feature = "pl031")]
#[def_test]
fn prepared_binding_preserves_identity_and_reports_driver_precision() {
    // Register-sized backing storage; the metadata test does not sample it.
    let mut registers = [0_u32; 1024];
    let config = RtcConfig::mmio_mapped(
        RtcKind::Pl031,
        VirtAddr::from_usize(registers.as_mut_ptr() as usize),
        RtcSource::PlatformStatic,
    );
    // SAFETY: aligned 4KiB storage outlives the binding, with no competing
    // accesses. Only constructor and precision metadata are exercised.
    let clock = unsafe { RtcClock::from_config(config) }.unwrap();
    assert_eq!(clock.config, config);
    assert_eq!(clock.resolution(), TimeSpan::from_secs(1));
}

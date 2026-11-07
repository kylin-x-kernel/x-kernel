// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform policy for choosing the x86 IO-APIC boot configuration.
//!
//! Only an absent firmware description may use the authorized static
//! fallback; malformed and unsupported outcomes must fail boot.

use acpi::{MadtParseError, MadtParseErrorKind};
use irq_driver::x86::{IoApicDeclaration, IoApicInitPolicy, X86IrqInitConfig};
use kerrno::KError;
use unittest::{assert_eq, def_test};

use super::{IoApicBootSource, STATIC_IO_APIC_BASE, select_io_apic_config};

#[def_test]
fn absent_uses_the_authorized_static_config() {
    let config = select_io_apic_config(IoApicBootSource::Absent).expect("static fallback");
    assert_eq!(
        config,
        X86IrqInitConfig::platform_static(
            IoApicDeclaration {
                id: 0,
                address: STATIC_IO_APIC_BASE,
                gsi_base: 0,
            },
            IoApicInitPolicy::platform_legacy(),
        )
    );
}

#[def_test]
fn malformed_stream_fails_without_fallback() {
    let error = MadtParseError {
        offset: 44,
        kind: MadtParseErrorKind::ShortEntry,
    };
    assert_eq!(
        select_io_apic_config(IoApicBootSource::Malformed(error)),
        Err(KError::InvalidInput)
    );
}

#[def_test]
fn unsupported_topology_fails_without_fallback() {
    assert_eq!(
        select_io_apic_config(IoApicBootSource::Unsupported(KError::Unsupported)),
        Err(KError::Unsupported)
    );
}

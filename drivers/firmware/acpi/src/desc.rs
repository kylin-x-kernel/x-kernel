// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Boot ACPI descriptor.
//!
//! Owns the one-shot physical RSDP address handed over by the boot firmware.
//! This is the root every static table query walks from; it holds no parsed
//! tables and applies no memory policy.

use lazyinit::LazyInit;

/// ACPI boot descriptor: the firmware-passed RSDP physical address.
///
/// All later table lookups walk from this root. Stored once by
/// [`init`] and read back through [`desc`] / [`rsdp_addr`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AcpiDesc {
    rsdp_addr: usize,
}

impl AcpiDesc {
    /// Wraps a raw RSDP physical address.
    pub const fn new(rsdp_addr: usize) -> Self {
        Self { rsdp_addr }
    }

    /// Returns the RSDP physical address.
    pub const fn rsdp_addr(self) -> usize {
        self.rsdp_addr
    }
}

/// Failure to initialize the ACPI subsystem.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AcpiInitError {
    /// `init` was called with a zero RSDP address.
    MissingRsdp,
}

static ACPI_DESC: LazyInit<AcpiDesc> = LazyInit::new();

/// Stores the boot RSDP physical address; must run before any lookup.
///
/// The first successful call wins; later calls with different addresses
/// have no effect.
///
/// # Errors
///
/// Returns [`AcpiInitError::MissingRsdp`] when `rsdp_addr` is zero.
pub fn init(rsdp_addr: usize) -> Result<(), AcpiInitError> {
    if rsdp_addr == 0 {
        return Err(AcpiInitError::MissingRsdp);
    }
    ACPI_DESC.init_once(AcpiDesc::new(rsdp_addr));
    Ok(())
}

/// Returns the boot descriptor, or `None` before [`init`].
pub fn desc() -> Option<AcpiDesc> {
    ACPI_DESC.get().copied()
}

/// Returns the boot RSDP physical address, or `None` before [`init`].
pub fn rsdp_addr() -> Option<usize> {
    desc().map(AcpiDesc::rsdp_addr)
}

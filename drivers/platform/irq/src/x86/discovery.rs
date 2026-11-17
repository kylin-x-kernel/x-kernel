// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Firmware discovery for x86 interrupt controllers.
//!
//! This module turns the ACPI MADT into a bounded, hardware-free description
//! of IO-APICs and interrupt source overrides. It does not map MMIO, program
//! registers, allocate domains, or validate pin counts; that stays with the
//! backend and the unified initialization entry.
//!
//! Discovery is intentionally tolerant of firmware quirks: unusable entries
//! are skipped and recorded in [`X86ApicDeclarations::issues`], while a
//! malformed entry fails the whole walk with
//! [`X86DiscoveryError::Malformed`]. Ordering between IO-APIC and override
//! entries does not matter, but override declarations are interpreted in
//! order because that order determines the effective ISA -> GSI table.

use acpi::{MadtEntry, MadtEntryIter, MadtParseError};
use fwnode::FwSource;
use kerrno::KResult;
use kirq::IrqTrigger;

/// Maximum number of IO-APIC controllers retained from firmware.
pub use super::MAX_IO_APICS;
/// Number of legacy ISA interrupt lines (sources 0..=15).
///
/// Because a retained override always names a distinct source in this range,
/// this is also the fixed capacity of the override result.
pub use super::MAX_LEGACY_IRQS;
use super::{IoApicDeclaration, IoApicInitPolicy, X86IrqInitConfig};
/// Upper bound on recorded discovery issues.
pub const MAX_ISSUES: usize = 64;

const NO_IO_APIC: IoApicDeclaration = IoApicDeclaration {
    id: 0,
    address: 0,
    gsi_base: 0,
};

const NO_OVERRIDE: IrqOverride = IrqOverride {
    isa_irq: 0,
    gsi: 0,
    trigger: IrqTrigger::EdgeRising,
};

/// A legacy ISA interrupt remapped by an ACPI interrupt source override.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IrqOverride {
    /// ISA interrupt line (0..=15).
    pub isa_irq: u8,
    /// Global system interrupt the line is routed to.
    pub gsi: u32,
    /// Decoded trigger and polarity.
    pub trigger: IrqTrigger,
}

/// A firmware declaration discovery could not use as-is.
///
/// Issues describe skipped or superseded declarations; they never invalidate
/// the declarations that were retained.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum X86DiscoveryIssue {
    /// IO-APIC address is zero.
    ZeroIoApicAddress { id: u8 },
    /// Another retained IO-APIC already uses this address.
    DuplicateIoApicAddress { id: u8, address: u32 },
    /// Another retained IO-APIC already uses this GSI base.
    GsiBaseConflict { id: u8, gsi_base: u32 },
    /// The fixed controller capacity was reached.
    TooManyIoApics { id: u8, address: u32 },
    /// Override names a non-ISA bus or a source outside 0..=15.
    InvalidIsaSource { bus: u8, source: u8 },
    /// Override uses the reserved polarity or trigger encoding.
    ReservedIsoFlags { bus: u8, source: u8, flags: u16 },
    /// No retained IO-APIC can own this GSI.
    UnknownGsi { isa_irq: u8, gsi: u32 },
    /// A later override replaced an earlier mapping for the same ISA line.
    OverriddenIsaMapping {
        isa_irq: u8,
        previous_gsi: u32,
        gsi: u32,
    },
}

/// Why firmware discovery could not produce a usable declaration set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum X86DiscoveryError {
    /// No ACPI MADT or no usable IO-APIC was reported.
    Absent,
    /// The MADT entry stream is structurally corrupt.
    Malformed(MadtParseError),
}

/// Bounded IO-APIC and override declarations discovered from firmware.
///
/// The result is an unvalidated topology: addresses, GSI ranges and pin
/// counts are not checked against hardware here.
#[derive(Debug, Eq, PartialEq)]
pub struct X86ApicDeclarations {
    io_apics: [IoApicDeclaration; MAX_IO_APICS],
    io_apic_count: usize,
    overrides: [IrqOverride; MAX_LEGACY_IRQS],
    override_count: usize,
    /// ISA lines whose effective route is invalidated. Linux clears the
    /// identity mapping of an override target so two lines never share one GSI
    /// through different routes; a later explicit override revives the line.
    invalid_identity: u16,
    issues: [X86DiscoveryIssue; MAX_ISSUES],
    issue_count: usize,
    issues_truncated: bool,
    source: FwSource,
}

impl X86ApicDeclarations {
    const fn new() -> Self {
        Self {
            io_apics: [NO_IO_APIC; MAX_IO_APICS],
            io_apic_count: 0,
            overrides: [NO_OVERRIDE; MAX_LEGACY_IRQS],
            override_count: 0,
            invalid_identity: 0,
            issues: [X86DiscoveryIssue::ZeroIoApicAddress { id: 0 }; MAX_ISSUES],
            issue_count: 0,
            issues_truncated: false,
            source: FwSource::Acpi,
        }
    }

    /// Retained IO-APIC controllers, in firmware order.
    pub fn io_apics(&self) -> &[IoApicDeclaration] {
        &self.io_apics[..self.io_apic_count]
    }

    /// Effective legacy overrides, in first-seen order.
    ///
    /// A later declaration for the same ISA line replaces the stored mapping
    /// in place rather than appending a second entry. An override that a later
    /// declaration invalidates is removed, because it no longer contributes an
    /// effective route.
    pub fn overrides(&self) -> &[IrqOverride] {
        &self.overrides[..self.override_count]
    }

    /// Skipped or superseded declarations, in discovery order.
    pub fn issues(&self) -> &[X86DiscoveryIssue] {
        &self.issues[..self.issue_count]
    }

    /// Whether more issues were observed than [`MAX_ISSUES`] can hold.
    pub const fn issues_truncated(&self) -> bool {
        self.issues_truncated
    }

    /// Firmware source that produced these declarations.
    pub const fn source(&self) -> FwSource {
        self.source
    }

    /// Converts the controller declaration set into the unified entry
    /// configuration.
    ///
    /// Every retained controller becomes one instance, in firmware order. The
    /// first instance reuses the well-known IO-APIC domain; later instances
    /// receive distinct per-instance domains, because their pin number spaces
    /// are independent. Addresses and GSI ranges are validated by the unified
    /// entry against probed hardware, not here.
    ///
    /// The effective legacy route table was computed in declaration order
    /// during discovery; replaying its invalidated identity routes and then
    /// applying the surviving overrides rebuilds that exact table.
    ///
    /// # Errors
    ///
    /// [`KError::InvalidInput`](kerrno::KError::InvalidInput) when there is no
    /// controller (normally reported as [`X86DiscoveryError::Absent`] earlier)
    /// or too many controllers for the instance capacity, and
    /// [`KError::NoMemory`](kerrno::KError::NoMemory) when a per-instance
    /// domain cannot be allocated.
    pub fn to_init_config(&self, policy: IoApicInitPolicy) -> KResult<X86IrqInitConfig> {
        let mut config = X86IrqInitConfig::from_acpi_controllers(self.io_apics(), policy)?;
        for isa_irq in 0..MAX_LEGACY_IRQS {
            if self.invalid_identity & (1u16 << isa_irq) != 0 {
                config.invalidate_identity_route(isa_irq as u8);
            }
        }
        for route in self.overrides() {
            config.set_legacy_override(route.isa_irq, route.gsi, route.trigger);
        }
        Ok(config)
    }
}

/// Discovers x86 interrupt controllers from the boot ACPI MADT.
///
/// # Errors
///
/// [`X86DiscoveryError::Absent`] when ACPI is not initialized, the firmware
/// has no MADT, or the MADT has no usable IO-APIC;
/// [`X86DiscoveryError::Malformed`] on any malformed MADT entry.
pub fn from_acpi() -> Result<X86ApicDeclarations, X86DiscoveryError> {
    let (_, entries) = acpi::find_madt_from_init().ok_or(X86DiscoveryError::Absent)?;
    from_entries(entries)
}

fn from_entries(entries: MadtEntryIter<'_>) -> Result<X86ApicDeclarations, X86DiscoveryError> {
    let mut declarations = X86ApicDeclarations::new();

    // Phase 1: collect every controller. Overrides are interpreted only after
    // all controllers are known, so the order of MADT entries does not affect
    // the result.
    for entry in entries {
        if let MadtEntry::IoApic(io_apic) = entry.map_err(X86DiscoveryError::Malformed)? {
            declarations.retain_io_apic(io_apic);
        }
    }

    if declarations.io_apic_count == 0 {
        return Err(X86DiscoveryError::Absent);
    }

    // Phase 2: walk the same entry stream again and interpret overrides
    // against the complete controller set. Skipped declarations consume no
    // capacity, so a later duplicate of an accepted ISA line still replaces
    // the earlier mapping.
    for entry in entries {
        if let MadtEntry::InterruptSourceOverride(iso) =
            entry.map_err(X86DiscoveryError::Malformed)?
        {
            declarations.apply_override(iso);
        }
    }

    Ok(declarations)
}

impl X86ApicDeclarations {
    fn retain_io_apic(&mut self, io_apic: acpi::IoApicEntry) {
        let retained = &self.io_apics[..self.io_apic_count];
        if io_apic.address == 0 {
            self.record_issue(X86DiscoveryIssue::ZeroIoApicAddress { id: io_apic.id });
            return;
        }
        if retained.iter().any(|io| io.address == io_apic.address) {
            self.record_issue(X86DiscoveryIssue::DuplicateIoApicAddress {
                id: io_apic.id,
                address: io_apic.address,
            });
            return;
        }
        if retained
            .iter()
            .any(|io| io.gsi_base == io_apic.global_system_interrupt_base)
        {
            self.record_issue(X86DiscoveryIssue::GsiBaseConflict {
                id: io_apic.id,
                gsi_base: io_apic.global_system_interrupt_base,
            });
            return;
        }
        if self.io_apic_count == MAX_IO_APICS {
            self.record_issue(X86DiscoveryIssue::TooManyIoApics {
                id: io_apic.id,
                address: io_apic.address,
            });
            return;
        }

        self.io_apics[self.io_apic_count] = IoApicDeclaration {
            id: io_apic.id,
            address: io_apic.address,
            gsi_base: io_apic.global_system_interrupt_base,
        };
        self.io_apic_count += 1;
    }

    fn apply_override(&mut self, iso: acpi::InterruptSourceOverride) {
        if iso.bus != 0 || usize::from(iso.source) >= MAX_LEGACY_IRQS {
            self.record_issue(X86DiscoveryIssue::InvalidIsaSource {
                bus: iso.bus,
                source: iso.source,
            });
            return;
        }
        if iso_flags_reserved(iso.flags) {
            self.record_issue(X86DiscoveryIssue::ReservedIsoFlags {
                bus: iso.bus,
                source: iso.source,
                flags: iso.flags,
            });
            return;
        }
        if !self.owns_gsi(iso.gsi) {
            self.record_issue(X86DiscoveryIssue::UnknownGsi {
                isa_irq: iso.source,
                gsi: iso.gsi,
            });
            return;
        }
        // Capture a duplicate of this ISA line before target invalidation: the
        // last-wins diagnostic must survive even when invalidating the target
        // removes the previous override for the same line.
        let previous_gsi = self.overrides[..self.override_count]
            .iter()
            .find(|entry| entry.isa_irq == iso.source)
            .map(|entry| entry.gsi);
        // Match Linux `mp_override_legacy_irq`: the target is checked against
        // the mapping in force at this point in the declaration stream, before
        // this override is installed. A target still on its identity route is
        // invalidated so the same GSI is not reachable through two lines. A
        // later explicit override for the target line can restore it, and an
        // earlier override that mapped the target elsewhere is left alone.
        if iso.gsi < MAX_LEGACY_IRQS as u32 {
            let target = iso.gsi as u8;
            if self.effective_gsi(target) == Some(iso.gsi) {
                self.invalidate_identity_route(target);
            }
        }

        let trigger = decode_trigger(iso.source, iso.flags);
        // Installing this route revives a line invalidated earlier.
        self.invalid_identity &= !(1u16 << iso.source);
        if let Some(previous_gsi) = previous_gsi {
            self.record_issue(X86DiscoveryIssue::OverriddenIsaMapping {
                isa_irq: iso.source,
                previous_gsi,
                gsi: iso.gsi,
            });
        }
        let existing = self.overrides[..self.override_count]
            .iter()
            .position(|entry| entry.isa_irq == iso.source);
        if let Some(index) = existing {
            self.overrides[index] = IrqOverride {
                isa_irq: iso.source,
                gsi: iso.gsi,
                trigger,
            };
            return;
        }

        // A retained source is unique and below `MAX_LEGACY_IRQS`, so at
        // most one slot per source can be occupied.
        debug_assert!(self.override_count < MAX_LEGACY_IRQS);
        self.overrides[self.override_count] = IrqOverride {
            isa_irq: iso.source,
            gsi: iso.gsi,
            trigger,
        };
        self.override_count += 1;
    }

    fn owns_gsi(&self, gsi: u32) -> bool {
        self.io_apics[..self.io_apic_count]
            .iter()
            .any(|io| io.gsi_base <= gsi)
    }

    /// Effective GSI of one ISA line under the declarations applied so far.
    fn effective_gsi(&self, isa_irq: u8) -> Option<u32> {
        debug_assert!(usize::from(isa_irq) < MAX_LEGACY_IRQS);
        if self.invalid_identity & (1u16 << isa_irq) != 0 {
            return None;
        }
        match self.overrides[..self.override_count]
            .iter()
            .find(|route| route.isa_irq == isa_irq)
        {
            Some(route) => Some(route.gsi),
            None => Some(u32::from(isa_irq)),
        }
    }

    /// Invalidates one line's currently effective identity route.
    ///
    /// Any retained override for the line encoded that identity (its GSI
    /// equals the line) and is dropped with it.
    fn invalidate_identity_route(&mut self, isa_irq: u8) {
        debug_assert!(usize::from(isa_irq) < MAX_LEGACY_IRQS);
        self.invalid_identity |= 1u16 << isa_irq;
        let index = self.overrides[..self.override_count]
            .iter()
            .position(|route| route.isa_irq == isa_irq);
        if let Some(index) = index {
            self.overrides
                .copy_within(index + 1..self.override_count, index);
            self.override_count -= 1;
        }
    }

    fn record_issue(&mut self, issue: X86DiscoveryIssue) {
        warn!("x86 APIC discovery: {issue:?}");
        if self.issue_count < MAX_ISSUES {
            self.issues[self.issue_count] = issue;
            self.issue_count += 1;
        } else {
            self.issues_truncated = true;
        }
    }
}

/// Returns whether the ISO flags use a reserved polarity or trigger encoding.
fn iso_flags_reserved(flags: u16) -> bool {
    (flags & 0b11) == 0b10 || ((flags >> 2) & 0b11) == 0b10
}

/// Decodes MPS INTI polarity (bits 0:1) and trigger mode (bits 2:3).
///
/// `0b00` and `0b01` both mean the platform default (bus-relative) for that
/// field, so they map to high/edge. The reserved `0b10` encoding is filtered
/// out before this runs.
///
/// A level-triggered override for ISA line 0 is treated as edge, matching
/// Linux `mp_override_legacy_irq`: such firmware entries are faulty and a
/// level timer causes an interrupt storm. Polarity is still decoded as given.
fn decode_trigger(isa_irq: u8, flags: u16) -> IrqTrigger {
    let level = isa_irq != 0 && flags >> 2 & 0b11 == 0b11;
    let low = flags & 0b11 == 0b11;
    match (level, low) {
        (false, false) => IrqTrigger::EdgeRising,
        (false, true) => IrqTrigger::EdgeFalling,
        (true, false) => IrqTrigger::LevelHigh,
        (true, true) => IrqTrigger::LevelLow,
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/x86_discovery.rs");

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ACPI firmware description and table helpers.
//!
//! This crate is the static-table subsystem: it owns the boot RSDP, validated
//! table access, and the platform facts the kernel consumes before (and
//! independently of) any AML interpreter context.
//!
//! Module responsibilities:
//!
//! - `desc`: one-shot boot RSDP slot.
//! - `host`: AML host failure vocabulary; no interpreter or OSL state.
//! - `raw`: direct-map access to validated firmware ranges (private).
//! - `table`: boot address access and required/optional lookup policy.
//! - `madt`: boot lookup and re-exported vendor MADT facts.
//! - `mcfg`: first-segment-zero compatibility selection.
//! - `fadt`: PM1a SystemIO/port selection and DSDT compatibility query.
//! - `pci_window`: transitional DSDT `_CRS` byte scan for the PCI host
//!   memory window.
//! - `context`: x86-only AML evaluation ownership, `_PIC`, and host-failure
//!   poisoning.
//! - `prt`: x86-only PCI root bridge and `_PRT` routing-table query.
//!
//! The static queries are usable during early boot (including the
//! pre-allocation x86 handoff) and later. Their availability does not depend on
//! interpreter execution state. The x86 context is a separate, allocation-using
//! path that exists only after the boot allocator is ready.
//! Protocol validation and decoding belong to `acpi_rs`: `sdt::borrowed` for
//! static tables, and `aml::pci_routing` / `aml::resource` for evaluated
//! objects and resource templates. This crate applies support scope and boot
//! policy to decoded facts; it does not maintain parallel protocol parsers.

#![allow(
    clippy::panic,
    reason = "CG-021-no-panic-on-reachable-failure: case (b): boot-critical firmware corruption \
              in RSDP and SDT/MADT fixed headers still panics by contract; MADT entry framing \
              reports typed errors instead, and new code remains reviewed"
)]
#![no_std]

#[cfg(all(target_arch = "x86_64", feature = "interpreter"))]
extern crate alloc;

#[cfg(all(target_arch = "x86_64", feature = "interpreter"))]
pub mod context;
#[cfg(all(target_arch = "x86_64", feature = "interpreter"))]
unittest::path_mod_test!(pub mod acpi_test_support, "../ut-tests/acpi_support.rs");
mod desc;
mod fadt;
mod host;
mod madt;
mod mcfg;
mod pci_window;
#[cfg(all(target_arch = "x86_64", feature = "interpreter"))]
pub mod prt;
mod raw;
mod table;

pub use self::{
    desc::{AcpiDesc, AcpiInitError, desc, init, rsdp_addr},
    fadt::{Pm1aControlBlock, find_facs_address_from_init, find_pm1a_control_block_from_init},
    host::{AmlHost, HostAccessKind, HostFailure, HostFailureReason},
    madt::{
        InterruptSourceOverride, IoApicEntry, LocalApicEntry, MadtEntry, MadtEntryIter, MadtInfo,
        MadtParseError, MadtParseErrorKind, find_madt_from_init, find_madt_from_rsdp,
    },
    mcfg::{McfgAllocation, find_mcfg, find_mcfg_from_init},
    pci_window::{PciHostMemWindow, find_pci_host_mem_window_from_init},
    table::{AcpiTableHeader, find_table, find_table_from_rsdp},
};

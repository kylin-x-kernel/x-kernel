// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The single AML evaluation owner.
//!
//! This module loads the ACPI namespace from the boot tables, applies the
//! interrupt-model switch with `\_PIC(1)`, and exposes scoped evaluation helpers
//! to the `prt` query. It deliberately exposes no namespace or interpreter
//! handle: callers cannot retain interpreter objects or evaluate AML outside
//! the serialized methods here.
//!
//! The context is allocation-based and therefore available only after boot
//! allocation. It is separate from the early, allocation-free static table
//! path in the crate root.

use alloc::{sync::Arc, vec::Vec};
use core::{mem::size_of, str::FromStr};

use acpi_rs::{
    AcpiError, AcpiTables, Handler,
    aml::{
        AmlError,
        namespace::{AmlName, NamespaceLevelKind},
        object::{Object, WrappedObject},
    },
    registers::FixedRegisters,
    sdt::{SdtHeader, Signature, fadt::Fadt},
};

use crate::host::{AmlHost, HostFailure};

/// Largest AML stream accepted from one DSDT or SSDT.
pub const MAX_AML_TABLE_LEN: usize = 16 * 1024 * 1024;
/// Largest interpreted namespace device count accepted by [`AcpiContext`].
const MAX_DEVICE_PATHS: usize = 1024;

/// The interrupt model selected for AML evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqModel {
    /// IO-APIC mode, selected with `\_PIC(1)`.
    IoApic,
}

/// Failure to construct or query the AML evaluation context.
#[derive(Debug)]
pub enum ContextError {
    /// No boot RSDP address is available.
    MissingRsdp,
    /// The FADT is absent.
    MissingFadt,
    /// The DSDT is absent or its address is invalid.
    MissingDsdt,
    /// An AML table is shorter than its fixed header.
    TableTooShort,
    /// An AML table exceeds [`MAX_AML_TABLE_LEN`].
    TableTooLarge,
    /// An AML table failed the vendor's strict signature, length, or checksum
    /// validation.
    InvalidAmlTable(acpi_rs::sdt::borrowed::Error),
    /// A table that was enumerated could not be loaded.
    IncompleteTableSet,
    /// The interpreted namespace exceeded the device-path bound.
    NamespaceTooLarge,
    /// A helper that requires an absolute scope received a relative name.
    NotAbsolute,
    /// The interrupt model was already initialized.
    AlreadyInitialized,
    /// A previous firmware or evaluation failure prevents further use.
    Poisoned,
    /// An unknown operation-region space was rejected by the interpreter.
    UnsupportedRegion(u8),
    /// The host adapter recorded an access failure.
    HostAccess(HostFailure),
    /// A vendor table or interpreter operation failed.
    Vendor(AcpiError),
    /// AML parsing or evaluation failed.
    Aml(AmlError),
}

/// Sticky failure state. Evaluation never resumes after one of these is set.
#[derive(Debug, Clone, Copy)]
enum Poison {
    Host(HostFailure),
    UnsupportedRegion(u8),
    Firmware,
}

impl Poison {
    const fn error(self) -> ContextError {
        match self {
            Self::Host(failure) => ContextError::HostAccess(failure),
            Self::UnsupportedRegion(space) => ContextError::UnsupportedRegion(space),
            Self::Firmware => ContextError::Poisoned,
        }
    }
}

/// The one AML evaluation owner.
///
/// All evaluation methods take `&mut self`, which is the serialization
/// contract of the vendored interpreter. The associated host must not evaluate
/// AML through any other path while this context exists.
pub struct AcpiContext<H: Handler + AmlHost> {
    host: H,
    interpreter: acpi_rs::aml::Interpreter<H>,
    irq_model: Option<IrqModel>,
    poison: Option<Poison>,
}

impl<H: Handler + AmlHost> AcpiContext<H> {
    /// Builds the context from the boot RSDP stored by [`crate::init`].
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::MissingRsdp`] before ACPI initialization, and
    /// the first table, namespace, or host failure after it.
    pub fn new(host: H) -> Result<Self, ContextError> {
        let rsdp_address = crate::desc::rsdp_addr().ok_or(ContextError::MissingRsdp)?;
        // SAFETY: `desc::init` accepts only a boot-firmware RSDP physical
        // address, and the host owns the mapping capability for it.
        unsafe { Self::from_rsdp(host, rsdp_address) }
    }

    /// Builds the context from an explicit physical RSDP address.
    ///
    /// The boot descriptor is not consumed; this is for callers that already
    /// hold a validated firmware handoff.
    ///
    /// # Safety
    ///
    /// `rsdp_address` must be the physical address of a valid RSDP. The host
    /// must be able to map and access every firmware table reachable from it
    /// for the context's lifetime and for every clone of the host.
    ///
    /// # Errors
    ///
    /// Returns the first missing table, malformed AML table, or host access
    /// failure. No partial namespace is returned.
    pub unsafe fn from_rsdp(host: H, rsdp_address: usize) -> Result<Self, ContextError> {
        if rsdp_address == 0 {
            return Err(ContextError::MissingRsdp);
        }

        // SAFETY: the caller guarantees a valid RSDP address and host mapping
        // capability for it.
        let tables = match unsafe { AcpiTables::from_rsdp(host.clone(), rsdp_address) } {
            Ok(tables) => tables,
            Err(error) => return Err(vendor_error(&host, error)),
        };
        if let Some(error) = take_host_error(&host) {
            return Err(error);
        }

        let expected_ssdts = count_ssdts(&tables);
        if let Some(error) = take_host_error(&host) {
            return Err(error);
        }

        let Some(fadt) = tables.find_table::<Fadt>() else {
            if let Some(error) = take_host_error(&host) {
                return Err(error);
            }
            return Err(ContextError::MissingFadt);
        };
        let dsdt = match tables.dsdt() {
            Ok(dsdt) => dsdt,
            Err(error) => {
                if let Some(host_error) = take_host_error(&host) {
                    return Err(host_error);
                }
                log::error!("ACPI: DSDT lookup failed: {error:?}");
                return Err(ContextError::MissingDsdt);
            }
        };
        let registers = match FixedRegisters::new(&fadt, host.clone()) {
            Ok(registers) => Arc::new(registers),
            Err(error) => return Err(vendor_error(&host, error)),
        };
        drop(fadt);

        let interpreter =
            acpi_rs::aml::Interpreter::new(host.clone(), dsdt.revision, registers, None);
        let mut context = Self {
            host,
            interpreter,
            irq_model: None,
            poison: None,
        };

        context.load_aml_table(dsdt, Signature::DSDT)?;
        let ssdts: Vec<_> = tables.ssdts().collect();
        context.check_host()?;
        let mut loaded_ssdts = 0usize;
        for table in ssdts {
            context.load_aml_table(table, Signature::SSDT)?;
            loaded_ssdts = loaded_ssdts.saturating_add(1);
        }
        context.check_host()?;
        if loaded_ssdts != expected_ssdts {
            context.poison = Some(Poison::Firmware);
            return Err(ContextError::IncompleteTableSet);
        }
        context.check_host()?;
        Ok(context)
    }

    /// Returns the initialized interrupt model, if any.
    pub const fn irq_model(&self) -> Option<IrqModel> {
        self.irq_model
    }

    /// Executes `\_PIC(1)` once and marks the context as initialized.
    ///
    /// An absent `_PIC` is tolerated because the model is a firmware hint; the
    /// context still records `model`. A present `_PIC` that fails poisons the
    /// context and leaves the model unset.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::AlreadyInitialized`] on a second call, a host
    /// failure, or the interpreter error from `_PIC`.
    pub fn init_irq_model(&mut self, model: IrqModel) -> Result<(), ContextError> {
        self.check_host()?;
        if self.irq_model.is_some() {
            return Err(ContextError::AlreadyInitialized);
        }

        let name = AmlName::from_str("\\_PIC").map_err(ContextError::Aml)?;
        let args = alloc::vec![Object::Integer(1).wrap()];
        match self.interpreter.evaluate_if_present(name, args) {
            Ok(Some(_)) => log::info!("ACPI: _PIC(1) applied"),
            Ok(None) => log::info!("ACPI: _PIC absent; assuming IO-APIC model"),
            Err(error) => return Err(self.aml_error(error, true)),
        }

        self.check_host()?;
        self.irq_model = Some(model);
        Ok(())
    }

    /// Returns the sticky poison or drains a newly recorded host failure.
    pub(crate) fn check_host(&mut self) -> Result<(), ContextError> {
        if let Some(poison) = self.poison {
            return Err(poison.error());
        }
        if let Some(failure) = self.host.take_failure() {
            self.poison = Some(Poison::Host(failure));
            return Err(ContextError::HostAccess(failure));
        }
        Ok(())
    }

    /// Evaluates `path` and synchronously decodes the result.
    ///
    /// `decode` runs while the host/context pair is exclusively borrowed. It
    /// must copy the values it needs; it may not retain the vendor object or
    /// call back into this context.
    #[cfg(unittest)]
    pub(crate) fn evaluate<T>(
        &mut self,
        path: &AmlName,
        args: Vec<WrappedObject>,
        decode: impl FnOnce(&Object) -> T,
    ) -> Result<T, ContextError> {
        self.check_host()?;
        match self.interpreter.evaluate(path.clone(), args) {
            Ok(object) => {
                self.check_host()?;
                Ok(decode(&object))
            }
            Err(error) => Err(self.aml_error(error, false)),
        }
    }

    /// Evaluates `path` when present and synchronously decodes the result.
    ///
    /// Only an exact absence of `path` returns `Ok(None)`; an absent object
    /// referenced by the method is still an interpreter error.
    pub(crate) fn evaluate_if_present<T>(
        &mut self,
        path: &AmlName,
        args: Vec<WrappedObject>,
        decode: impl FnOnce(&Object) -> T,
    ) -> Result<Option<T>, ContextError> {
        self.check_host()?;
        match self.interpreter.evaluate_if_present(path.clone(), args) {
            Ok(Some(object)) => {
                self.check_host()?;
                Ok(Some(decode(&object)))
            }
            Ok(None) => {
                self.check_host()?;
                Ok(None)
            }
            Err(error) => Err(self.aml_error(error, false)),
        }
    }

    /// Collects every device scope in the interpreted namespace.
    ///
    /// Traversal stops recording new paths after the bound but still walks the
    /// namespace, so the returned error does not depend on traversal order.
    pub(crate) fn device_paths(&mut self) -> Result<Vec<AmlName>, ContextError> {
        self.check_host()?;
        let mut namespace = self.interpreter.namespace.lock();
        let mut paths = Vec::new();
        let mut overflow = false;
        let result = namespace.traverse(|name, level| {
            if level.kind == NamespaceLevelKind::Device {
                if paths.len() < MAX_DEVICE_PATHS {
                    paths.push(name.clone());
                } else {
                    overflow = true;
                }
            }
            Ok(true)
        });
        drop(namespace);

        match result {
            Ok(()) if overflow => Err(ContextError::NamespaceTooLarge),
            Ok(()) => Ok(paths),
            Err(error) => Err(ContextError::Aml(error)),
        }
    }

    /// Resolves a link-like name with the ACPI search rules.
    ///
    /// `scope` must be absolute. The result is always absolute.
    pub(crate) fn search_for_level(
        &mut self,
        level: &AmlName,
        scope: &AmlName,
    ) -> Result<AmlName, ContextError> {
        self.check_host()?;
        if !scope.is_absolute() {
            return Err(ContextError::NotAbsolute);
        }
        let namespace = self.interpreter.namespace.lock();
        let resolved = namespace.search_for_level(level, scope);
        drop(namespace);

        let resolved = resolved.map_err(ContextError::Aml)?;
        if resolved.is_absolute() {
            Ok(resolved)
        } else {
            resolved.resolve(scope).map_err(ContextError::Aml)
        }
    }

    /// Resolves a name against an absolute scope.
    pub(crate) fn resolve_name(
        &mut self,
        scope: &AmlName,
        name: &AmlName,
    ) -> Result<AmlName, ContextError> {
        self.check_host()?;
        if !scope.is_absolute() {
            return Err(ContextError::NotAbsolute);
        }
        name.resolve(scope).map_err(ContextError::Aml)
    }

    /// Converts an AML failure, preserving host failure precedence.
    fn aml_error(&mut self, error: AmlError, poison_firmware: bool) -> ContextError {
        if let Some(failure) = self.host.take_failure() {
            self.poison = Some(Poison::Host(failure));
            return ContextError::HostAccess(failure);
        }

        if let AmlError::NoHandlerForRegionAccess(space) = error {
            return self.unsupported_region(space_code(space));
        }

        if poison_firmware {
            self.poison = Some(Poison::Firmware);
        }
        ContextError::Aml(error)
    }

    /// Records an unsupported operation region through the host hook.
    fn unsupported_region(&mut self, space: u8) -> ContextError {
        self.host.note_unsupported_region(space);
        match self.host.take_failure() {
            Some(failure) => {
                self.poison = Some(Poison::Host(failure));
                ContextError::HostAccess(failure)
            }
            None => {
                self.poison = Some(Poison::UnsupportedRegion(space));
                ContextError::UnsupportedRegion(space)
            }
        }
    }

    /// Maps one AML table, validates it with the vendor's strict borrowed
    /// parser, and loads the validated body.
    ///
    /// The vendor table enumeration used to obtain `table` does not verify the
    /// DSDT/SSDT checksum or the DSDT's target signature, so a readable but
    /// corrupted table must be rejected here rather than handed to the
    /// interpreter.
    fn load_aml_table(
        &mut self,
        table: acpi_rs::AmlTable,
        expected: Signature,
    ) -> Result<(), ContextError> {
        let header_len = size_of::<SdtHeader>();
        if (table.length as usize) < header_len {
            self.poison = Some(Poison::Firmware);
            return Err(ContextError::TableTooShort);
        }
        if table.length as usize > MAX_AML_TABLE_LEN {
            self.poison = Some(Poison::Firmware);
            return Err(ContextError::TableTooLarge);
        }

        // SAFETY: the address is the DSDT/SSDT address from the boot table set
        // and the host validates the mapping against the kernel memory map.
        let mapping = match unsafe {
            self.host
                .map_physical_region_read_only::<u8>(table.phys_address, table.length as usize)
        } {
            Ok(mapping) => mapping,
            Err(error) => return Err(self.vendor_error(error)),
        };
        // SAFETY: `mapping` covers `table.length` bytes and is read-only for
        // this scope.
        let bytes = unsafe {
            core::slice::from_raw_parts(mapping.virtual_start().as_ptr(), table.length as usize)
        };
        let validated = match acpi_rs::sdt::borrowed::Table::parse(bytes) {
            Ok(validated) => validated,
            Err(error) => {
                self.poison = Some(Poison::Firmware);
                return Err(ContextError::InvalidAmlTable(error));
            }
        };
        if let Err(error) = validated.require_signature(expected.to_bytes()) {
            self.poison = Some(Poison::Firmware);
            return Err(ContextError::InvalidAmlTable(error));
        }
        let result = self.interpreter.load_table(validated.body());
        drop(mapping);

        match result {
            Ok(()) => self.check_host(),
            Err(error) => Err(self.aml_error(error, true)),
        }
    }

    /// Maps a vendor error after checking whether the host recorded a failure.
    fn vendor_error(&mut self, error: AcpiError) -> ContextError {
        if let Some(failure) = self.host.take_failure() {
            self.poison = Some(Poison::Host(failure));
            return ContextError::HostAccess(failure);
        }
        ContextError::Vendor(error)
    }

    /// Test-only constructor around an already-built interpreter.
    #[cfg(unittest)]
    pub(crate) const fn from_interpreter_for_test(
        host: H,
        interpreter: acpi_rs::aml::Interpreter<H>,
    ) -> Self {
        Self {
            host,
            interpreter,
            irq_model: None,
            poison: None,
        }
    }

    /// Test-only scoped namespace access for fixture injection.
    #[cfg(unittest)]
    pub(crate) fn with_namespace_for_test<R>(
        &mut self,
        f: impl FnOnce(&mut acpi_rs::aml::namespace::Namespace) -> R,
    ) -> R {
        let mut namespace = self.interpreter.namespace.lock();
        f(&mut namespace)
    }
}

/// Polls a host that is not yet owned by a context.
fn take_host_error<H: AmlHost>(host: &H) -> Option<ContextError> {
    host.take_failure().map(ContextError::HostAccess)
}

/// Maps a vendor error when no context exists yet.
fn vendor_error<H: AmlHost>(host: &H, error: AcpiError) -> ContextError {
    match host.take_failure() {
        Some(failure) => ContextError::HostAccess(failure),
        None => ContextError::Vendor(error),
    }
}

/// Counts SSDT headers while also forcing full header enumeration.
fn count_ssdts<H: Handler>(tables: &AcpiTables<H>) -> usize {
    tables
        .table_headers()
        .filter(|(_, header)| header.signature == Signature::SSDT)
        .count()
}

/// Encodes a region space for the host failure hook.
fn space_code(space: acpi_rs::aml::op_region::RegionSpace) -> u8 {
    use acpi_rs::aml::op_region::RegionSpace;

    match space {
        RegionSpace::SystemMemory => 0,
        RegionSpace::SystemIO => 1,
        RegionSpace::PciConfig => 2,
        RegionSpace::EmbeddedControl => 3,
        RegionSpace::SmBus => 4,
        RegionSpace::SystemCmos => 5,
        RegionSpace::PciBarTarget => 6,
        RegionSpace::Ipmi => 7,
        RegionSpace::GeneralPurposeIo => 8,
        RegionSpace::GenericSerialBus => 9,
        RegionSpace::Pcc => 10,
        RegionSpace::Oem(code) => code,
    }
}

#[cfg(unittest)]
unittest::path_mod_test!(tests, "../ut-tests/acpi_context.rs");

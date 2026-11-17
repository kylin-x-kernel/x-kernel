// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel host adapter for the vendored ACPI AML interpreter.
//!
//! The interpreter returns raw values from its [`acpi_rs::Handler`] methods and
//! cannot stop itself when one fails. This adapter therefore owns two pieces of
//! policy: it validates every firmware-supplied address against the kernel
//! memory map and the capability contracts in [`driver_os_api`], and it latches
//! the first failure so no later access reaches hardware. A failed evaluation
//! is rejected by the ACPI context; the adapter does not attempt to roll back
//! writes that already completed.
//!
//! The adapter is not a global singleton. The host/PCI integration constructs
//! one with borrowed capabilities, and the ACPI context owns the interpreter
//! lifetime. Every clone shares one writable-object registry, which enforces
//! host-issued writable exclusivity; constructing independent adapters over
//! the same memory would bypass that enforcement and is outside the serial
//! execution contract.
//!
//! # Serial execution
//!
//! The failure gate is checked before each access and latched by whichever
//! evaluation fails first. It is not a lock: it assumes the interpreter is
//! evaluated by one owner at a time, as the vendored interpreter requires.
//! The ACPI context must serialize evaluation; the adapter's [`Clone`] exists
//! because the vendor stores a handler in every [`PhysicalMapping`], not so
//! that independent evaluations can share it concurrently.
//!
//! AML mutexes are bounded, non-reentrant spin locks. The adapter cannot
//! identify the owning evaluation, so re-acquiring a held mutex times out and
//! poisons the host instead of deadlocking.

mod memory;
mod state;

use alloc::sync::Arc;
use core::{
    mem::{align_of, size_of},
    ptr::NonNull,
    sync::atomic::Ordering,
};

use acpi_rs::{
    AcpiError, Handle, Handler, PciAddress, PhysicalMapping, ReadOnlyPhysicalMapping,
    aml::{AmlError, op_region::RegionSpace},
    sdt::SdtHeader,
};
use driver_os_api::{PortOp, PortWidth, ResError, TimeOp};
use khal::{
    firmware::{AmlHost, HostAccessKind, HostFailure, HostFailureReason},
    mem::PhysAddr,
};
use ktime_types::TimeSpan;
use pci::{DeviceFunction, PciConfigAccess, PciConfigError};

pub use self::memory::{AcpiMemoryMap, KernelMemoryMap, MemoryClass, MemoryRangeError};
#[cfg(unittest)]
use self::state::MAX_TABLE_RANGES;
use self::state::{HostState, MAX_MUTEXES};
use crate::XKernelResourceProvider;

/// Largest AML `Stall` the adapter performs. Firmware is expected to stall for
/// at most 100 microseconds; the adapter keeps a bounded margin.
const MAX_STALL_US: u64 = 1_000;
/// Longest bounded AML mutex wait. This also caps the vendor's "indefinite"
/// timeout because the adapter never blocks boot indefinitely.
const MAX_MUTEX_WAIT_MS: u64 = 100;

/// A validated ACPI AML host backed by X-Kernel capabilities.
///
/// All clones share one failure gate. Only the ACPI context that owns the
/// interpreter may evaluate AML, and it must not evaluate concurrently;
/// cloning is required by the vendor's [`Handler`] contract, not for parallel
/// evaluations.
#[derive(Clone)]
pub struct AcpiOs {
    memory: &'static dyn AcpiMemoryMap,
    time: &'static dyn TimeOp,
    ports: &'static dyn PortOp,
    pci: Option<PciConfigAccess>,
    state: Arc<HostState>,
}

impl AcpiOs {
    /// Constructs an adapter over borrowed host capabilities.
    pub fn new(
        memory: &'static dyn AcpiMemoryMap,
        time: &'static dyn TimeOp,
        ports: &'static dyn PortOp,
        pci: Option<PciConfigAccess>,
    ) -> Self {
        Self {
            memory,
            time,
            ports,
            pci,
            state: Arc::new(HostState::new()),
        }
    }

    /// Constructs an adapter over the generic X-Kernel capabilities.
    pub fn for_kernel(pci: Option<PciConfigAccess>) -> Self {
        static MEMORY: KernelMemoryMap = KernelMemoryMap;
        static PROVIDER: XKernelResourceProvider = XKernelResourceProvider;
        Self::new(&MEMORY, &PROVIDER, &PROVIDER, pci)
    }

    /// Records an operation-region space the adapter does not implement.
    ///
    /// The vendored interpreter rejects such a space with
    /// `AmlError::NoHandlerForRegionAccess` before calling a [`Handler`] method,
    /// so the ACPI context must report it here to poison the host.
    pub fn record_unsupported_region(&self, space: RegionSpace) {
        self.note_unsupported_region(region_space_code(space));
    }

    fn failed(&self) -> bool {
        self.state.failed.load(Ordering::Acquire)
    }

    /// Records the first failure and latches the access gate.
    fn record(&self, access: HostAccessKind, address: u64, width: u8, reason: HostFailureReason) {
        if self.failed() {
            return;
        }
        let failure = HostFailure::new(access, address, width, reason);
        let recorded = {
            let mut slot = self.state.failure.lock();
            if slot.is_none() {
                *slot = Some(failure);
                self.state.failed.store(true, Ordering::Release);
                true
            } else {
                false
            }
        };
        if recorded {
            log::error!("ACPI host access failed: {failure:?}");
        }
    }

    fn record_memory_error(&self, address: usize, width: u8, error: MemoryRangeError) {
        let reason = match error {
            MemoryRangeError::Wrap => HostFailureReason::Wrap,
            MemoryRangeError::Mixed => HostFailureReason::NotPermitted,
            MemoryRangeError::Empty | MemoryRangeError::Unmapped => HostFailureReason::OutOfRange,
        };
        self.record(HostAccessKind::SystemMemory, address as u64, width, reason);
    }

    fn record_res_error(&self, access: HostAccessKind, address: u64, width: u8, error: ResError) {
        let reason = match error {
            ResError::Unsupported => HostFailureReason::Unsupported,
            ResError::InvalidResource | ResError::BadState => HostFailureReason::OutOfRange,
            ResError::NoMemory => HostFailureReason::Allocation,
            ResError::Io | ResError::MappingFailed => HostFailureReason::Io,
            ResError::Busy
            | ResError::AlreadyExists
            | ResError::NotFound
            | ResError::Interrupted => HostFailureReason::Io,
        };
        self.record(access, address, width, reason);
    }

    /// Validates a SystemMemory access and rejects writes into table memory.
    ///
    /// Returns `None` after recording the failure.
    fn checked_system_memory(&self, address: usize, width: u8, for_write: bool) -> Option<usize> {
        let size = width as usize;
        if size == 0 || width > 8 || !address.is_multiple_of(size) {
            self.record(
                HostAccessKind::SystemMemory,
                address as u64,
                width,
                HostFailureReason::OutOfRange,
            );
            return None;
        }
        let end = match address.checked_add(size) {
            Some(end) => end,
            None => {
                self.record(
                    HostAccessKind::SystemMemory,
                    address as u64,
                    width,
                    HostFailureReason::Wrap,
                );
                return None;
            }
        };

        match self.memory.classify_range(address, size) {
            Ok(MemoryClass::Acpi) => {}
            Ok(_) => {
                self.record(
                    HostAccessKind::SystemMemory,
                    address as u64,
                    width,
                    HostFailureReason::NotPermitted,
                );
                return None;
            }
            Err(error) => {
                self.record_memory_error(address, width, error);
                return None;
            }
        }

        if for_write && self.state.table_ranges.lock().overlaps(address, end) {
            self.record(
                HostAccessKind::SystemMemory,
                address as u64,
                width,
                HostFailureReason::NotPermitted,
            );
            return None;
        }
        Some(address)
    }

    fn read_memory(&self, address: usize, width: u8) -> u64 {
        if self.failed() {
            return 0;
        }
        let Some(address) = self.checked_system_memory(address, width, false) else {
            return 0;
        };
        let pointer = khal::mem::p2v(PhysAddr::from(address)).as_usize();
        match width {
            1 => {
                // SAFETY: `classify_range` accepted ACPI-reserved memory,
                // whose `AcpiMemoryMap` contract guarantees the range is
                // direct-mapped and readable; `u8` needs no alignment.
                unsafe { core::ptr::read_volatile(pointer as *const u8) as u64 }
            }
            2 => {
                // SAFETY: as above, and the 2-byte alignment was checked.
                unsafe { core::ptr::read_volatile(pointer as *const u16) as u64 }
            }
            4 => {
                // SAFETY: as above, and the 4-byte alignment was checked.
                unsafe { core::ptr::read_volatile(pointer as *const u32) as u64 }
            }
            8 => {
                // SAFETY: as above, and the 8-byte alignment was checked.
                unsafe { core::ptr::read_volatile(pointer as *const u64) }
            }
            _ => 0,
        }
    }

    fn write_memory(&self, address: usize, width: u8, value: u64) {
        if self.failed() {
            return;
        }
        let Some(address) = self.checked_system_memory(address, width, true) else {
            return;
        };
        let pointer = khal::mem::p2v(PhysAddr::from(address)).as_usize();
        match width {
            1 => {
                // SAFETY: `classify_range` accepted ACPI-reserved memory,
                // whose `AcpiMemoryMap` contract guarantees the range is
                // direct-mapped and writable; `u8` needs no alignment.
                unsafe { core::ptr::write_volatile(pointer as *mut u8, value as u8) }
            }
            2 => {
                // SAFETY: as above, and the 2-byte alignment was checked.
                unsafe { core::ptr::write_volatile(pointer as *mut u16, value as u16) }
            }
            4 => {
                // SAFETY: as above, and the 4-byte alignment was checked.
                unsafe { core::ptr::write_volatile(pointer as *mut u32, value as u32) }
            }
            8 => {
                // SAFETY: as above, and the 8-byte alignment was checked.
                unsafe { core::ptr::write_volatile(pointer as *mut u64, value) }
            }
            _ => {}
        }
    }

    fn read_io(&self, port: u16, width: PortWidth) -> u32 {
        if self.failed() {
            return 0;
        }
        if !port_access_fits(port, width) {
            self.record(
                HostAccessKind::SystemIo,
                u64::from(port),
                width.bytes() as u8,
                HostFailureReason::OutOfRange,
            );
            return 0;
        }
        match self.ports.read_port(port, width) {
            Ok(value) => value,
            Err(error) => {
                self.record_res_error(
                    HostAccessKind::SystemIo,
                    u64::from(port),
                    width.bytes() as u8,
                    error,
                );
                0
            }
        }
    }

    fn write_io(&self, port: u16, width: PortWidth, value: u32) {
        if self.failed() {
            return;
        }
        if !port_access_fits(port, width) || !width.accepts(value) {
            self.record(
                HostAccessKind::SystemIo,
                u64::from(port),
                width.bytes() as u8,
                HostFailureReason::OutOfRange,
            );
            return;
        }
        if let Err(error) = self.ports.write_port(port, width, value) {
            self.record_res_error(
                HostAccessKind::SystemIo,
                u64::from(port),
                width.bytes() as u8,
                error,
            );
        }
    }

    fn pci_device(&self, address: PciAddress) -> Option<DeviceFunction> {
        if address.segment() != 0 {
            self.record(
                HostAccessKind::PciConfig,
                u64::from(address.segment()),
                0,
                HostFailureReason::Unsupported,
            );
            return None;
        }
        Some(DeviceFunction {
            bus: address.bus(),
            device: address.device(),
            function: address.function(),
        })
    }

    fn read_pci(&self, address: PciAddress, offset: u16, width: usize) -> u32 {
        if self.failed() {
            return 0;
        }
        let Some(device) = self.pci_device(address) else {
            return 0;
        };
        let Some(config) = self.pci else {
            self.record(
                HostAccessKind::PciConfig,
                u64::from(offset),
                width as u8,
                HostFailureReason::Unsupported,
            );
            return 0;
        };
        match config.try_read(device, offset, width) {
            Ok(value) => value,
            Err(error) => {
                self.record(
                    HostAccessKind::PciConfig,
                    u64::from(offset),
                    width as u8,
                    pci_error_reason(error),
                );
                0
            }
        }
    }

    fn write_pci(&self, address: PciAddress, offset: u16, width: usize, value: u32) {
        if self.failed() {
            return;
        }
        let Some(device) = self.pci_device(address) else {
            return;
        };
        let Some(config) = self.pci else {
            self.record(
                HostAccessKind::PciConfig,
                u64::from(offset),
                width as u8,
                HostFailureReason::Unsupported,
            );
            return;
        };
        if let Err(error) = config.try_write(device, offset, width, value) {
            self.record(
                HostAccessKind::PciConfig,
                u64::from(offset),
                width as u8,
                pci_error_reason(error),
            );
        }
    }

    /// Validates one host mapping request and returns its direct-map pointer
    /// together with the validated mapping length.
    ///
    /// `writable` selects the contract of the mapping that will be
    /// constructed. A writable mapping requires `MemoryClass::Acpi` memory; a
    /// read-only mapping also accepts `MemoryClass::FirmwareReserved`, which
    /// only guarantees read access.
    ///
    /// A read-only table request may name a region shorter than `T`: the
    /// vendor maps a typed table with its declared length, and revision-bounded
    /// tables (for example an ACPI 2.0 FADT) are shorter than the vendor's
    /// full structure. The host then maps `size_of::<T>()` bytes so the
    /// returned pointer is a complete, readable `T`; the vendor still only
    /// interprets the fields its table revision declares. Writable mappings
    /// and regions shorter than an SDT header must cover the whole object.
    ///
    /// The host enforces the writable constructor's exclusivity requirement
    /// with `WritableRanges`: the `T` object range of every live writable
    /// mapping is retained until its unmap hook removes it. A second writable
    /// mapping that overlaps a live object, or a writable mapping that touches
    /// a read-only table range, is rejected. Read-only mappings check the
    /// reverse direction because the vendor reads the whole mapped region
    /// through a shared slice.
    fn checked_mapping<T>(
        &self,
        physical_address: usize,
        size: usize,
        writable: bool,
    ) -> Result<(NonNull<T>, usize), AcpiError> {
        if self.failed() {
            return Err(AcpiError::HostMappingFailed);
        }
        let mapped_size = if size >= size_of::<T>() {
            size
        } else if !writable && size >= size_of::<SdtHeader>() {
            size_of::<T>()
        } else {
            return Err(self.table_mapping_failed(physical_address, HostFailureReason::OutOfRange));
        };
        let end = match physical_address.checked_add(mapped_size) {
            Some(end) => end,
            None => {
                return Err(self.table_mapping_failed(physical_address, HostFailureReason::Wrap));
            }
        };

        match self.memory.classify_range(physical_address, mapped_size) {
            Ok(MemoryClass::Acpi) => {}
            Ok(MemoryClass::FirmwareReserved) if !writable => {}
            Ok(_) => {
                return Err(
                    self.table_mapping_failed(physical_address, HostFailureReason::NotPermitted)
                );
            }
            Err(error) => {
                let reason = match error {
                    MemoryRangeError::Wrap => HostFailureReason::Wrap,
                    MemoryRangeError::Mixed => HostFailureReason::NotPermitted,
                    MemoryRangeError::Empty | MemoryRangeError::Unmapped => {
                        HostFailureReason::OutOfRange
                    }
                };
                return Err(self.table_mapping_failed(physical_address, reason));
            }
        }

        if !physical_address.is_multiple_of(align_of::<T>()) {
            return Err(self.table_mapping_failed(physical_address, HostFailureReason::OutOfRange));
        }
        let object_end = match physical_address.checked_add(size_of::<T>()) {
            Some(object_end) => object_end,
            None => {
                return Err(self.table_mapping_failed(physical_address, HostFailureReason::Wrap));
            }
        };

        let virtual_address = khal::mem::p2v(PhysAddr::from(physical_address)).as_usize();
        let Some(virtual_start) = NonNull::new(virtual_address as *mut T) else {
            return Err(self.table_mapping_failed(physical_address, HostFailureReason::Allocation));
        };

        if writable {
            // Writable mappings expose `&mut T`; no other live writable
            // mapping may cover the same object. The requested region must
            // also stay out of table memory, which is read-only.
            if self
                .state
                .table_ranges
                .lock()
                .overlaps(physical_address, end)
            {
                return Err(
                    self.table_mapping_failed(physical_address, HostFailureReason::NotPermitted)
                );
            }
            let mut live = self.state.writable_ranges.lock();
            if live.overlaps(physical_address, object_end) {
                return Err(
                    self.table_mapping_failed(physical_address, HostFailureReason::NotPermitted)
                );
            }
            if !live.push(physical_address, object_end) {
                return Err(
                    self.table_mapping_failed(physical_address, HostFailureReason::Allocation)
                );
            }
        } else {
            // The vendor reads the whole mapped region through a shared slice,
            // so it must not overlap a live writable object. Read-only table
            // ranges themselves are retained to reject later SystemMemory
            // writes.
            if self
                .state
                .writable_ranges
                .lock()
                .overlaps(physical_address, end)
            {
                return Err(
                    self.table_mapping_failed(physical_address, HostFailureReason::NotPermitted)
                );
            }
            if !self.state.table_ranges.lock().push(physical_address, end) {
                return Err(
                    self.table_mapping_failed(physical_address, HostFailureReason::Allocation)
                );
            }
        }

        Ok((virtual_start, mapped_size))
    }

    /// Records a rejected table mapping and returns the interpreter error.
    fn table_mapping_failed(
        &self,
        physical_address: usize,
        reason: HostFailureReason,
    ) -> AcpiError {
        self.record(HostAccessKind::TableMap, physical_address as u64, 0, reason);
        AcpiError::HostMappingFailed
    }
}

impl AmlHost for AcpiOs {
    fn take_failure(&self) -> Option<HostFailure> {
        if !self.failed() {
            return None;
        }
        self.state.failure.lock().take()
    }

    fn note_unsupported_region(&self, space: u8) {
        self.record(
            HostAccessKind::UnsupportedRegion,
            u64::from(space),
            0,
            HostFailureReason::Unsupported,
        );
    }
}

impl Handler for AcpiOs {
    unsafe fn map_physical_region<T>(
        &self,
        physical_address: usize,
        size: usize,
    ) -> Result<PhysicalMapping<Self, T>, AcpiError> {
        let (virtual_start, mapped_size) =
            self.checked_mapping::<T>(physical_address, size, true)?;
        // SAFETY: `checked_mapping` accepted the whole `[physical_address,
        // physical_address + mapped_size)` range as `MemoryClass::Acpi`, which
        // `AcpiMemoryMap` guarantees is direct-mapped, readable and writable
        // for the adapter's lifetime; a writable request must cover
        // `size_of::<T>()`, so `mapped_size == size`, and the address is
        // aligned for `T`. It also retained `[physical_address,
        // physical_address + size_of::<T>())` as a live writable object and
        // rejected every existing writable mapping or table range that
        // overlaps it, so this host does not hand out a second `&mut T` for
        // the same object. The unsafe caller must not create raw aliases
        // outside this host.
        Ok(unsafe {
            PhysicalMapping::new(
                physical_address,
                virtual_start,
                size,
                mapped_size,
                self.clone(),
            )
        })
    }

    unsafe fn map_physical_region_read_only<T>(
        &self,
        physical_address: usize,
        size: usize,
    ) -> Result<ReadOnlyPhysicalMapping<Self, T>, AcpiError> {
        let (virtual_start, mapped_size) =
            self.checked_mapping::<T>(physical_address, size, false)?;
        // SAFETY: `checked_mapping` accepted the whole `[physical_address,
        // physical_address + mapped_size)` range as ACPI or firmware-reserved
        // memory, which `AcpiMemoryMap` guarantees is direct-mapped and
        // readable for the adapter's lifetime; firmware-reserved memory may
        // not be writable, so this mapping is read-only. It maps at least
        // `size_of::<T>()` bytes, so `virtual_start` is a complete `T`, and
        // the address is aligned for `T`. It also rejected the range if a live
        // writable object overlapped it, so no `&T` from this mapping shares
        // bytes with a host-issued `&mut T`. The returned mapping never yields
        // `&mut T`, so overlapping read-only table mappings are allowed.
        Ok(unsafe {
            ReadOnlyPhysicalMapping::new(
                physical_address,
                virtual_start,
                mapped_size,
                mapped_size,
                self.clone(),
            )
        })
    }

    fn unmap_physical_region<T>(region: &PhysicalMapping<Self, T>) {
        // Hardware regions live in the kernel direct map; only the ownership
        // record changes. Release the object range so a later mapping of the
        // same `T` is allowed.
        let start = region.physical_start();
        if let Some(end) = start.checked_add(size_of::<T>()) {
            region
                .handler()
                .state
                .writable_ranges
                .lock()
                .remove(start, end);
        }
    }

    fn unmap_physical_region_read_only<T>(_: &ReadOnlyPhysicalMapping<Self, T>) {
        // Table memory lives in the kernel direct map; no mapping was created.
    }

    fn read_u8(&self, address: usize) -> u8 {
        self.read_memory(address, 1) as u8
    }

    fn read_u16(&self, address: usize) -> u16 {
        self.read_memory(address, 2) as u16
    }

    fn read_u32(&self, address: usize) -> u32 {
        self.read_memory(address, 4) as u32
    }

    fn read_u64(&self, address: usize) -> u64 {
        self.read_memory(address, 8)
    }

    fn write_u8(&self, address: usize, value: u8) {
        self.write_memory(address, 1, u64::from(value));
    }

    fn write_u16(&self, address: usize, value: u16) {
        self.write_memory(address, 2, u64::from(value));
    }

    fn write_u32(&self, address: usize, value: u32) {
        self.write_memory(address, 4, u64::from(value));
    }

    fn write_u64(&self, address: usize, value: u64) {
        self.write_memory(address, 8, value);
    }

    fn read_io_u8(&self, port: u16) -> u8 {
        self.read_io(port, PortWidth::U8) as u8
    }

    fn read_io_u16(&self, port: u16) -> u16 {
        self.read_io(port, PortWidth::U16) as u16
    }

    fn read_io_u32(&self, port: u16) -> u32 {
        self.read_io(port, PortWidth::U32)
    }

    fn write_io_u8(&self, port: u16, value: u8) {
        self.write_io(port, PortWidth::U8, u32::from(value));
    }

    fn write_io_u16(&self, port: u16, value: u16) {
        self.write_io(port, PortWidth::U16, u32::from(value));
    }

    fn write_io_u32(&self, port: u16, value: u32) {
        self.write_io(port, PortWidth::U32, value);
    }

    fn read_pci_u8(&self, address: PciAddress, offset: u16) -> u8 {
        self.read_pci(address, offset, 1) as u8
    }

    fn read_pci_u16(&self, address: PciAddress, offset: u16) -> u16 {
        self.read_pci(address, offset, 2) as u16
    }

    fn read_pci_u32(&self, address: PciAddress, offset: u16) -> u32 {
        self.read_pci(address, offset, 4)
    }

    fn write_pci_u8(&self, address: PciAddress, offset: u16, value: u8) {
        self.write_pci(address, offset, 1, u32::from(value));
    }

    fn write_pci_u16(&self, address: PciAddress, offset: u16, value: u16) {
        self.write_pci(address, offset, 2, u32::from(value));
    }

    fn write_pci_u32(&self, address: PciAddress, offset: u16, value: u32) {
        self.write_pci(address, offset, 4, value);
    }

    fn nanos_since_boot(&self) -> u64 {
        self.time.monotonic_time().as_nanos_u64_saturating()
    }

    fn stall(&self, microseconds: u64) {
        if self.failed() {
            return;
        }
        if microseconds > MAX_STALL_US {
            self.record(
                HostAccessKind::Stall,
                microseconds,
                0,
                HostFailureReason::OutOfRange,
            );
            return;
        }
        let start = self.time.monotonic_time();
        let Some(deadline) = start.checked_add(TimeSpan::from_micros(microseconds)) else {
            self.record(
                HostAccessKind::Stall,
                microseconds,
                0,
                HostFailureReason::OutOfRange,
            );
            return;
        };
        while self.time.monotonic_time() < deadline {
            core::hint::spin_loop();
        }
    }

    fn sleep(&self, milliseconds: u64) {
        if self.failed() {
            return;
        }
        self.record(
            HostAccessKind::Sleep,
            milliseconds,
            0,
            HostFailureReason::Unsupported,
        );
    }

    fn create_mutex(&self) -> Handle {
        if self.failed() {
            return Handle(u32::MAX);
        }
        let index = self.state.next_mutex.fetch_add(1, Ordering::Relaxed);
        if index >= MAX_MUTEXES {
            self.record(
                HostAccessKind::Mutex,
                index as u64,
                0,
                HostFailureReason::Allocation,
            );
            return Handle(u32::MAX);
        }
        Handle(index as u32)
    }

    fn acquire(&self, mutex: Handle, timeout_ms: u16) -> Result<(), AmlError> {
        if self.failed() {
            return Err(AmlError::MutexAcquireTimeout);
        }
        let Ok(index) = usize::try_from(mutex.0) else {
            self.record(
                HostAccessKind::Mutex,
                u64::from(mutex.0),
                0,
                HostFailureReason::Unsupported,
            );
            return Err(AmlError::MutexAcquireTimeout);
        };
        let Some(held) = self.state.mutexes.get(index) else {
            self.record(
                HostAccessKind::Mutex,
                u64::from(mutex.0),
                0,
                HostFailureReason::Unsupported,
            );
            return Err(AmlError::MutexAcquireTimeout);
        };

        if held
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return Ok(());
        }

        let limit_ms = match timeout_ms {
            0 => 0,
            u16::MAX => MAX_MUTEX_WAIT_MS,
            value => u64::from(value).min(MAX_MUTEX_WAIT_MS),
        };
        let start = self.time.monotonic_time();
        let deadline = if limit_ms == 0 {
            None
        } else {
            start.checked_add(TimeSpan::from_millis(limit_ms))
        };

        loop {
            if held
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return Ok(());
            }
            if limit_ms == 0
                || deadline.is_none_or(|deadline| self.time.monotonic_time() >= deadline)
            {
                self.record(
                    HostAccessKind::Mutex,
                    u64::from(mutex.0),
                    0,
                    HostFailureReason::Timeout,
                );
                return Err(AmlError::MutexAcquireTimeout);
            }
            core::hint::spin_loop();
        }
    }

    fn release(&self, mutex: Handle) {
        let Ok(index) = usize::try_from(mutex.0) else {
            return;
        };
        if let Some(held) = self.state.mutexes.get(index) {
            held.store(false, Ordering::Release);
        }
    }

    fn handle_fatal_error(&self, fatal_type: u8, fatal_code: u32, fatal_arg: u64) {
        self.record(
            HostAccessKind::AmlFatal,
            fatal_arg,
            0,
            HostFailureReason::Unsupported,
        );
        log::error!("AML fatal error: type={fatal_type} code={fatal_code} arg={fatal_arg:#x}");
    }
}

fn port_access_fits(port: u16, width: PortWidth) -> bool {
    port as u32 + width.bytes() as u32 - 1 <= u16::MAX as u32
}

fn pci_error_reason(error: PciConfigError) -> HostFailureReason {
    match error {
        PciConfigError::InvalidBdf
        | PciConfigError::InvalidOffset
        | PciConfigError::UnsupportedWidth
        | PciConfigError::OutOfWindow => HostFailureReason::PciAccess,
    }
}

/// Encodes a region space for the `HostFailure` address field.
fn region_space_code(space: RegionSpace) -> u8 {
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

unittest::path_mod_test!(tests, "../../ut-tests/acpi_osl.rs");

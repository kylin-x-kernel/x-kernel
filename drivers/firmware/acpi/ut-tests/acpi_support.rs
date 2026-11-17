// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared fixtures for the x86 ACPI context and PRT unit tests.

use alloc::{
    format, rc::Rc, string::{String, ToString}, sync::Arc, vec, vec::Vec,
};
use core::{cell::RefCell, mem::size_of, ptr::NonNull, str::FromStr};

use acpi_rs::{
    AcpiError, Handle, Handler, PhysicalMapping, PciAddress, ReadOnlyPhysicalMapping,
    address::{AddressSpace, GenericAddress, MappedGas},
    aml::{
        AmlError, Interpreter,
        namespace::{AmlName, NamespaceLevelKind},
        object::{Object, WrappedObject},
    },
    registers::{FixedRegisters, Pm1ControlRegisterBlock, Pm1EventRegisterBlock},
};

use crate::context::{AcpiContext, ContextError};
use crate::host::{AmlHost, HostAccessKind, HostFailure, HostFailureReason};

pub const PHYSICAL_BASE: usize = 0x4000_0000;
pub const STORAGE_LEN: usize = 0x4000;

pub const RSDP_OFFSET: usize = 0x0000;
pub const RSDT_OFFSET: usize = 0x0100;
pub const XSDT_OFFSET: usize = 0x0100;
pub const FADT_OFFSET: usize = 0x0400;
pub const DSDT_OFFSET: usize = 0x0800;
pub const SSDT_OFFSET: usize = 0x1000;
pub const SSDT_STRIDE: usize = 0x0100;

/// Empty AML body: `Scope (\_SB_) {}`.
const EMPTY_SCOPE: &[u8] = &[0x10, 0x06, 0x5c, b'_', b'S', b'B', b'_'];

struct HostState {
    storage: Vec<u64>,
    physical_base: usize,
    len: usize,
    failed: bool,
    failure: Option<HostFailure>,
    fail_ranges: Vec<(usize, usize)>,
    record_unsupported: bool,
    unsupported: Vec<u8>,
    mutexes: usize,
}

/// A deterministic AML host backed by test-owned storage.
#[derive(Clone)]
pub struct MockHost {
    state: Rc<RefCell<HostState>>,
}

impl MockHost {
    pub fn new(storage_len: usize, physical_base: usize) -> Self {
        let words = storage_len.div_ceil(8) + 1;
        Self {
            state: Rc::new(RefCell::new(HostState {
                storage: vec![0u64; words],
                physical_base,
                len: storage_len,
                failed: false,
                failure: None,
                fail_ranges: Vec::new(),
                record_unsupported: true,
                unsupported: Vec::new(),
                mutexes: 0,
            })),
        }
    }

    pub fn small() -> Self {
        Self::new(0x100, PHYSICAL_BASE)
    }

    pub fn base(&self) -> usize {
        self.state.borrow().physical_base
    }

    pub fn addr(&self, offset: usize) -> usize {
        self.base() + offset
    }

    pub fn write(&self, offset: usize, bytes: &[u8]) {
        let mut state = self.state.borrow_mut();
        let end = offset.checked_add(bytes.len()).unwrap();
        assert!(end <= state.len);
        let storage = state.storage.as_mut_ptr().cast::<u8>();
        // SAFETY: `storage` never grows after construction, so this pointer is
        // stable for the host lifetime; the range was checked above.
        let destination = unsafe { storage.add(offset) };
        // SAFETY: `destination` is a valid writable range of `bytes.len()`
        // bytes in the test storage and does not overlap `bytes`.
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), destination, bytes.len());
        }
    }

    pub fn write_u32(&self, offset: usize, value: u32) {
        self.write(offset, &value.to_le_bytes());
    }

    pub fn fail_map(&self, address: usize, len: usize) {
        self.state
            .borrow_mut()
            .fail_ranges
            .push((address, len));
    }

    pub fn record_failure(&self, access: HostAccessKind) {
        self.record(access, 0, 0, HostFailureReason::Unsupported);
    }

    pub fn set_record_unsupported(&self, enabled: bool) {
        self.state.borrow_mut().record_unsupported = enabled;
    }

    pub fn unsupported_regions(&self) -> Vec<u8> {
        self.state.borrow().unsupported.clone()
    }

    fn record(&self, access: HostAccessKind, address: u64, width: u8, reason: HostFailureReason) {
        let mut state = self.state.borrow_mut();
        if state.failed {
            return;
        }
        state.failure = Some(HostFailure::new(access, address, width, reason));
        state.failed = true;
    }

    fn failed(&self) -> bool {
        self.state.borrow().failed
    }

    fn map_error(&self, address: usize, reason: HostFailureReason) -> AcpiError {
        self.record(HostAccessKind::TableMap, address as u64, 0, reason);
        AcpiError::HostMappingFailed
    }

    fn check_map(&self, address: usize, size: usize) -> Result<usize, HostFailureReason> {
        let state = self.state.borrow();
        if state.failed {
            return Err(HostFailureReason::Io);
        }
        if size == 0 {
            return Err(HostFailureReason::OutOfRange);
        }
        let offset = address
            .checked_sub(state.physical_base)
            .ok_or(HostFailureReason::OutOfRange)?;
        let end = offset.checked_add(size).ok_or(HostFailureReason::Wrap)?;
        if end > state.len {
            return Err(HostFailureReason::OutOfRange);
        }
        for &(start, len) in &state.fail_ranges {
            let fail_end = start.saturating_add(len);
            if address < fail_end && address.saturating_add(size) > start {
                return Err(HostFailureReason::Io);
            }
        }
        Ok(offset)
    }

    fn storage_ptr(&self) -> NonNull<u8> {
        let pointer = self.state.borrow().storage.as_ptr() as *mut u8;
        NonNull::new(pointer).unwrap()
    }

    fn read_storage(&self, address: usize, width: usize) -> u64 {
        if self.failed() {
            return 0;
        }
        let Ok(offset) = self.check_map(address, width) else {
            return 0;
        };
        if !address.is_multiple_of(width) {
            return 0;
        }
        let pointer = self.storage_ptr().as_ptr();
        let mut value = 0u64;
        for index in 0..width {
            // SAFETY: `check_map` accepted `[offset, offset + width)` inside
            // the stable test storage.
            let source = unsafe { pointer.add(offset + index) };
            // SAFETY: `source` is an in-bounds, aligned byte in test storage.
            let byte = unsafe { core::ptr::read_volatile(source) };
            value |= u64::from(byte) << (8 * index);
        }
        value
    }
}

impl AmlHost for MockHost {
    fn take_failure(&self) -> Option<HostFailure> {
        if !self.failed() {
            return None;
        }
        self.state.borrow_mut().failure.take()
    }

    fn note_unsupported_region(&self, space: u8) {
        let mut state = self.state.borrow_mut();
        state.unsupported.push(space);
        if state.record_unsupported && !state.failed {
            state.failure = Some(HostFailure::new(
                HostAccessKind::UnsupportedRegion,
                u64::from(space),
                0,
                HostFailureReason::Unsupported,
            ));
            state.failed = true;
        }
    }
}

impl Handler for MockHost {
    unsafe fn map_physical_region<T>(
        &self,
        physical_address: usize,
        size: usize,
    ) -> Result<PhysicalMapping<Self, T>, AcpiError> {
        self.record(
            HostAccessKind::TableMap,
            physical_address as u64,
            0,
            HostFailureReason::NotPermitted,
        );
        let _ = size;
        Err(AcpiError::HostMappingFailed)
    }

    unsafe fn map_physical_region_read_only<T>(
        &self,
        physical_address: usize,
        size: usize,
    ) -> Result<ReadOnlyPhysicalMapping<Self, T>, AcpiError> {
        if size < size_of::<T>() {
            return Err(self.map_error(physical_address, HostFailureReason::OutOfRange));
        }
        let offset = match self.check_map(physical_address, size) {
            Ok(offset) => offset,
            Err(reason) => return Err(self.map_error(physical_address, reason)),
        };
        if !offset.is_multiple_of(core::mem::align_of::<T>()) {
            return Err(self.map_error(physical_address, HostFailureReason::OutOfRange));
        }
        let pointer = self.storage_ptr().as_ptr();
        // SAFETY: the storage never grows and `offset` is in-bounds.
        let element = unsafe { pointer.add(offset) };
        // SAFETY: `check_map` validated the range and alignment, so the
        // offset pointer is a valid, aligned `T`.
        let virtual_start = unsafe { NonNull::new_unchecked(element.cast::<T>()) };
        // SAFETY: `virtual_start` points inside a stable, readable region of
        // at least `size` bytes.
        Ok(unsafe {
            ReadOnlyPhysicalMapping::new(physical_address, virtual_start, size, size, self.clone())
        })
    }

    fn unmap_physical_region<T>(_: &PhysicalMapping<Self, T>) {}

    fn unmap_physical_region_read_only<T>(_: &ReadOnlyPhysicalMapping<Self, T>) {}

    fn read_u8(&self, address: usize) -> u8 {
        self.read_storage(address, 1) as u8
    }

    fn read_u16(&self, address: usize) -> u16 {
        self.read_storage(address, 2) as u16
    }

    fn read_u32(&self, address: usize) -> u32 {
        self.read_storage(address, 4) as u32
    }

    fn read_u64(&self, address: usize) -> u64 {
        self.read_storage(address, 8)
    }

    fn write_u8(&self, _address: usize, _value: u8) {}
    fn write_u16(&self, _address: usize, _value: u16) {}
    fn write_u32(&self, _address: usize, _value: u32) {}
    fn write_u64(&self, _address: usize, _value: u64) {}

    fn read_io_u8(&self, _port: u16) -> u8 {
        0
    }

    fn read_io_u16(&self, _port: u16) -> u16 {
        0
    }

    fn read_io_u32(&self, _port: u16) -> u32 {
        0
    }

    fn write_io_u8(&self, _port: u16, _value: u8) {}
    fn write_io_u16(&self, _port: u16, _value: u16) {}
    fn write_io_u32(&self, _port: u16, _value: u32) {}

    fn read_pci_u8(&self, _address: PciAddress, _offset: u16) -> u8 {
        0
    }

    fn read_pci_u16(&self, _address: PciAddress, _offset: u16) -> u16 {
        0
    }

    fn read_pci_u32(&self, _address: PciAddress, _offset: u16) -> u32 {
        0
    }

    fn write_pci_u8(&self, _address: PciAddress, _offset: u16, _value: u8) {}
    fn write_pci_u16(&self, _address: PciAddress, _offset: u16, _value: u16) {}
    fn write_pci_u32(&self, _address: PciAddress, _offset: u16, _value: u32) {}

    fn nanos_since_boot(&self) -> u64 {
        0
    }

    fn stall(&self, _microseconds: u64) {}

    fn sleep(&self, _milliseconds: u64) {}

    fn create_mutex(&self) -> Handle {
        let mut state = self.state.borrow_mut();
        let index = state.mutexes as u32;
        state.mutexes += 1;
        Handle(index)
    }

    fn acquire(&self, _mutex: Handle, _timeout_ms: u16) -> Result<(), AmlError> {
        Ok(())
    }

    fn release(&self, _mutex: Handle) {}
}

/// Builds an interpreter context over a host that maps no tables.
pub fn context_with_host(host: MockHost) -> AcpiContext<MockHost> {
    let gas = GenericAddress {
        address_space: AddressSpace::SystemIo,
        bit_width: 16,
        bit_offset: 0,
        access_size: 0,
        address: 0,
    };
    // SAFETY: a SystemIO GAS at port 0 does not map memory; the registers are
    // never accessed by these tests.
    let event = unsafe { MappedGas::map_gas(gas, &host).unwrap() };
    // SAFETY: as above.
    let control = unsafe { MappedGas::map_gas(gas, &host).unwrap() };
    let registers = FixedRegisters {
        pm1_event_registers: Pm1EventRegisterBlock {
            pm1_event_length: 0,
            pm1a: event,
            pm1b: None,
        },
        pm1_control_registers: Pm1ControlRegisterBlock {
            pm1a: control,
            pm1b: None,
        },
    };
    #[expect(
        clippy::arc_with_non_send_sync,
        reason = "the unittest harness is single-threaded, but MockHost keeps its state in Rc"
    )]
    let interpreter = Interpreter::new(host.clone(), 2, Arc::new(registers), None);
    AcpiContext::from_interpreter_for_test(host, interpreter)
}

pub fn test_context() -> AcpiContext<MockHost> {
    context_with_host(MockHost::small())
}

/// Builds a context from fixture tables, or observes the zero-address
/// sentinel being rejected before any mapping.
pub fn fixture_context(
    host: MockHost,
    rsdp: usize,
) -> Result<AcpiContext<MockHost>, ContextError> {
    // SAFETY: `host` is either an empty mock, where `0` is rejected before any
    // mapping, or `test_tables` storage containing a valid RSDP table set.
    unsafe { AcpiContext::from_rsdp(host, rsdp) }
}

pub fn add_device(ctx: &mut AcpiContext<MockHost>, path: &str) {
    let name = AmlName::from_str(path).unwrap();
    ctx.with_namespace_for_test(|namespace| {
        namespace
            .add_level(name, NamespaceLevelKind::Device)
            .unwrap();
    });
}

pub fn insert(ctx: &mut AcpiContext<MockHost>, path: &str, object: Object) {
    let name = AmlName::from_str(path).unwrap();
    ctx.with_namespace_for_test(|namespace| {
        namespace.insert(name, object.wrap()).unwrap();
    });
}

pub fn insert_native<F>(
    ctx: &mut AcpiContext<MockHost>,
    path: &str,
    args: u8,
    f: F,
) where
    F: Fn(&[WrappedObject]) -> Result<WrappedObject, AmlError> + 'static,
{
    insert(ctx, path, Object::native_method(args, f));
}

pub fn install_root(ctx: &mut AcpiContext<MockHost>, path: &str, id: Object) {
    add_device(ctx, path);
    insert(ctx, &format!("{path}._HID"), id);
}

pub fn integer(value: u64) -> Object {
    Object::Integer(value)
}

pub fn string(value: &str) -> Object {
    Object::String(value.to_string())
}

pub fn buffer(bytes: Vec<u8>) -> Object {
    Object::Buffer(bytes)
}

pub fn package(items: Vec<WrappedObject>) -> Object {
    Object::Package(items)
}

pub fn prt_entry(device: u8, function: u16, pin: u8, source: Object, index: u64) -> WrappedObject {
    let address = (u32::from(device) << 16) | u32::from(function);
    Object::Package(vec![
        Object::Integer(u64::from(address)).wrap(),
        Object::Integer(u64::from(pin)).wrap(),
        source.wrap(),
        Object::Integer(index).wrap(),
    ])
    .wrap()
}

pub fn prt_object(entries: Vec<WrappedObject>) -> Object {
    Object::Package(entries)
}

pub fn small_irq(gsi: u16, flags: Option<u8>) -> Vec<u8> {
    let mask = 1u16 << gsi;
    let mut bytes = vec![if flags.is_some() { 0x23 } else { 0x22 }];
    bytes.extend_from_slice(&mask.to_le_bytes());
    if let Some(flags) = flags {
        bytes.push(flags);
    }
    bytes.extend_from_slice(&[0x79, 0x00]);
    bytes
}

pub fn extended_irq(gsi: u32, flags: u8) -> Vec<u8> {
    let mut bytes = vec![0x89, 0x06, 0x00, flags, 0x01];
    bytes.extend_from_slice(&gsi.to_le_bytes());
    bytes.extend_from_slice(&[0x79, 0x00]);
    bytes
}

/// An extended IRQ with the optional `ResourceSource` suffix (index + name).
pub fn extended_irq_with_source(gsi: u32, flags: u8, index: u8, name: &[u8]) -> Vec<u8> {
    let body_len = 2 + 4 + 1 + name.len() + 1;
    let mut bytes = vec![0x89, (body_len & 0xff) as u8, (body_len >> 8) as u8, flags, 0x01];
    bytes.extend_from_slice(&gsi.to_le_bytes());
    bytes.push(index);
    bytes.extend_from_slice(name);
    bytes.push(0);
    bytes.extend_from_slice(&[0x79, 0x00]);
    bytes
}

pub fn bus_range_no_end(width: usize, min: u64, max: u64) -> Vec<u8> {
    // Large-item tags select the field width: 0x87 DWord, 0x88 Word,
    // 0x8A QWord.
    let header = match width {
        2 => 0x88,
        4 => 0x87,
        _ => 0x8A,
    };
    let len = 3 + 5 * width;
    let mut bytes = vec![header, (len & 0xff) as u8, (len >> 8) as u8];
    bytes.extend_from_slice(&[2, 0, 0]);
    // Granularity, minimum, maximum, translation, length.
    for value in [0, min, max, 0, max.wrapping_sub(min).wrapping_add(1)] {
        for index in 0..width {
            bytes.push((value >> (8 * index)) as u8);
        }
    }
    bytes
}

pub fn bus_range(width: usize, min: u64, max: u64) -> Vec<u8> {
    let mut bytes = bus_range_no_end(width, min, max);
    bytes.extend_from_slice(&[0x79, 0x00]);
    bytes
}

pub const END_TAG: [u8; 2] = [0x79, 0x00];

pub fn add_many_devices(ctx: &mut AcpiContext<MockHost>, count: usize) {
    const DIGITS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    for index in 0..count {
        let mut name = String::from("D");
        let mut value = index;
        for _ in 0..3 {
            name.push(DIGITS[value % DIGITS.len()] as char);
            value /= DIGITS.len();
        }
        add_device(ctx, &format!("\\_SB.{name}"));
    }
}

/// Test ACPI table set with a minimal DSDT and configurable SSDT count.
pub struct TestTables {
    pub host: MockHost,
    pub rsdp: usize,
    pub fadt: usize,
    pub dsdt: usize,
}

impl TestTables {
    pub fn context(&self) -> AcpiContext<MockHost> {
        // SAFETY: the fixture forms a valid RSDP/RSDT/FADT/DSDT set in the
        // mock host's own storage.
        unsafe { AcpiContext::from_rsdp(self.host.clone(), self.rsdp).unwrap() }
    }

    pub fn set_dsdt_length(&self, length: u32) {
        self.host.write_u32(DSDT_OFFSET + 4, length);
    }

    pub fn set_ssdt_length(&self, index: usize, length: u32) {
        self.host.write_u32(SSDT_OFFSET + index * SSDT_STRIDE + 4, length);
    }

    pub fn ssdt_offset(&self, index: usize) -> usize {
        SSDT_OFFSET + index * SSDT_STRIDE
    }

    /// Replaces the DSDT signature and recomputes its checksum, so a test can
    /// distinguish a signature failure from a checksum failure.
    pub fn set_dsdt_signature(&self, signature: &[u8; 4]) {
        self.host.write(DSDT_OFFSET, signature);
        finalize_table(&self.host, DSDT_OFFSET, 36 + EMPTY_SCOPE.len());
    }
}

pub fn test_tables(ssdt_count: usize) -> TestTables {
    test_tables_with_dsdt(ssdt_count, EMPTY_SCOPE)
}

pub fn test_tables_with_dsdt(ssdt_count: usize, dsdt_body: &[u8]) -> TestTables {
    build_tables(ssdt_count, dsdt_body, false)
}

/// Builds a revision-2 RSDP/XSDT table set. The XSDT header is 36 bytes, so
/// its 8-byte entries begin at a 4-byte-aligned offset, exercising the real
/// XSDT layout rather than the naturally aligned RSDT layout.
pub fn test_tables_xsdt(ssdt_count: usize) -> TestTables {
    build_tables(ssdt_count, EMPTY_SCOPE, true)
}

fn build_tables(ssdt_count: usize, dsdt_body: &[u8], xsdt: bool) -> TestTables {
    let host = MockHost::new(STORAGE_LEN, PHYSICAL_BASE);

    let mut rsdp = [0u8; 36];
    rsdp[0..8].copy_from_slice(b"RSD PTR ");
    rsdp[9..15].copy_from_slice(b"TEST  ");
    rsdp[15] = if xsdt { 2 } else { 0 };
    if xsdt {
        rsdp[20..24].copy_from_slice(&(36u32).to_le_bytes());
        rsdp[24..32].copy_from_slice(&(host.addr(XSDT_OFFSET) as u64).to_le_bytes());
    } else {
        rsdp[16..20].copy_from_slice(&(host.addr(RSDT_OFFSET) as u32).to_le_bytes());
    }
    let sum = rsdp[..20]
        .iter()
        .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
    rsdp[8] = 0u8.wrapping_sub(sum);
    if xsdt {
        // The extended checksum covers the whole 36-byte RSDP.
        let sum = rsdp.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte));
        rsdp[32] = 0u8.wrapping_sub(sum);
    }
    host.write(RSDP_OFFSET, &rsdp);

    let mut entries = vec![FADT_OFFSET];
    for index in 0..ssdt_count {
        entries.push(SSDT_OFFSET + index * SSDT_STRIDE);
    }
    let entry_size = if xsdt { 8 } else { 4 };
    let root_len = 36 + entries.len() * entry_size;
    let (root_offset, root_signature) =
        if xsdt { (XSDT_OFFSET, b"XSDT") } else { (RSDT_OFFSET, b"RSDT") };
    write_header(&host, root_offset, root_signature, root_len as u32, 1);
    for (index, entry) in entries.iter().enumerate() {
        let offset = root_offset + 36 + index * entry_size;
        if xsdt {
            host.write(offset, &(host.addr(*entry) as u64).to_le_bytes());
        } else {
            host.write_u32(offset, host.addr(*entry) as u32);
        }
    }
    finalize_table(&host, root_offset, root_len);

    write_header(&host, FADT_OFFSET, b"FACP", 0x200, 0);
    host.write_u32(FADT_OFFSET + 40, host.addr(DSDT_OFFSET) as u32);
    finalize_table(&host, FADT_OFFSET, 0x200);

    write_header(
        &host,
        DSDT_OFFSET,
        b"DSDT",
        (36 + dsdt_body.len()) as u32,
        2,
    );
    host.write(DSDT_OFFSET + 36, dsdt_body);
    finalize_table(&host, DSDT_OFFSET, 36 + dsdt_body.len());

    for index in 0..ssdt_count {
        let offset = SSDT_OFFSET + index * SSDT_STRIDE;
        write_header(&host, offset, b"SSDT", (36 + dsdt_body.len()) as u32, 2);
        host.write(offset + 36, dsdt_body);
        finalize_table(&host, offset, 36 + dsdt_body.len());
    }

    TestTables {
        rsdp: host.addr(RSDP_OFFSET),
        fadt: host.addr(FADT_OFFSET),
        dsdt: host.addr(DSDT_OFFSET),
        host,
    }
}

fn write_header(host: &MockHost, offset: usize, signature: &[u8; 4], length: u32, revision: u8) {
    host.write(offset, signature);
    host.write_u32(offset + 4, length);
    host.write(offset + 8, &[revision]);
}

/// Writes the checksum byte for a fully populated table.
fn finalize_table(host: &MockHost, offset: usize, length: usize) {
    host.write(offset + 9, &[0]);
    let sum = {
        let state = host.state.borrow();
        let pointer = state.storage.as_ptr().cast::<u8>();
        // SAFETY: fixture offsets and lengths are within the fixed test
        // storage, as checked by `MockHost::write` for the same range.
        let table_start = unsafe { pointer.add(offset) };
        // SAFETY: `table_start` points to `length` initialised bytes in the
        // stable storage allocation.
        let bytes = unsafe { core::slice::from_raw_parts(table_start, length) };
        bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte))
    };
    host.write(offset + 9, &[0u8.wrapping_sub(sum)]);
}

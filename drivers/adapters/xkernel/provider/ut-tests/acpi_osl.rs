// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
};

use acpi_rs::{
    AcpiError, PciAddress,
    aml::{AmlError, op_region::RegionSpace},
};
use driver_os_api::{PortOp, PortWidth, ResError, ResResult, TimeOp};
use khal::mem::VirtAddr;
use ktime_types::{MonotonicInstant, TimeSpan};
use pci::{Cam, PciConfigAccess};
use unittest::{assert, assert_eq, def_test};

use super::*;

const REGION: usize = 0x1000;
const TEST_REGIONS: usize = 10;
/// The classified test windows all live in this allocated, physically
/// contiguous, direct-mapped page run.
const RAM_BUF_LEN: usize = REGION * TEST_REGIONS;
/// Virtual base of the leaked test page run.
static RAM_VA: AtomicUsize = AtomicUsize::new(0);

/// Returns the test page run, allocating it once.
///
/// A `.bss` array is mapped only at its kernel-image alias, so `p2v(v2p(a))`
/// would fault. Allocator pages are mapped in the linear map; the allocation
/// is intentionally leaked because the static memory map outlives every test.
fn ram_va() -> usize {
    let existing = RAM_VA.load(Ordering::Acquire);
    if existing != 0 {
        return existing;
    }
    let va = kalloc::global_allocator()
        .alloc_pages(TEST_REGIONS, memaddr::PAGE_SIZE_4K, kalloc::UsageKind::Global)
        .expect("failed to allocate test RAM");
    // SAFETY: `alloc_pages` returned this many contiguous writable pages.
    unsafe { core::ptr::write_bytes(va as *mut u8, 0, RAM_BUF_LEN) };
    match RAM_VA.compare_exchange(0, va, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => va,
        Err(existing) => {
            kalloc::global_allocator().dealloc_pages(
                va,
                TEST_REGIONS,
                kalloc::UsageKind::Global,
            );
            existing
        }
    }
}

fn ram_words() -> &'static [AtomicU64] {
    // SAFETY: the leaked page run is writable, zero-initialized and owned by
    // the tests; its page-aligned address satisfies `AtomicU64` alignment.
    unsafe { core::slice::from_raw_parts(ram_va() as *const AtomicU64, RAM_BUF_LEN / 8) }
}

/// A memory map with one region per class, all backed by `RAM_BUF`.
struct TestMemoryMap;

static TEST_MEMORY: TestMemoryMap = TestMemoryMap;

// SAFETY: every accepted window is a sub-range of `RAM_BUF`, which is test-
// owned, direct-mapped, and readable and writable for the whole test. The
// `Acpi` and `FirmwareReserved` windows model boot-reserved memory that the
// allocator never receives; `FreeRam` and the other classes are deliberately
// refused by policy, and the classification table never changes.
unsafe impl AcpiMemoryMap for TestMemoryMap {
    fn classify_range(
        &self,
        start: usize,
        size: usize,
    ) -> Result<MemoryClass, MemoryRangeError> {
        if size == 0 {
            return Err(MemoryRangeError::Empty);
        }
        let end = start.checked_add(size).ok_or(MemoryRangeError::Wrap)?;

        let regions = [
            (test_ram(), test_ram() + REGION, MemoryClass::FreeRam),
            (test_acpi(), test_acpi() + REGION, MemoryClass::Acpi),
            (
                test_firmware(),
                test_firmware() + REGION,
                MemoryClass::FirmwareReserved,
            ),
            (test_unusable(), test_unusable() + REGION, MemoryClass::Unusable),
            (test_persistent(), test_persistent() + REGION, MemoryClass::Persistent),
            (test_platform(), test_platform() + REGION, MemoryClass::Platform),
            (test_device(), test_device() + REGION, MemoryClass::DevicePrivate),
            // A boundary that is not naturally aligned for an 8-byte access,
            // so a naturally aligned SystemMemory access can straddle it.
            (
                test_mixed(),
                test_mixed() + REGION - 4,
                MemoryClass::FreeRam,
            ),
            (
                test_mixed() + REGION - 4,
                test_mixed() + REGION,
                MemoryClass::Acpi,
            ),
        ];
        for (base, region_end, class) in regions {
            if start >= base && end <= region_end {
                return Ok(class);
            }
            if start < region_end && end > base {
                return Err(MemoryRangeError::Mixed);
            }
        }
        Err(MemoryRangeError::Unmapped)
    }
}

fn ram_phys() -> usize {
    khal::mem::v2p(VirtAddr::from_usize(ram_va())).as_usize()
}

fn test_base(index: usize) -> usize {
    ram_phys() + index * REGION
}

fn test_ram() -> usize {
    test_base(0)
}

fn test_acpi() -> usize {
    test_base(1)
}

fn test_firmware() -> usize {
    test_base(2)
}

fn test_unusable() -> usize {
    test_base(3)
}

fn test_persistent() -> usize {
    test_base(4)
}

fn test_platform() -> usize {
    test_base(5)
}

fn test_device() -> usize {
    test_base(6)
}

/// Inside the buffer but absent from the classification table.
fn test_unmapped() -> usize {
    test_base(7)
}

/// Start of the FreeRam/Acpi pair whose boundary is not 8-byte aligned.
fn test_mixed() -> usize {
    test_base(8)
}

struct TestTime;

impl TimeOp for TestTime {
    fn monotonic_time(&self) -> MonotonicInstant {
        static NOW_MS: AtomicU64 = AtomicU64::new(0);
        let now = NOW_MS.fetch_add(1, Ordering::Relaxed) + 1;
        MonotonicInstant::from_span_since_origin(TimeSpan::from_millis(now))
    }
}

static TEST_TIME: TestTime = TestTime;

static PORT_READS: AtomicUsize = AtomicUsize::new(0);
static PORT_WRITES: AtomicUsize = AtomicUsize::new(0);
static PORT_READ_VALUE: AtomicU32 = AtomicU32::new(0xabcd);
static PORT_LAST_WRITE: AtomicU32 = AtomicU32::new(0);
static PORT_FAILS: AtomicBool = AtomicBool::new(false);

struct TestPort;

impl PortOp for TestPort {
    fn read_port(&self, _port: u16, _width: PortWidth) -> ResResult<u32> {
        PORT_READS.fetch_add(1, Ordering::Relaxed);
        if PORT_FAILS.load(Ordering::Relaxed) {
            return Err(ResError::Io);
        }
        Ok(PORT_READ_VALUE.load(Ordering::Relaxed))
    }

    fn write_port(&self, _port: u16, _width: PortWidth, value: u32) -> ResResult<()> {
        PORT_WRITES.fetch_add(1, Ordering::Relaxed);
        PORT_LAST_WRITE.store(value, Ordering::Relaxed);
        Ok(())
    }
}

static TEST_PORT: TestPort = TestPort;

fn reset_port() {
    PORT_READS.store(0, Ordering::Relaxed);
    PORT_WRITES.store(0, Ordering::Relaxed);
    PORT_READ_VALUE.store(0xabcd, Ordering::Relaxed);
    PORT_LAST_WRITE.store(0, Ordering::Relaxed);
    PORT_FAILS.store(false, Ordering::Relaxed);
}

/// Serializes tests that share the global `TestPort` counters. The unittest
/// harness runs tests concurrently on several CPUs.
static PORT_TEST_LOCK: AtomicBool = AtomicBool::new(false);

struct PortTestGuard;

impl PortTestGuard {
    fn acquire() -> Self {
        while PORT_TEST_LOCK
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        Self
    }
}

impl Drop for PortTestGuard {
    fn drop(&mut self) {
        PORT_TEST_LOCK.store(false, Ordering::Release);
    }
}

struct UnsupportedPort;

impl PortOp for UnsupportedPort {
    fn read_port(&self, _port: u16, _width: PortWidth) -> ResResult<u32> {
        Err(ResError::Unsupported)
    }

    fn write_port(&self, _port: u16, _width: PortWidth, _value: u32) -> ResResult<()> {
        Err(ResError::Unsupported)
    }
}

static NO_PORT: UnsupportedPort = UnsupportedPort;

fn test_host() -> AcpiOs {
    AcpiOs::new(&TEST_MEMORY, &TEST_TIME, &TEST_PORT, None)
}

fn test_host_with_pci(pci: PciConfigAccess) -> AcpiOs {
    AcpiOs::new(&TEST_MEMORY, &TEST_TIME, &TEST_PORT, Some(pci))
}

fn map_table<const SIZE: usize>(host: &AcpiOs, address: usize) -> Result<(), AcpiError> {
    if SIZE == 0 {
        // SAFETY: `()` is zero-sized, so a zero-byte request satisfies the
        // mapping contract. The test never dereferences the result.
        unsafe { host.map_physical_region_read_only::<()>(address, 0) }.map(drop)
    } else {
        // SAFETY: every test window is backed by `RAM_BUF`, and the test never
        // dereferences the result. Rejected mappings return an error.
        unsafe { host.map_physical_region_read_only::<u32>(address, SIZE) }.map(drop)
    }
}

fn map_writable<const SIZE: usize>(host: &AcpiOs, address: usize) -> Result<(), AcpiError> {
    // SAFETY: every test window is backed by `RAM_BUF`, `SIZE` is non-zero for
    // the callers of this helper, and the test never dereferences the result.
    unsafe { host.map_physical_region::<u32>(address, SIZE) }.map(drop)
}

#[repr(C)]
struct SixBytes([u8; 6]);

#[def_test]
fn read_only_table_mapping_accepts_acpi_and_firmware_reserved_memory() {
    for address in [test_acpi(), test_firmware()] {
        let host = test_host();
        assert!(map_table::<16>(&host, address).is_ok());
        assert_eq!(host.take_failure(), None);
    }

    let host = test_host();
    assert!(map_table::<16>(&host, test_ram()).is_err());
    let failure = host.take_failure().unwrap();
    assert_eq!(failure.access, HostAccessKind::TableMap);
    assert_eq!(failure.reason, HostFailureReason::NotPermitted);
}

#[def_test]
fn writable_mapping_requires_writable_memory() {
    let accepted = test_host();
    assert!(map_writable::<16>(&accepted, test_acpi()).is_ok());
    assert_eq!(accepted.take_failure(), None);

    for address in [test_firmware(), test_ram()] {
        let host = test_host();
        assert!(map_writable::<16>(&host, address).is_err());
        let failure = host.take_failure().unwrap();
        assert_eq!(failure.access, HostAccessKind::TableMap);
        assert_eq!(failure.reason, HostFailureReason::NotPermitted);
    }
}

#[def_test]
fn writable_mapping_rejects_a_live_overlapping_object() {
    let host = test_host();
    // SAFETY: the window is backed by `RAM_BUF`; the test does not dereference
    // either mapping.
    let first = unsafe { host.map_physical_region::<u32>(test_acpi(), 16) }.unwrap();
    // SAFETY: as above; the host rejects the overlapping second mapping.
    let second = unsafe { host.map_physical_region::<u32>(test_acpi(), 16) };
    assert!(matches!(second, Err(AcpiError::HostMappingFailed)));
    let failure = host.take_failure().unwrap();
    assert_eq!(failure.access, HostAccessKind::TableMap);
    assert_eq!(failure.reason, HostFailureReason::NotPermitted);
    drop(first);
}

#[def_test]
fn writable_mapping_allows_distinct_objects_in_an_overlapping_window() {
    let host = test_host();
    // The vendor maps a whole GAS page as `u8`, then maps another register in
    // the same page. The `T` objects are distinct, so both mappings may live.
    // SAFETY: both windows are backed by `RAM_BUF`; no dereference.
    let first = unsafe { host.map_physical_region::<u32>(test_acpi(), 16) }.unwrap();
    // SAFETY: as above.
    let second = unsafe { host.map_physical_region::<u32>(test_acpi() + 4, 16) }.unwrap();
    assert_eq!(host.take_failure(), None);
    drop(second);
    drop(first);
}

#[def_test]
fn writable_mapping_rejects_a_straddling_object() {
    let host = test_host();
    // Two six-byte objects starting two bytes apart overlap even though the
    // requested windows are both valid.
    // SAFETY: both windows are backed by `RAM_BUF`; no dereference.
    let first = unsafe { host.map_physical_region::<SixBytes>(test_acpi(), 8) }.unwrap();
    // SAFETY: as above; the host rejects the straddling second object.
    let second = unsafe { host.map_physical_region::<SixBytes>(test_acpi() + 2, 8) };
    assert!(matches!(second, Err(AcpiError::HostMappingFailed)));
    let failure = host.take_failure().unwrap();
    assert_eq!(failure.reason, HostFailureReason::NotPermitted);
    drop(first);
}

#[def_test]
fn writable_mapping_releases_its_object_on_unmap() {
    let host = test_host();
    // SAFETY: the window is backed by `RAM_BUF`; no dereference.
    let first = unsafe { host.map_physical_region::<u32>(test_acpi(), 16) }.unwrap();
    drop(first);
    assert!(map_writable::<16>(&host, test_acpi()).is_ok());
    assert_eq!(host.take_failure(), None);
}

#[def_test]
fn writable_mapping_rejects_read_only_table_ranges() {
    let host = test_host();
    assert!(map_table::<16>(&host, test_acpi()).is_ok());
    assert!(map_writable::<16>(&host, test_acpi()).is_err());
    let failure = host.take_failure().unwrap();
    assert_eq!(failure.reason, HostFailureReason::NotPermitted);
}

#[def_test]
fn read_only_mapping_rejects_a_live_writable_object() {
    let host = test_host();
    // SAFETY: the window is backed by `RAM_BUF`; no dereference.
    let live = unsafe { host.map_physical_region::<u32>(test_acpi(), 16) }.unwrap();
    assert!(map_table::<16>(&host, test_acpi()).is_err());
    let failure = host.take_failure().unwrap();
    assert_eq!(failure.access, HostAccessKind::TableMap);
    assert_eq!(failure.reason, HostFailureReason::NotPermitted);
    drop(live);
}

#[def_test]
fn repeated_table_mappings_do_not_exhaust_the_range_table() {
    let host = test_host();
    // The vendor maps the same header and full table while enumerating; more
    // requests than `MAX_TABLE_RANGES` must still succeed because the range is
    // retained once.
    for _ in 0..(MAX_TABLE_RANGES + 32) {
        assert!(map_table::<16>(&host, test_acpi()).is_ok());
    }
    assert_eq!(host.take_failure(), None);
}

#[def_test]
fn read_only_table_mappings_may_overlap() {
    let host = test_host();
    // SAFETY: both windows are backed by `RAM_BUF`; the test does not
    // dereference the mappings.
    let header =
        unsafe { host.map_physical_region_read_only::<u32>(test_acpi(), 4) }.unwrap();
    // SAFETY: as above. The vendor keeps a header mapping alive while it maps
    // the full table, so the read-only contract must permit this overlap.
    let full =
        unsafe { host.map_physical_region_read_only::<u32>(test_acpi(), 16) }.unwrap();
    assert_eq!(header.physical_start(), full.physical_start());
    assert_eq!(host.take_failure(), None);
}

#[def_test]
fn system_memory_writes_into_read_only_table_ranges_are_rejected() {
    let host = test_host();
    assert!(map_table::<16>(&host, test_acpi()).is_ok());
    assert!(
        host.checked_system_memory(test_acpi() + 4, 4, false)
            .is_some()
    );
    assert!(
        host.checked_system_memory(test_acpi() + 4, 4, true)
            .is_none()
    );
    let failure = host.take_failure().unwrap();
    assert_eq!(failure.access, HostAccessKind::SystemMemory);
    assert_eq!(failure.reason, HostFailureReason::NotPermitted);
}

#[def_test]
fn failed_mapping_is_an_error_not_a_fake_mapping() {
    let host = test_host();
    assert!(matches!(
        map_table::<16>(&host, test_device()),
        Err(AcpiError::HostMappingFailed)
    ));
    assert_eq!(
        host.take_failure().unwrap().access,
        HostAccessKind::TableMap
    );

    // The gate stays closed after the first failure is drained, even for an
    // otherwise acceptable range.
    assert!(matches!(
        map_table::<16>(&host, test_acpi()),
        Err(AcpiError::HostMappingFailed)
    ));
    assert_eq!(host.take_failure(), None);
}

#[def_test]
fn table_mapping_rejects_reserved_categories() {
    for address in [
        test_ram(),
        test_unusable(),
        test_persistent(),
        test_platform(),
        test_device(),
    ] {
        let host = test_host();
        assert!(map_table::<16>(&host, address).is_err());
        let failure = host.take_failure().unwrap();
        assert_eq!(failure.access, HostAccessKind::TableMap);
        assert_eq!(failure.reason, HostFailureReason::NotPermitted);
    }
}

#[def_test]
fn table_mapping_rejects_mixed_wrap_zero_and_unmapped() {
    let mixed = test_host();
    assert!(map_table::<16>(&mixed, test_ram() + REGION - 8).is_err());
    let failure = mixed.take_failure().unwrap();
    assert_eq!(failure.reason, HostFailureReason::NotPermitted);

    let wrapped = test_host();
    assert!(map_table::<16>(&wrapped, usize::MAX - 4).is_err());
    let failure = wrapped.take_failure().unwrap();
    assert_eq!(failure.reason, HostFailureReason::Wrap);

    let empty = test_host();
    assert!(map_table::<0>(&empty, test_ram()).is_err());
    let failure = empty.take_failure().unwrap();
    assert_eq!(failure.reason, HostFailureReason::OutOfRange);

    let unmapped = test_host();
    assert!(map_table::<16>(&unmapped, test_unmapped()).is_err());
    let failure = unmapped.take_failure().unwrap();
    assert_eq!(failure.reason, HostFailureReason::OutOfRange);
}

#[def_test]
fn system_memory_validation_allows_only_reserved_acpi_memory() {
    let accepted = test_host();
    assert!(accepted.checked_system_memory(test_acpi(), 4, false).is_some());
    assert!(accepted.checked_system_memory(test_acpi(), 4, true).is_some());
    assert_eq!(accepted.take_failure(), None);

    for address in [
        test_ram(),
        test_firmware(),
        test_unusable(),
        test_persistent(),
        test_platform(),
        test_device(),
    ] {
        let host = test_host();
        assert!(host.checked_system_memory(address, 4, false).is_none());
        let failure = host.take_failure().unwrap();
        assert_eq!(failure.access, HostAccessKind::SystemMemory);
        assert_eq!(failure.reason, HostFailureReason::NotPermitted);
    }
}

#[def_test]
fn system_memory_validation_rejects_mixed_wrap_unaligned_empty_and_unmapped() {
    let mixed = test_host();
    assert!(
        mixed
            .checked_system_memory(test_mixed() + REGION - 8, 8, false)
            .is_none()
    );
    assert_eq!(
        mixed.take_failure().unwrap().reason,
        HostFailureReason::NotPermitted
    );

    let wrapped = test_host();
    assert!(
        wrapped
            .checked_system_memory(usize::MAX - 3, 4, false)
            .is_none()
    );
    assert_eq!(
        wrapped.take_failure().unwrap().reason,
        HostFailureReason::Wrap
    );

    let unaligned = test_host();
    assert!(unaligned.checked_system_memory(test_ram() + 1, 2, false).is_none());
    assert_eq!(
        unaligned.take_failure().unwrap().reason,
        HostFailureReason::OutOfRange
    );

    let empty = test_host();
    assert!(empty.checked_system_memory(test_ram(), 0, false).is_none());
    assert_eq!(
        empty.take_failure().unwrap().reason,
        HostFailureReason::OutOfRange
    );

    let unmapped = test_host();
    assert!(
        unmapped
            .checked_system_memory(test_unmapped(), 4, false)
            .is_none()
    );
    assert_eq!(
        unmapped.take_failure().unwrap().reason,
        HostFailureReason::OutOfRange
    );
}

#[def_test]
fn system_memory_reads_and_writes_reach_the_validated_acpi_window() {
    let words = ram_words();
    for word in words {
        word.store(0, Ordering::Relaxed);
    }
    words[REGION / 8].store(0x1122_3344, Ordering::Relaxed);

    let host = test_host();
    let address = test_acpi();
    assert_eq!(host.read_u32(address), 0x1122_3344);

    host.write_u32(address, 0xaabb_ccdd);
    assert_eq!(words[REGION / 8].load(Ordering::Relaxed), 0xaabb_ccdd);

    host.write_u8(address + 9, 0x7e);
    assert_eq!(host.read_u8(address + 9), 0x7e);

    host.write_u64(address, 0x0102_0304_0506_0708);
    assert_eq!(host.read_u64(address), 0x0102_0304_0506_0708);
    assert_eq!(host.take_failure(), None);
}

#[def_test]
fn system_io_uses_the_port_capability_and_checks_arithmetic() {
    let _guard = PortTestGuard::acquire();
    reset_port();
    PORT_READ_VALUE.store(0xbeef, Ordering::Relaxed);

    let host = test_host();
    assert_eq!(host.read_io_u16(0x80), 0xbeef);
    assert_eq!(PORT_READS.load(Ordering::Relaxed), 1);

    host.write_io_u16(0x81, 0x1234);
    assert_eq!(PORT_WRITES.load(Ordering::Relaxed), 1);
    assert_eq!(PORT_LAST_WRITE.load(Ordering::Relaxed), 0x1234);
    assert_eq!(host.take_failure(), None);

    assert_eq!(host.read_io_u16(0xffff), 0);
    let failure = host.take_failure().unwrap();
    assert_eq!(failure.access, HostAccessKind::SystemIo);
    assert_eq!(failure.reason, HostFailureReason::OutOfRange);
    assert_eq!(PORT_READS.load(Ordering::Relaxed), 1);
}

#[def_test]
fn unavailable_port_capability_records_a_failure() {
    let host = AcpiOs::new(&TEST_MEMORY, &TEST_TIME, &NO_PORT, None);
    assert_eq!(host.read_io_u8(0x80), 0);
    let failure = host.take_failure().unwrap();
    assert_eq!(failure.access, HostAccessKind::SystemIo);
    assert_eq!(failure.reason, HostFailureReason::Unsupported);
}

#[def_test]
fn failure_gate_blocks_later_port_access_and_keeps_the_first_failure() {
    let _guard = PortTestGuard::acquire();
    reset_port();
    PORT_FAILS.store(true, Ordering::Relaxed);

    let host = test_host();
    assert_eq!(host.read_io_u8(0x80), 0);
    assert_eq!(PORT_READS.load(Ordering::Relaxed), 1);

    PORT_FAILS.store(false, Ordering::Relaxed);
    PORT_READ_VALUE.store(0x55, Ordering::Relaxed);
    assert_eq!(host.read_io_u8(0x80), 0);
    host.write_io_u8(0x80, 0x55);

    assert_eq!(PORT_READS.load(Ordering::Relaxed), 1);
    assert_eq!(PORT_WRITES.load(Ordering::Relaxed), 0);
    let failure = host.take_failure().unwrap();
    assert_eq!(failure.reason, HostFailureReason::Io);
    assert_eq!(host.take_failure(), None);
}

#[def_test]
fn writes_before_a_failure_are_not_rolled_back() {
    let _guard = PortTestGuard::acquire();
    reset_port();
    let host = test_host();

    host.write_io_u8(0x60, 0x12);
    host.record_unsupported_region(RegionSpace::SmBus);
    host.write_io_u8(0x60, 0x34);

    assert_eq!(PORT_WRITES.load(Ordering::Relaxed), 1);
    assert_eq!(PORT_LAST_WRITE.load(Ordering::Relaxed), 0x12);
    let failure = host.take_failure().unwrap();
    assert_eq!(failure.access, HostAccessKind::UnsupportedRegion);
    assert_eq!(failure.reason, HostFailureReason::Unsupported);
}

#[def_test]
fn unsupported_region_is_recorded_from_the_context_boundary() {
    let host = test_host();
    host.record_unsupported_region(RegionSpace::EmbeddedControl);
    let failure = host.take_failure().unwrap();
    assert_eq!(failure.access, HostAccessKind::UnsupportedRegion);
    assert_eq!(failure.address, 3);
}

#[repr(align(4096))]
struct PciWindow(UnsafeCell<[u8; 256]>);

// SAFETY: the tests access disjoint bytes through volatile reads and writes.
unsafe impl Sync for PciWindow {}

static PCI_WINDOW: PciWindow = PciWindow(UnsafeCell::new([0x5a; 256]));

fn pci_access() -> PciConfigAccess {
    // SAFETY: the static window is 4096-byte aligned, lives for the program,
    // and the bound matches the backing array.
    unsafe {
        PciConfigAccess::new_bounded(PCI_WINDOW.0.get().cast::<u8>(), Cam::Ecam, 256)
    }
}

#[def_test]
fn pci_config_uses_the_checked_accessor() {
    let address = PciAddress::new(0, 0, 0, 0);

    let read_host = test_host_with_pci(pci_access());
    assert_eq!(read_host.read_pci_u8(address, 0x10), 0x5a);
    assert_eq!(read_host.take_failure(), None);

    let write_host = test_host_with_pci(pci_access());
    write_host.write_pci_u8(address, 0x20, 0x77);
    // SAFETY: offset 0x20 is inside the 256-byte static window. The read is
    // not concurrent with another access to the same byte.
    let pointer = unsafe { PCI_WINDOW.0.get().cast::<u8>().add(0x20) };
    // SAFETY: `pointer` points at an initialized byte in the static window.
    let written = unsafe { pointer.read_volatile() };
    assert_eq!(written, 0x77);
    assert_eq!(write_host.take_failure(), None);
}

#[def_test]
fn pci_config_failures_record_host_failure() {
    let address = PciAddress::new(0, 0, 0, 0);

    let out_of_window = test_host_with_pci(pci_access());
    assert_eq!(out_of_window.read_pci_u32(address, 0x100), 0);
    let failure = out_of_window.take_failure().unwrap();
    assert_eq!(failure.access, HostAccessKind::PciConfig);
    assert_eq!(failure.reason, HostFailureReason::PciAccess);

    let missing = test_host();
    assert_eq!(missing.read_pci_u8(address, 0), 0);
    let failure = missing.take_failure().unwrap();
    assert_eq!(failure.reason, HostFailureReason::Unsupported);

    let other_segment = test_host_with_pci(pci_access());
    let segmented = PciAddress::new(1, 0, 0, 0);
    assert_eq!(other_segment.read_pci_u8(segmented, 0), 0);
    let failure = other_segment.take_failure().unwrap();
    assert_eq!(failure.access, HostAccessKind::PciConfig);
    assert_eq!(failure.reason, HostFailureReason::Unsupported);
}

#[def_test]
fn pci_failure_gate_blocks_later_port_access() {
    let _guard = PortTestGuard::acquire();
    reset_port();
    let host = test_host_with_pci(pci_access());
    assert_eq!(host.read_pci_u32(PciAddress::new(0, 0, 0, 0), 0x100), 0);
    assert_eq!(host.read_io_u8(0x80), 0);
    assert_eq!(PORT_READS.load(Ordering::Relaxed), 0);
    assert_eq!(
        host.take_failure().unwrap().reason,
        HostFailureReason::PciAccess
    );
}

#[def_test]
fn sleep_is_unsupported_and_records_a_failure() {
    let host = test_host();
    host.sleep(5);
    let failure = host.take_failure().unwrap();
    assert_eq!(failure.access, HostAccessKind::Sleep);
    assert_eq!(failure.reason, HostFailureReason::Unsupported);
}

#[def_test]
fn stall_is_bounded_by_monotonic_time() {
    let short = test_host();
    short.stall(100);
    assert_eq!(short.take_failure(), None);

    let long = test_host();
    long.stall(MAX_STALL_US + 1);
    let failure = long.take_failure().unwrap();
    assert_eq!(failure.access, HostAccessKind::Stall);
    assert_eq!(failure.reason, HostFailureReason::OutOfRange);
}

#[def_test]
fn mutex_acquire_is_bounded_and_timeout_records_a_failure() {
    let host = test_host();
    let mutex = host.create_mutex();
    assert!(mutex.0 < MAX_MUTEXES as u32);
    assert_eq!(host.acquire(mutex, 0), Ok(()));
    assert_eq!(
        host.acquire(mutex, 5),
        Err(AmlError::MutexAcquireTimeout)
    );
    let failure = host.take_failure().unwrap();
    assert_eq!(failure.access, HostAccessKind::Mutex);
    assert_eq!(failure.reason, HostFailureReason::Timeout);

    host.release(mutex);
}

#[def_test]
fn mutex_allocation_is_bounded() {
    let host = test_host();
    for index in 0..MAX_MUTEXES {
        assert_eq!(host.create_mutex().0, index as u32);
    }
    assert_eq!(host.create_mutex().0, u32::MAX);
    let failure = host.take_failure().unwrap();
    assert_eq!(failure.access, HostAccessKind::Mutex);
    assert_eq!(failure.reason, HostFailureReason::Allocation);
}

#[def_test]
fn a_context_like_check_rejects_results_after_failure() {
    fn accept_after_check(host: &AcpiOs, value: u32) -> Option<u32> {
        if host.take_failure().is_some() {
            return None;
        }
        Some(value)
    }

    let host = test_host();
    assert_eq!(accept_after_check(&host, 7), Some(7));

    host.sleep(1);
    assert_eq!(accept_after_check(&host, 0), None);
}

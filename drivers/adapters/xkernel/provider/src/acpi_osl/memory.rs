// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Memory-category validation for the ACPI host adapter.
//!
//! AML table loading and SystemMemory operation regions may only touch memory
//! the boot firmware described with a category this host can reason about. The
//! adapter reuses the kernel's physical memory map rather than trusting a
//! firmware-supplied range.

use khal::mem::{
    MemFlags, ReservedKind, memory_regions, reserved::describe_reserved_memory_region,
};

/// A physical-memory category accepted by the ACPI host adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryClass {
    /// Free RAM owned by the kernel allocator after boot. This class never
    /// authorizes firmware access: the range may already hold kernel objects
    /// or be handed out later.
    FreeRam,
    /// ACPI reclaimable or NVS memory. The boot map keeps this range out of the
    /// allocator, so the adapter may read and write it for the lifetime of the
    /// host.
    Acpi,
    /// Firmware-reserved memory. The boot map keeps this range out of the
    /// allocator; the adapter may map it to read ACPI tables, but never writes
    /// it or exposes it as SystemMemory.
    FirmwareReserved,
    /// Firmware marked the memory unusable.
    Unusable,
    /// Persistent-memory reservation.
    Persistent,
    /// Platform-private reservation.
    Platform,
    /// Device-private reservation.
    DevicePrivate,
    /// Any other reservation the adapter does not accept.
    Other,
}

/// Why a range could not be classified as one permitted memory category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryRangeError {
    /// The size is zero.
    Empty,
    /// The end of the range overflows the address space.
    Wrap,
    /// No described memory region covers the range.
    Unmapped,
    /// The range crosses a region or category boundary.
    Mixed,
}

/// Physical-memory description used by the ACPI host adapter.
///
/// Implementations must treat firmware descriptions as untrusted input and
/// return an error for any range that does not fit entirely inside one region.
///
/// # Safety
///
/// Classification is the adapter's authorization to touch physical memory.
/// Only [`MemoryClass::Acpi`] and [`MemoryClass::FirmwareReserved`] authorize
/// access, and the implementation must guarantee that for every range it
/// returns as one of those classes:
///
/// - the boot memory map reserves the entire range and never returns it to the
///   allocator for as long as the host adapter is alive; firmware does not own
///   or reuse the range behind the kernel's back;
/// - [`khal::mem::p2v`] yields a usable pointer for every byte in the range;
/// - [`MemoryClass::Acpi`] is readable and writable, and
///   [`MemoryClass::FirmwareReserved`] is readable;
/// - the range is not MMIO, device memory, or memory the kernel unmaps,
///   reallocates, or otherwise restricts after classification.
///
/// [`MemoryClass::FreeRam`] explicitly does **not** authorize access: free RAM
/// is owned by the kernel allocator after boot and may already contain live
/// objects. Other classes also do not authorize any access. Classification
/// must be side-effect free and safe to call concurrently.
pub unsafe trait AcpiMemoryMap: Sync {
    /// Classifies `[start, start + size)` as exactly one memory category.
    fn classify_range(&self, start: usize, size: usize) -> Result<MemoryClass, MemoryRangeError>;
}

/// The kernel's boot-firmware physical memory map.
#[derive(Clone, Copy, Debug, Default)]
pub struct KernelMemoryMap;

// SAFETY: the kernel memory map is immutable after boot, and its reserved
// regions are subtracted from free RAM before the allocator is initialized.
// `ReservedKind::Acpi` (ACPI reclaim/NVS) and `ReservedKind::Firmware` are
// therefore never handed to the allocator and stay direct-mapped for the
// kernel's lifetime; ACPI memory is writable and firmware-reserved memory is
// read-only. Free RAM is reported as `FreeRam`, which the adapter refuses, and
// device-private, platform, persistent, unusable, and unknown regions are not
// accepted either.
unsafe impl AcpiMemoryMap for KernelMemoryMap {
    fn classify_range(&self, start: usize, size: usize) -> Result<MemoryClass, MemoryRangeError> {
        if size == 0 {
            return Err(MemoryRangeError::Empty);
        }
        let end = start.checked_add(size).ok_or(MemoryRangeError::Wrap)?;

        for region in memory_regions() {
            let region_start = region.paddr.as_usize();
            let region_end = region_start
                .checked_add(region.size)
                .ok_or(MemoryRangeError::Wrap)?;

            if start >= region_start && end <= region_end {
                return Ok(classify_region(&region));
            }
            if start < region_end && end > region_start {
                return Err(MemoryRangeError::Mixed);
            }
        }
        Err(MemoryRangeError::Unmapped)
    }
}

fn classify_region(region: &khal::mem::MemoryRegion) -> MemoryClass {
    if region.flags.contains(MemFlags::FREE) {
        return MemoryClass::FreeRam;
    }

    match describe_reserved_memory_region(region).map(|reserved| reserved.kind) {
        Some(ReservedKind::Acpi) => MemoryClass::Acpi,
        Some(ReservedKind::Firmware) => MemoryClass::FirmwareReserved,
        Some(ReservedKind::Unusable) => MemoryClass::Unusable,
        Some(ReservedKind::Persistent) => MemoryClass::Persistent,
        Some(ReservedKind::Platform) => MemoryClass::Platform,
        Some(ReservedKind::DevicePrivate) => MemoryClass::DevicePrivate,
        _ => MemoryClass::Other,
    }
}

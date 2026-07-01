// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Structures and functions for PCI bus operations.
//!
//! Currently, it just re-exports structures from the crate [virtio-drivers][1]
//! and its module [`virtio_drivers::transport::pci::bus`][2].
//!
//! [1]: https://docs.rs/virtio-drivers/latest/virtio_drivers/
//! [2]: https://docs.rs/virtio-drivers/latest/virtio_drivers/transport/pci/bus/index.html

#![no_std]

use core::{
    ptr::NonNull,
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
};

use khal::firmware::devices as fw;
// Re-export the OS-neutral firmware interrupt descriptor types so downstream
// crates (e.g. virtio) can consume them without taking a direct `khal`
// dependency.
pub use khal::firmware::devices::{InterruptControllerKind, InterruptTrigger};
pub use virtio_drivers::transport::pci::bus::{
    BarInfo, Cam, CapabilityInfo, Command, ConfigurationAccess, DeviceFunction, DeviceFunctionInfo,
    HeaderType, MemoryBarType, MmioCam, PciError, PciRoot, Status,
};

pub mod msix;

static PCI_CONFIG_BASE_OVERRIDE: AtomicU64 = AtomicU64::new(0);
static PCI_BUS_END_OVERRIDE: AtomicU32 = AtomicU32::new(u32::MAX);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PciConfigSource {
    Static,
    Firmware,
    RuntimeOverride,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PciInitError {
    NoMemory,
    InvalidRange,
    MappingFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegacyInterruptRoute {
    pub irq: usize,
    pub trigger: fw::InterruptTrigger,
    pub controller: fw::InterruptControllerKind,
}

impl PciInitError {
    fn from_iomap(err: memspace::IoMapError) -> Self {
        match err {
            memspace::IoMapError::NoMemory => Self::NoMemory,
            memspace::IoMapError::InvalidRange => Self::InvalidRange,
            memspace::IoMapError::MappingFailed => Self::MappingFailed,
        }
    }
}

pub fn set_pci_config_space(base: u64, bus_end: u8) {
    PCI_CONFIG_BASE_OVERRIDE.store(base, Ordering::Relaxed);
    PCI_BUS_END_OVERRIDE.store(bus_end as u32, Ordering::Relaxed);
}

pub fn pci_config_space() -> (u64, u8, PciConfigSource) {
    let base = PCI_CONFIG_BASE_OVERRIDE.load(Ordering::Relaxed);
    let bus_end = PCI_BUS_END_OVERRIDE.load(Ordering::Relaxed);
    if base != 0 && bus_end != u32::MAX {
        log::info!(
            "PCI config from runtime override: ecam={:#x} bus_end={:#x}",
            base,
            bus_end
        );
        return (base, bus_end as u8, PciConfigSource::RuntimeOverride);
    }

    if let Some(host) = fw::pci_host()
        && host.bus_start == 0
    {
        log::info!(
            "PCI host from firmware: cam={:?} ecam={:#x} size={:#x} bus_range={:#x}..={:#x}",
            host.cam,
            host.ecam_base,
            host.ecam_size,
            host.bus_start,
            host.bus_end
        );
        return (host.ecam_base, host.bus_end, PciConfigSource::Firmware);
    }

    log::info!(
        "PCI config from kbuild: ecam={:#x} bus_end={:#x}",
        kbuild_config::PCI_ECAM_BASE as u64,
        kbuild_config::PCI_BUS_END as u8
    );
    (
        kbuild_config::PCI_ECAM_BASE as u64,
        kbuild_config::PCI_BUS_END as u8,
        PciConfigSource::Static,
    )
}

pub fn pci_ecam_size(bus_end: u8, cam: Cam) -> usize {
    let buses = bus_end as usize + 1;
    let bus_stride = match cam {
        Cam::MmioCam => 1usize << 16,
        Cam::Ecam => 1usize << 20,
    };
    buses * bus_stride
}

pub fn iomap_mmio(
    paddr: usize,
    size: usize,
    name: &'static str,
) -> Result<NonNull<u8>, PciInitError> {
    let vaddr = memspace::iomap_device(paddr.into(), size, name).map_err(|err| {
        log::warn!(
            "failed to iomap {name} at [PA:{:#x}, PA:{:#x}): {:?}",
            paddr,
            paddr.saturating_add(size),
            err
        );
        PciInitError::from_iomap(err)
    })?;
    NonNull::new(vaddr.as_mut_ptr()).ok_or(PciInitError::MappingFailed)
}

pub struct PciBus {
    root: PciRoot<MmioCam<'static>>,
    config: PciConfigAccess,
    config_base: u64,
    bus_end: u8,
    source: PciConfigSource,
}

impl PciBus {
    pub fn new(cam: Cam) -> Result<Self, PciInitError> {
        let (config_base, bus_end, source) = pci_config_space();
        if source == PciConfigSource::Firmware
            && let Some(host) = fw::pci_host()
        {
            let expected = match cam {
                Cam::MmioCam => fw::PciHostCam::Cam,
                Cam::Ecam => fw::PciHostCam::Ecam,
            };
            if host.cam != expected {
                log::warn!(
                    "PCI firmware host advertises {:?} but build selected {:?}; falling back to \
                     static config",
                    host.cam,
                    cam
                );
                return Self::new_static(cam);
            }
            log::info!(
                "PCI bus using firmware host: cam={:?} ecam={:#x} bus_end={:#x}",
                cam,
                config_base,
                bus_end
            );
        }
        let ecam_size = pci_ecam_size(bus_end, cam);
        let base_vaddr = iomap_mmio(config_base as usize, ecam_size, "pci-ecam")?;
        // SAFETY: `base_vaddr` maps the entire ECAM window described by `cam`.
        let mmio_cam = unsafe { MmioCam::new(base_vaddr.as_ptr(), cam) };
        let root = PciRoot::new(mmio_cam);
        // SAFETY: `base_vaddr` maps the entire ECAM window described by `cam`.
        let config = unsafe { PciConfigAccess::new(base_vaddr.as_ptr(), cam) };
        Ok(Self {
            root,
            config,
            config_base,
            bus_end,
            source,
        })
    }

    fn new_static(cam: Cam) -> Result<Self, PciInitError> {
        let config_base = kbuild_config::PCI_ECAM_BASE as u64;
        let bus_end = kbuild_config::PCI_BUS_END as u8;
        let ecam_size = pci_ecam_size(bus_end, cam);
        let base_vaddr = iomap_mmio(config_base as usize, ecam_size, "pci-ecam")?;
        // SAFETY: `base_vaddr` maps the entire static ECAM window described by `cam`.
        let mmio_cam = unsafe { MmioCam::new(base_vaddr.as_ptr(), cam) };
        let root = PciRoot::new(mmio_cam);
        // SAFETY: `base_vaddr` maps the entire static ECAM window described by `cam`.
        let config = unsafe { PciConfigAccess::new(base_vaddr.as_ptr(), cam) };
        Ok(Self {
            root,
            config,
            config_base,
            bus_end,
            source: PciConfigSource::Static,
        })
    }

    pub fn config_base(&self) -> u64 {
        self.config_base
    }

    pub fn bus_end(&self) -> u8 {
        self.bus_end
    }

    pub fn source(&self) -> PciConfigSource {
        self.source
    }

    pub fn parts_mut(&mut self) -> (&mut PciRoot<MmioCam<'static>>, &mut PciConfigAccess) {
        (&mut self.root, &mut self.config)
    }
}

pub fn pci_bar_allocation_range() -> Option<(u64, u64, PciConfigSource)> {
    if let Some(range) = fw::pci_bar_range() {
        log::info!(
            "PCI BAR allocator from firmware: cpu_base={:#x} size={:#x} prefetchable={}",
            range.cpu_base,
            range.size,
            range.prefetchable
        );
        return Some((range.cpu_base, range.size, PciConfigSource::Firmware));
    }

    let range = kbuild_config::PCI_RANGES
        .get(1)
        .copied()
        .map(|(start, len)| (start, len, PciConfigSource::Static));
    if let Some((start, len, _)) = range {
        log::info!(
            "PCI BAR allocator from kbuild: cpu_base={:#x} size={:#x}",
            start,
            len
        );
    }
    range
}

pub fn legacy_interrupt_route(
    config: &PciConfigAccess,
    bdf: DeviceFunction,
) -> Option<LegacyInterruptRoute> {
    let word = config.read_word(bdf, 0x3C);
    let pin = ((word >> 8) & 0xff) as u8;
    if pin == 0 || pin == 0xff {
        log::info!(
            "PCI legacy IRQ route: bdf={:?} pin not programmed (pin={:#x})",
            bdf,
            pin
        );
        return None;
    }
    let irq = fw::pci_legacy_irq(bdf.bus, bdf.device, bdf.function, pin)?;
    log::info!(
        "PCI legacy IRQ from firmware: bdf={:?} pin=INT{} irq={} trigger={:?} controller={:?}",
        bdf,
        pin,
        irq.irq,
        irq.trigger,
        irq.controller
    );
    Some(LegacyInterruptRoute {
        irq: irq.irq,
        trigger: irq.trigger,
        controller: irq.controller,
    })
}

/// Provides read/write access to PCI configuration space registers.
///
/// The `virtio-drivers` crate exposes `PciRoot` but keeps its internal
/// `config_read_word` / `config_write_word` methods `pub(crate)`. This type
/// re-implements the same memory-mapped access so that callers outside of
/// `virtio-drivers` (e.g. the MSI-X subsystem) can also read/write arbitrary
/// configuration space registers.
///
/// Create one from the same MMIO base pointer and [`Cam`] type used for
/// [`PciRoot`]. Both types reference the same underlying memory region.
#[derive(Clone, Copy)]
pub struct PciConfigAccess {
    mmio_base: usize,
    cam: Cam,
}

impl PciConfigAccess {
    /// Creates a new `PciConfigAccess` from a raw MMIO base and CAM type.
    ///
    /// # Safety
    ///
    /// `mmio_base` must meet the same requirements as for [`PciRoot::new`]:
    /// it must be a valid, 4-byte-aligned, appropriately-mapped MMIO pointer
    /// valid for the lifetime of the program. The mapped ECAM window must be
    /// large enough for every later BDF/register access performed through this
    /// wrapper, and no other abstraction may violate MMIO aliasing rules for
    /// the same config window.
    pub unsafe fn new(mmio_base: *mut u8, cam: Cam) -> Self {
        assert!(mmio_base as usize & 0x3 == 0);
        Self {
            mmio_base: mmio_base as usize,
            cam,
        }
    }

    /// Computes the byte offset of a register within the flat MMIO CAM window.
    fn cam_offset(&self, device_function: DeviceFunction, register_offset: u16) -> u32 {
        let bdf = (device_function.bus as u32) << 8
            | (device_function.device as u32) << 3
            | device_function.function as u32;
        let shift = match self.cam {
            Cam::MmioCam => 8,
            Cam::Ecam => 12,
        };
        (bdf << shift) | (register_offset as u32 & !0x3)
    }

    /// Reads a 32-bit word from PCI configuration space.
    ///
    /// `register_offset` is the byte offset of the register; the two LSBs are
    /// ignored (the access is always 32-bit aligned).
    pub fn read_word(&self, device_function: DeviceFunction, register_offset: u16) -> u32 {
        let address = self.cam_offset(device_function, register_offset);
        // SAFETY: The pointer arithmetic stays within the MMIO window because
        // cam_offset() produces offsets bounded by Cam::size().
        unsafe {
            (self.mmio_base as *mut u32)
                .add((address >> 2) as usize)
                .read_volatile()
        }
    }

    /// Writes a 32-bit word to PCI configuration space.
    ///
    /// `register_offset` is the byte offset; the two LSBs are ignored.
    pub fn write_word(&mut self, device_function: DeviceFunction, register_offset: u16, data: u32) {
        let address = self.cam_offset(device_function, register_offset);
        // SAFETY: Same as read_word.
        unsafe {
            (self.mmio_base as *mut u32)
                .add((address >> 2) as usize)
                .write_volatile(data);
        }
    }
}

/// Sequential allocator for MMIO address space used by PCI Base Address Registers.
///
/// This allocator manages a contiguous range of physical addresses and dispenses
/// aligned chunks suitable for memory-mapped I/O regions. Each allocation must
/// be power-of-two sized and will be naturally aligned to that size.
pub struct PciRangeAllocator {
    _begin_addr: u64,
    end_addr: u64,
    cursor_addr: u64,
}

pub fn configure_device<C: ConfigurationAccess>(
    root: &mut PciRoot<C>,
    bdf: DeviceFunction,
    allocator: &mut Option<PciRangeAllocator>,
) -> Result<(), PciInitError> {
    let mut bar = 0;
    while bar < 6 {
        let info = match root.bar_info(bdf, bar).unwrap() {
            Some(info) => info,
            None => {
                bar += 1;
                continue;
            }
        };

        if let BarInfo::Memory {
            address_type,
            address,
            size,
            ..
        } = info
            && size > 0
            && address == 0
        {
            let new_addr = allocator
                .as_mut()
                .expect("No memory ranges available for PCI BARs!")
                .alloc_buf(size as _)
                .ok_or(PciInitError::NoMemory)?;
            if address_type == MemoryBarType::Width32 {
                root.set_bar_32(bdf, bar, new_addr as _);
            } else if address_type == MemoryBarType::Width64 {
                root.set_bar_64(bdf, bar, new_addr);
            }
        }

        let info = match root.bar_info(bdf, bar).unwrap() {
            Some(info) => info,
            None => {
                bar += 1;
                continue;
            }
        };
        let takes_two = info.takes_two_entries();
        match info {
            BarInfo::IO { address, size } => {
                if address > 0 && size > 0 {
                    log::debug!("  BAR {}: IO  [{:#x}, {:#x})", bar, address, address + size);
                }
            }
            BarInfo::Memory {
                address_type,
                prefetchable,
                address,
                size,
            } => {
                if address > 0 && size > 0 {
                    log::debug!(
                        "  BAR {}: MEM [{:#x}, {:#x}){}{}",
                        bar,
                        address,
                        address + size,
                        if address_type == MemoryBarType::Width64 {
                            " 64bit"
                        } else {
                            ""
                        },
                        if prefetchable { " pref" } else { "" },
                    );
                }
            }
        }

        bar += 1;
        if takes_two {
            bar += 1;
        }
    }

    let (_status, cmd) = root.get_status_command(bdf);
    root.set_command(
        bdf,
        cmd | Command::IO_SPACE | Command::MEMORY_SPACE | Command::BUS_MASTER,
    );
    Ok(())
}

impl PciRangeAllocator {
    /// Constructs a new allocator managing the specified address range.
    ///
    /// # Arguments
    ///
    /// * `start` - The starting physical address of the MMIO region
    /// * `length` - The total size in bytes of the allocatable region
    pub const fn new(start: u64, length: u64) -> Self {
        Self {
            _begin_addr: start,
            end_addr: start.saturating_add(length),
            cursor_addr: start,
        }
    }

    /// Attempts to allocate an aligned MMIO region of the requested size.
    ///
    /// # Arguments
    ///
    /// * `length` - The requested allocation size. Must be a power of two.
    ///
    /// # Returns
    ///
    /// * `Some(address)` - The base address of the allocated region, aligned to `length`
    /// * `None` - If `length` is not a power of two or insufficient space remains
    ///
    /// # Alignment guarantee
    ///
    /// The returned address is guaranteed to be a multiple of `length`, ensuring
    /// natural alignment for the allocated region.
    pub fn alloc_buf(&mut self, length: u64) -> Option<u64> {
        // Validate that length is a power of two (required for alignment)
        if length == 0 || (length & (length.wrapping_sub(1))) != 0 {
            return None;
        }

        // Calculate the aligned starting address for this allocation
        let aligned_addr = self.compute_aligned_address(length)?;

        // Verify that the full allocation fits within our managed range
        let allocation_end = aligned_addr.checked_add(length)?;
        if allocation_end > self.end_addr {
            return None;
        }

        // Commit the allocation by advancing our free pointer
        self.cursor_addr = allocation_end;
        Some(aligned_addr)
    }

    /// Computes the next address aligned to the given boundary.
    ///
    /// Returns `None` if alignment calculation would overflow.
    fn compute_aligned_address(&self, alignment: u64) -> Option<u64> {
        // Calculate the alignment mask (e.g., for alignment=4096, mask=4095)
        let alignment_mask = alignment.wrapping_sub(1);

        // Apply ceiling alignment: (addr + mask) & ~mask
        let adjusted = self.cursor_addr.checked_add(alignment_mask)?;
        Some(adjusted & !alignment_mask)
    }
}

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PciConfigSource {
    Static,
    Firmware,
    RuntimeOverride,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PciInitError {
    NoMemory,
    InvalidRange,
    MappingFailed,
    /// The device exposes an invalid PCI BAR encoding.
    InvalidBar,
}

/// Firmware routing for one legacy PCI INTx line.
///
/// The variant records what kind of number firmware produced. ACPI names a
/// platform-wide GSI that only the x86 IRQ integration can resolve; device
/// tree compatibility routes name a controller-local hardware IRQ.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegacyInterruptRoute {
    /// ACPI GSI. The x86 platform IRQ integration resolves it.
    Gsi {
        gsi: u32,
        trigger: fw::InterruptTrigger,
    },
    /// DT compatibility route: a controller-local hardware number.
    /// This variant is not a neutral contract and will shrink with OF.
    Hwirq {
        hwirq: usize,
        trigger: fw::InterruptTrigger,
        controller: fw::InterruptControllerKind,
    },
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

    if kbuild_config::PCI_ECAM_BASE == 0 {
        log::info!("PCI: no firmware PCI host and PCI_ECAM_BASE=0; skipping PCI");
        return (0, 0, PciConfigSource::Static);
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
    /// Brings up the PCI bus over the resolved configuration window.
    ///
    /// # Errors
    ///
    /// Returns [`PciInitError::InvalidRange`] when no config space exists,
    /// and [`PciInitError::NoMemory`] / [`PciInitError::MappingFailed`]
    /// when the ECAM window cannot be mapped.
    pub fn new(cam: Cam) -> Result<Self, PciInitError> {
        let (config_base, bus_end, source) = pci_config_space();
        if config_base == 0 {
            log::info!("PCI: no config space available; skipping PCI bus init");
            return Err(PciInitError::InvalidRange);
        }
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
        // SAFETY: `base_vaddr` maps exactly `ecam_size` bytes, which is the
        // bound passed to the checked accessor.
        let config = unsafe { PciConfigAccess::new_bounded(base_vaddr.as_ptr(), cam, ecam_size) };
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
        // SAFETY: `base_vaddr` maps exactly `ecam_size` bytes, which is the
        // bound passed to the checked accessor.
        let config = unsafe { PciConfigAccess::new_bounded(base_vaddr.as_ptr(), cam, ecam_size) };
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

/// Reads the Interrupt Pin register (config space offset `0x3D`).
///
/// The value is the encoded INTx line (`1` = INTA .. `4` = INTD); `0` means
/// that the device does not use legacy interrupts and `0xFF` is a reserved
/// encoding. This helper never reads the Interrupt Line register and never
/// interprets the pin.
pub fn interrupt_pin(config: &PciConfigAccess, bdf: DeviceFunction) -> Result<u8, PciConfigError> {
    Ok(config.try_read(bdf, 0x3D, 1)? as u8)
}

pub fn legacy_interrupt_route(
    config: &PciConfigAccess,
    bdf: DeviceFunction,
) -> Option<LegacyInterruptRoute> {
    let pin = interrupt_pin(config, bdf).ok()?;
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
        "PCI legacy IRQ from firmware: bdf={:?} pin=INT{} hwirq={} trigger={:?} controller={:?}",
        bdf,
        pin,
        irq.irq,
        irq.trigger,
        irq.controller
    );
    Some(LegacyInterruptRoute::Hwirq {
        hwirq: irq.irq,
        trigger: irq.trigger,
        controller: irq.controller,
    })
}

/// Why a checked PCI configuration access was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PciConfigError {
    /// Device or function number is outside the PCI encoding.
    InvalidBdf,
    /// The width or offset is outside the configuration space.
    InvalidOffset,
    /// The access is not one, two, or four bytes wide.
    UnsupportedWidth,
    /// The access falls outside the mapped configuration window.
    OutOfWindow,
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
    window_size: usize,
}

impl PciConfigAccess {
    /// Creates a new `PciConfigAccess` from a raw MMIO base and CAM type.
    ///
    /// The checked accessors treat the full CAM span as mapped. Prefer
    /// [`Self::new_bounded`] when the mapped window covers fewer buses.
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
        // SAFETY: the caller provides the same mapping contract as
        // `new_bounded`; the full CAM span is the documented bound.
        unsafe { Self::new_bounded(mmio_base, cam, cam.size() as usize) }
    }

    /// Creates a `PciConfigAccess` that rejects accesses beyond `window_size`.
    ///
    /// # Safety
    ///
    /// The same requirements as [`Self::new`] apply. `window_size` must not
    /// exceed the mapped window and must cover every access the caller makes.
    pub unsafe fn new_bounded(mmio_base: *mut u8, cam: Cam, window_size: usize) -> Self {
        assert!(mmio_base as usize & 0x3 == 0);
        assert!(window_size > 0);
        Self {
            mmio_base: mmio_base as usize,
            cam,
            window_size,
        }
    }

    /// Returns the number of bytes in one function's configuration space.
    const fn config_size(&self) -> usize {
        match self.cam {
            Cam::MmioCam => 0x100,
            Cam::Ecam => 0x1000,
        }
    }

    /// Validates one checked access and returns its word-aligned offset in the
    /// mapped window.
    fn checked_word_offset(
        &self,
        device_function: DeviceFunction,
        register_offset: u16,
        width: usize,
    ) -> Result<usize, PciConfigError> {
        if !device_function.valid() {
            return Err(PciConfigError::InvalidBdf);
        }
        if !matches!(width, 1 | 2 | 4) {
            return Err(PciConfigError::UnsupportedWidth);
        }
        let end = register_offset as usize + width;
        if end > self.config_size() || !(register_offset as usize).is_multiple_of(width) {
            return Err(PciConfigError::InvalidOffset);
        }

        let address = self.cam_offset(device_function, register_offset) as usize;
        let word_end = (address & !0x3) + 4;
        if word_end > self.window_size {
            return Err(PciConfigError::OutOfWindow);
        }
        Ok(address)
    }

    /// Reads a configuration register as a zero-extended integer.
    ///
    /// `width` must be 1, 2, or 4 and `register_offset` must be naturally
    /// aligned for that width.
    pub fn try_read(
        &self,
        device_function: DeviceFunction,
        register_offset: u16,
        width: usize,
    ) -> Result<u32, PciConfigError> {
        let address = self.checked_word_offset(device_function, register_offset, width)?;
        // SAFETY: validation kept the word inside the mapped window.
        let pointer = unsafe { (self.mmio_base as *const u32).add(address >> 2) };
        // SAFETY: the 4-byte-aligned base plus a word-aligned offset is a
        // valid `u32`.
        let word = unsafe { pointer.read_volatile() };
        let shift = (register_offset as usize & 0x3) * 8;
        Ok(match width {
            1 => (word >> shift) & 0xff,
            2 => (word >> shift) & 0xffff,
            4 => word,
            _ => 0,
        })
    }

    /// Writes a configuration register.
    ///
    /// `width` must be 1, 2, or 4 and `register_offset` must be naturally
    /// aligned for that width. The write is a single access: narrower writes do
    /// not read-modify-write the enclosing word.
    pub fn try_write(
        &self,
        device_function: DeviceFunction,
        register_offset: u16,
        width: usize,
        value: u32,
    ) -> Result<(), PciConfigError> {
        let address = self.checked_word_offset(device_function, register_offset, width)?;
        let byte_offset = register_offset as usize & 0x3;
        match width {
            1 => {
                // SAFETY: validation kept the access inside the mapped window.
                let pointer = unsafe { (self.mmio_base as *mut u8).add(address + byte_offset) };
                // SAFETY: a byte access needs no alignment.
                unsafe { pointer.write_volatile(value as u8) };
            }
            2 => {
                // SAFETY: validation kept the access inside the mapped window.
                let pointer =
                    unsafe { (self.mmio_base as *mut u16).add((address + byte_offset) / 2) };
                // SAFETY: the base is 4-byte aligned and the halfword offset
                // preserves 2-byte alignment.
                unsafe { pointer.write_volatile(value as u16) };
            }
            4 => {
                // SAFETY: validation kept the access inside the mapped window.
                let pointer = unsafe { (self.mmio_base as *mut u32).add(address >> 2) };
                // SAFETY: the base and the word offset are 4-byte aligned.
                unsafe { pointer.write_volatile(value) };
            }
            _ => return Err(PciConfigError::UnsupportedWidth),
        }
        Ok(())
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
        let register = unsafe { (self.mmio_base as *mut u32).add((address >> 2) as usize) };
        // SAFETY: Construction retains a mapped CAM window, and cam_offset
        // selects a naturally aligned 32-bit configuration register within it.
        unsafe { register.read_volatile() }
    }

    /// Writes a 16-bit halfword to PCI configuration space.
    ///
    /// `register_offset` is the byte offset; the LSB is ignored. This is useful
    /// for registers such as PCI Command whose adjacent halfword has different
    /// write semantics.
    pub fn write_halfword(
        &mut self,
        device_function: DeviceFunction,
        register_offset: u16,
        data: u16,
    ) {
        let address =
            self.cam_offset(device_function, register_offset) + u32::from(register_offset & 0x2);
        // SAFETY: The pointer arithmetic stays within the MMIO window because
        // cam_offset() produces offsets bounded by Cam::size(), and the added
        // halfword offset stays inside that 32-bit configuration word.
        let register = unsafe { (self.mmio_base as *mut u8).add(address as usize) }.cast::<u16>();
        // SAFETY: cam_offset() returns a word-aligned address, and adding the
        // selected halfword offset (zero or two) keeps the u16 access naturally
        // aligned and within that 32-bit configuration word.
        unsafe { register.write_volatile(data) };
    }

    /// Writes a 32-bit word to PCI configuration space.
    ///
    /// `register_offset` is the byte offset; the two LSBs are ignored.
    pub fn write_word(&mut self, device_function: DeviceFunction, register_offset: u16, data: u32) {
        let address = self.cam_offset(device_function, register_offset);
        // SAFETY: Same as read_word.
        let register = unsafe { (self.mmio_base as *mut u32).add((address >> 2) as usize) };
        // SAFETY: Construction retains a mapped CAM window, and cam_offset
        // selects a naturally aligned 32-bit configuration register within it.
        unsafe { register.write_volatile(data) };
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

/// Assigns unassigned memory BARs from `allocator`, logs the final BAR
/// layout, and enables IO / memory / bus-master command bits.
///
/// # Errors
///
/// Returns [`PciInitError::NoMemory`] when the allocator cannot satisfy a
/// BAR request, or [`PciInitError::InvalidBar`] for an invalid device BAR encoding.
///
/// # Panics
///
/// Panics via `expect` when `allocator` is `None` while a memory BAR is
/// unassigned — configuring BARs without an allocation range is a
/// platform configuration error.
pub fn configure_device<C: ConfigurationAccess>(
    root: &mut PciRoot<C>,
    bdf: DeviceFunction,
    allocator: &mut Option<PciRangeAllocator>,
) -> Result<(), PciInitError> {
    let mut bar = 0;
    while bar < 6 {
        let info = match root
            .bar_info(bdf, bar)
            .map_err(|_| PciInitError::InvalidBar)?
        {
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
                .ok_or(PciInitError::NoMemory)?
                .alloc_buf(size as _)
                .ok_or(PciInitError::NoMemory)?;
            if address_type == MemoryBarType::Width32 {
                root.set_bar_32(bdf, bar, new_addr as _);
            } else if address_type == MemoryBarType::Width64 {
                root.set_bar_64(bdf, bar, new_addr);
            }
        }

        let info = match root
            .bar_info(bdf, bar)
            .map_err(|_| PciInitError::InvalidBar)?
        {
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

unittest::path_mod_test!(config_access_tests, "../ut-tests/config_access.rs");

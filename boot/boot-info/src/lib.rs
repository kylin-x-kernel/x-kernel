// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared boot handoff protocol for x-kernel boot loaders and kernel stubs.

#![no_std]

use core::{fmt, mem, ptr};

use kcpu_id_map::LogicalCpuId;

/// Magic number for BootInfo structure validation.
///
/// ASCII: "BOOTINFO" = 0x424f4f54494e464f
pub const BOOT_INFO_MAGIC: u64 = 0x424f_4f54_494e_464f;
pub const X86_LINUX_BOOT_MAGIC: u32 = 0x584b_4c42;

/// Current BootInfo structure version.
pub const BOOT_INFO_VERSION: u32 = 5;
pub const X86_LINUX_BOOT_E820_MAX_ENTRIES: usize = 128;
const X86_LINUX_BOOT_LEGACY_CMDLINE_MAX: usize = 255;
const X86_LINUX_BOOT_PARAMS_ACPI_RSDP_ADDR_OFFSET: usize = 0x70;
const X86_LINUX_BOOT_PARAMS_E820_ENTRIES_OFFSET: usize = 0x1e8;
const X86_LINUX_BOOT_PARAMS_CMD_LINE_PTR_OFFSET: usize = 0x228;
const X86_LINUX_BOOT_PARAMS_CMDLINE_SIZE_OFFSET: usize = 0x238;
const X86_LINUX_BOOT_PARAMS_E820_TABLE_OFFSET: usize = 0x2d0;
const X86_LINUX_BOOT_PARAMS_PAYLOAD_OFFSET_OFFSET: usize = 0x248;
const X86_LINUX_BOOT_PARAMS_PAYLOAD_LENGTH_OFFSET: usize = 0x24c;

/// Unified boot information passed from bootloader to kernel.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BootInfo {
    pub magic: u64,
    pub version: u32,
    pub protocol: BootProtocol,
    pub memory_description_root: MemoryDescriptionRoot,
    pub hardware_description_root: HardwareDescriptionRoot,
    pub _reserved: [u8; 1],
    pub arch_flags: u32,
    pub protocol_info_addr: usize,
    pub kernel_load_paddr: usize,
    pub phys_virt_offset: usize,
    pub dtb_addr: usize,
    pub dtb_vaddr: usize,
    pub uefi_memmap_addr: usize,
    pub uefi_memmap_vaddr: usize,
    pub rsdp_addr: usize,
    pub boot_runtime_paddr: usize,
    pub boot_runtime_size: usize,
    pub ramdisk_addr: usize,
    pub ramdisk_size: usize,
    pub cmdline_addr: usize,
    pub cmdline_len: usize,
    pub boot_console_transport: BootConsoleTransport,
    pub _boot_console_reserved: [u8; 7],
    pub boot_console_addr: usize,
    pub boot_console_vaddr: usize,
    pub boot_console_size: usize,
    pub cpu_id: LogicalCpuId,
    pub cpu_count: usize,
    pub framebuffer: Option<FrameBufferInfo>,
}

impl BootInfo {
    pub const fn new(protocol: BootProtocol) -> Self {
        Self {
            magic: BOOT_INFO_MAGIC,
            version: BOOT_INFO_VERSION,
            protocol,
            memory_description_root: MemoryDescriptionRoot::Unknown,
            hardware_description_root: HardwareDescriptionRoot::None,
            _reserved: [0; 1],
            arch_flags: 0,
            protocol_info_addr: 0,
            kernel_load_paddr: 0,
            phys_virt_offset: 0,
            dtb_addr: 0,
            dtb_vaddr: 0,
            uefi_memmap_addr: 0,
            uefi_memmap_vaddr: 0,
            rsdp_addr: 0,
            boot_runtime_paddr: 0,
            boot_runtime_size: 0,
            ramdisk_addr: 0,
            ramdisk_size: 0,
            cmdline_addr: 0,
            cmdline_len: 0,
            boot_console_transport: BootConsoleTransport::None,
            _boot_console_reserved: [0; 7],
            boot_console_addr: 0,
            boot_console_vaddr: 0,
            boot_console_size: 0,
            cpu_id: LogicalCpuId::new(0),
            cpu_count: 0,
            framebuffer: None,
        }
    }

    #[inline]
    pub const fn is_valid(&self) -> bool {
        self.magic == BOOT_INFO_MAGIC && self.version == BOOT_INFO_VERSION
    }

    #[inline]
    pub const fn protocol(&self) -> BootProtocol {
        self.protocol
    }

    #[inline]
    pub const fn with_memory_description_root(mut self, root: MemoryDescriptionRoot) -> Self {
        self.memory_description_root = root;
        self
    }

    #[inline]
    pub const fn memory_description_root(&self) -> MemoryDescriptionRoot {
        self.memory_description_root
    }

    #[inline]
    pub const fn with_hardware_description_root(mut self, root: HardwareDescriptionRoot) -> Self {
        self.hardware_description_root = root;
        self
    }

    #[inline]
    pub const fn hardware_description_root(&self) -> HardwareDescriptionRoot {
        self.hardware_description_root
    }

    pub fn cmdline(&self) -> Option<&str> {
        if self.cmdline_addr == 0 || self.cmdline_len == 0 {
            return None;
        }

        // SAFETY: `cmdline_addr..cmdline_addr + cmdline_len` comes from boot
        // handoff metadata, and callers only request a borrowed view over that
        // immutable byte range.
        unsafe {
            let slice =
                core::slice::from_raw_parts(self.cmdline_addr as *const u8, self.cmdline_len);
            core::str::from_utf8(slice).ok()
        }
    }

    #[inline]
    pub const fn with_dtb(mut self, paddr: usize, vaddr: usize) -> Self {
        self.dtb_addr = paddr;
        self.dtb_vaddr = vaddr;
        self
    }

    #[inline]
    pub const fn has_dtb(&self) -> bool {
        self.dtb_addr != 0 && self.dtb_vaddr != 0
    }

    #[inline]
    pub const fn dtb_ptr(&self) -> Option<*const u8> {
        if !self.has_dtb() {
            None
        } else {
            Some(self.dtb_vaddr as *const u8)
        }
    }

    #[inline]
    pub const fn with_uefi_memmap(mut self, paddr: usize, vaddr: usize) -> Self {
        self.uefi_memmap_addr = paddr;
        self.uefi_memmap_vaddr = vaddr;
        self
    }

    #[inline]
    pub const fn has_uefi_memmap(&self) -> bool {
        self.uefi_memmap_addr != 0 && self.uefi_memmap_vaddr != 0
    }

    #[inline]
    pub const fn uefi_memmap_ptr(&self) -> Option<*const u8> {
        if !self.has_uefi_memmap() {
            None
        } else {
            Some(self.uefi_memmap_vaddr as *const u8)
        }
    }

    #[inline]
    pub const fn with_rsdp(mut self, addr: usize) -> Self {
        self.rsdp_addr = addr;
        self
    }

    #[inline]
    pub const fn has_acpi(&self) -> bool {
        self.rsdp_addr != 0
    }

    #[inline]
    pub const fn with_ramdisk(mut self, addr: usize, size: usize) -> Self {
        self.ramdisk_addr = addr;
        self.ramdisk_size = size;
        self
    }

    #[inline]
    pub const fn with_boot_runtime(mut self, paddr: usize, size: usize) -> Self {
        self.boot_runtime_paddr = paddr;
        self.boot_runtime_size = size;
        self
    }

    #[inline]
    pub const fn with_cmdline(mut self, addr: usize, len: usize) -> Self {
        self.cmdline_addr = addr;
        self.cmdline_len = len;
        self
    }

    #[inline]
    pub const fn with_boot_console_mmio(mut self, paddr: usize, size: usize, vaddr: usize) -> Self {
        self.boot_console_transport = if paddr == 0 || size == 0 || vaddr == 0 {
            BootConsoleTransport::None
        } else {
            BootConsoleTransport::Mmio
        };
        self.boot_console_addr = paddr;
        self.boot_console_vaddr = vaddr;
        self.boot_console_size = size;
        self
    }

    #[inline]
    pub const fn with_boot_console_ioport(mut self, port: u16) -> Self {
        self.boot_console_transport = if port == 0 {
            BootConsoleTransport::None
        } else {
            BootConsoleTransport::IoPort
        };
        self.boot_console_addr = port as usize;
        self.boot_console_vaddr = 0;
        self.boot_console_size = 1;
        self
    }

    #[inline]
    pub const fn boot_runtime_paddr(&self) -> usize {
        self.boot_runtime_paddr
    }

    #[inline]
    pub const fn boot_runtime_size(&self) -> usize {
        self.boot_runtime_size
    }

    #[inline]
    pub const fn with_cpu_id(mut self, id: LogicalCpuId) -> Self {
        self.cpu_id = id;
        self
    }

    #[inline]
    pub const fn with_cpu_count(mut self, count: usize) -> Self {
        self.cpu_count = count;
        self
    }

    #[inline]
    pub const fn with_protocol_info_addr(mut self, addr: usize) -> Self {
        self.protocol_info_addr = addr;
        self
    }

    #[inline]
    pub const fn with_kernel_load_paddr(mut self, addr: usize) -> Self {
        self.kernel_load_paddr = addr;
        self
    }

    #[inline]
    pub const fn with_phys_virt_offset(mut self, offset: usize) -> Self {
        self.phys_virt_offset = offset;
        self
    }
}

impl fmt::Debug for BootInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BootInfo")
            .field("magic", &format_args!("{:#x}", self.magic))
            .field("version", &self.version)
            .field("protocol", &self.protocol)
            .field("memory_description_root", &self.memory_description_root)
            .field("hardware_description_root", &self.hardware_description_root)
            .field(
                "protocol_info_addr",
                &format_args!("{:#x}", self.protocol_info_addr),
            )
            .field(
                "kernel_load_paddr",
                &format_args!("{:#x}", self.kernel_load_paddr),
            )
            .field(
                "phys_virt_offset",
                &format_args!("{:#x}", self.phys_virt_offset),
            )
            .field("dtb_addr", &format_args!("{:#x}", self.dtb_addr))
            .field("dtb_vaddr", &format_args!("{:#x}", self.dtb_vaddr))
            .field(
                "uefi_memmap_addr",
                &format_args!("{:#x}", self.uefi_memmap_addr),
            )
            .field(
                "uefi_memmap_vaddr",
                &format_args!("{:#x}", self.uefi_memmap_vaddr),
            )
            .field("rsdp_addr", &format_args!("{:#x}", self.rsdp_addr))
            .field(
                "boot_runtime",
                &format_args!(
                    "{:#x}..{:#x}",
                    self.boot_runtime_paddr(),
                    self.boot_runtime_paddr() + self.boot_runtime_size()
                ),
            )
            .field(
                "ramdisk",
                &format_args!(
                    "{:#x}..{:#x}",
                    self.ramdisk_addr,
                    self.ramdisk_addr + self.ramdisk_size
                ),
            )
            .field("boot_console_transport", &self.boot_console_transport)
            .field(
                "boot_console_addr",
                &format_args!("{:#x}", self.boot_console_addr),
            )
            .field(
                "boot_console_vaddr",
                &format_args!("{:#x}", self.boot_console_vaddr),
            )
            .field(
                "boot_console_size",
                &format_args!("{:#x}", self.boot_console_size),
            )
            .field("cpu_id", &self.cpu_id)
            .field("cpu_count", &self.cpu_count)
            .finish()
    }
}

/// Minimal view of the x86 Linux boot protocol zeropage (`struct boot_params`).
///
/// The future `QEMU --kernel` entry will hand this pointer through
/// `BootInfo.protocol_info_addr` with `BootProtocol::LinuxBoot`.
#[derive(Clone, Copy)]
pub struct LinuxBootParams {
    addr: usize,
}

impl LinuxBootParams {
    #[inline]
    pub const fn new(addr: usize) -> Option<Self> {
        if addr == 0 { None } else { Some(Self { addr }) }
    }

    #[inline]
    pub const fn addr(self) -> usize {
        self.addr
    }

    #[inline]
    pub fn acpi_rsdp_addr(self) -> u64 {
        // SAFETY: reads a fixed protocol-defined field from the zeropage blob.
        unsafe { self.read_u64(X86_LINUX_BOOT_PARAMS_ACPI_RSDP_ADDR_OFFSET) }
    }

    #[inline]
    pub fn e820_entries(self) -> usize {
        usize::min(
            // SAFETY: reads a fixed protocol-defined field from the zeropage blob.
            unsafe { self.read_u8(X86_LINUX_BOOT_PARAMS_E820_ENTRIES_OFFSET) as usize },
            X86_LINUX_BOOT_E820_MAX_ENTRIES,
        )
    }

    #[inline]
    pub fn e820_entry(self, index: usize) -> Option<X86LinuxE820Entry> {
        if index >= self.e820_entries() {
            return None;
        }
        let entry_ptr = (self.addr
            + X86_LINUX_BOOT_PARAMS_E820_TABLE_OFFSET
            + index * mem::size_of::<X86LinuxE820Entry>())
            as *const X86LinuxE820Entry;
        // SAFETY: `index < e820_entries()` bounds the entry inside the boot
        // params e820 table, and the Linux boot protocol permits unaligned
        // field access from the zeropage blob.
        Some(unsafe { ptr::read_unaligned(entry_ptr) })
    }

    #[inline]
    pub fn cmdline_ptr(self) -> Option<usize> {
        // SAFETY: reads a fixed protocol-defined field from the zeropage blob.
        let ptr = unsafe { self.read_u32(X86_LINUX_BOOT_PARAMS_CMD_LINE_PTR_OFFSET) } as usize;
        (ptr != 0).then_some(ptr)
    }

    #[inline]
    pub fn cmdline_size(self) -> Option<usize> {
        // SAFETY: reads a fixed protocol-defined field from the zeropage blob.
        let size = unsafe { self.read_u32(X86_LINUX_BOOT_PARAMS_CMDLINE_SIZE_OFFSET) } as usize;
        (size != 0).then_some(size)
    }

    pub fn cmdline(self) -> Option<&'static str> {
        let ptr = self.cmdline_ptr()?;
        let max_len = self
            .cmdline_size()
            .unwrap_or(X86_LINUX_BOOT_LEGACY_CMDLINE_MAX);
        let scan_len = max_len.checked_add(1)?;
        // SAFETY: `ptr..ptr + scan_len` is the bootloader-provided command-line
        // buffer, and the extra byte allows scanning for the terminating NUL.
        let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, scan_len) };
        let nul_pos = bytes.iter().position(|&byte| byte == 0)?;
        core::str::from_utf8(&bytes[..nul_pos]).ok()
    }

    #[inline]
    pub fn payload_offset(self) -> Option<u32> {
        // SAFETY: reads a fixed protocol-defined field from the zeropage blob.
        let payload_offset = unsafe { self.read_u32(X86_LINUX_BOOT_PARAMS_PAYLOAD_OFFSET_OFFSET) };
        // SAFETY: reads a fixed protocol-defined field from the zeropage blob.
        let payload_length = unsafe { self.read_u32(X86_LINUX_BOOT_PARAMS_PAYLOAD_LENGTH_OFFSET) };
        if payload_offset == 0 || payload_length == 0 {
            None
        } else {
            Some(payload_offset)
        }
    }

    #[inline]
    pub fn payload_length(self) -> Option<u32> {
        // SAFETY: reads a fixed protocol-defined field from the zeropage blob.
        let payload_offset = unsafe { self.read_u32(X86_LINUX_BOOT_PARAMS_PAYLOAD_OFFSET_OFFSET) };
        // SAFETY: reads a fixed protocol-defined field from the zeropage blob.
        let payload_length = unsafe { self.read_u32(X86_LINUX_BOOT_PARAMS_PAYLOAD_LENGTH_OFFSET) };
        if payload_offset == 0 || payload_length == 0 {
            None
        } else {
            Some(payload_length)
        }
    }

    #[inline]
    unsafe fn read_u8(self, offset: usize) -> u8 {
        // SAFETY: `LinuxBootParams` is a typed view over the boot protocol blob
        // rooted at `self.addr`; callers only use fixed protocol-defined
        // offsets, and x86 zeropage fields permit unaligned reads.
        unsafe { ptr::read_unaligned((self.addr + offset) as *const u8) }
    }

    #[inline]
    unsafe fn read_u64(self, offset: usize) -> u64 {
        // SAFETY: see `read_u8`; the same zeropage invariant applies here.
        unsafe { ptr::read_unaligned((self.addr + offset) as *const u64) }
    }

    #[inline]
    unsafe fn read_u32(self, offset: usize) -> u32 {
        // SAFETY: see `read_u8`; the same zeropage invariant applies here.
        unsafe { ptr::read_unaligned((self.addr + offset) as *const u32) }
    }
}

impl fmt::Debug for LinuxBootParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinuxBootParams")
            .field("addr", &format_args!("{:#x}", self.addr))
            .field(
                "acpi_rsdp_addr",
                &format_args!("{:#x}", self.acpi_rsdp_addr()),
            )
            .field("e820_entries", &self.e820_entries())
            .field("payload_offset", &self.payload_offset())
            .field("payload_length", &self.payload_length())
            .finish()
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootProtocol {
    Unknown    = 0,
    Multiboot1 = 1,
    Multiboot2 = 2,
    Uefi       = 3,
    DeviceTree = 4,
    LinuxBoot  = 5,
    OpenSBI    = 6,
    UBoot      = 7,
    Bios       = 8,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryDescriptionRoot {
    Unknown         = 0,
    DeviceTree      = 1,
    UefiMemmap      = 2,
    X86BootProtocol = 3,
}

impl MemoryDescriptionRoot {
    #[inline]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::DeviceTree => "device tree",
            Self::UefiMemmap => "uefi memmap",
            Self::X86BootProtocol => "x86 boot protocol",
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardwareDescriptionRoot {
    None       = 0,
    DeviceTree = 1,
    Acpi       = 2,
}

impl HardwareDescriptionRoot {
    #[inline]
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::DeviceTree => "device tree",
            Self::Acpi => "acpi",
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootConsoleTransport {
    None   = 0,
    Mmio   = 1,
    IoPort = 2,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct X86LinuxE820Entry {
    pub addr: u64,
    pub size: u64,
    pub entry_type: u32,
    pub attr: u32,
}

impl X86LinuxE820Entry {
    #[inline]
    pub const fn is_usable_ram(&self) -> bool {
        self.entry_type == X86LinuxE820EntryType::Ram as u32 && self.size != 0
    }
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum X86LinuxE820EntryType {
    Ram          = 1,
    Reserved     = 2,
    Acpi         = 3,
    Nvs          = 4,
    Unusable     = 5,
    Disabled     = 6,
    Persistent   = 7,
    SoftReserved = 0xefff_fffe,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Rgb       = 0,
    Bgr       = 1,
    Grayscale = 2,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FrameBufferInfo {
    pub addr: usize,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u16,
    pub format: PixelFormat,
    pub _reserved: u8,
}

const _: () = {
    assert!(core::mem::size_of::<BootInfo>().is_multiple_of(8));
    assert!(core::mem::align_of::<BootInfo>() == 8);
    assert!(core::mem::size_of::<BootProtocol>() == 1);
    assert!(core::mem::size_of::<X86LinuxE820Entry>() == 24);
};

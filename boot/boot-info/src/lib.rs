// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared boot handoff protocol for x-kernel boot loaders and kernel stubs.

#![no_std]

use core::{fmt, mem, ptr};

/// Magic number for BootInfo structure validation.
///
/// ASCII: "BOOTINFO" = 0x424f4f54494e464f
pub const BOOT_INFO_MAGIC: u64 = 0x424f_4f54_494e_464f;
pub const X86_LINUX_BOOT_MAGIC: u32 = 0x584b_4c42;

/// Current BootInfo structure version.
pub const BOOT_INFO_VERSION: u32 = 1;
pub const X86_LINUX_BOOT_E820_MAX_ENTRIES: usize = 128;
const X86_LINUX_BOOT_PARAMS_ACPI_RSDP_ADDR_OFFSET: usize = 0x70;
const X86_LINUX_BOOT_PARAMS_E820_ENTRIES_OFFSET: usize = 0x1e8;
const X86_LINUX_BOOT_PARAMS_E820_TABLE_OFFSET: usize = 0x2d0;
const X86_LINUX_BOOT_PARAMS_PAYLOAD_OFFSET_OFFSET: usize = 0x248;
const X86_LINUX_BOOT_PARAMS_PAYLOAD_LENGTH_OFFSET: usize = 0x24c;

/// Unified boot information passed from bootloader to kernel.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BootInfo {
    pub magic: u64,
    pub version: u32,
    pub _reserved: u32,
    pub protocol: BootProtocol,
    pub arch_flags: u32,
    pub protocol_info_addr: usize,
    pub kernel_load_paddr: usize,
    pub phys_virt_offset: usize,
    pub dtb_addr: usize,
    pub rsdp_addr: usize,
    pub ramdisk_addr: usize,
    pub ramdisk_size: usize,
    pub cmdline_addr: usize,
    pub cmdline_len: usize,
    pub cpu_id: usize,
    pub cpu_count: usize,
    pub framebuffer: Option<FrameBufferInfo>,
}

impl BootInfo {
    pub const fn new(protocol: BootProtocol) -> Self {
        Self {
            magic: BOOT_INFO_MAGIC,
            version: BOOT_INFO_VERSION,
            _reserved: 0,
            protocol,
            arch_flags: 0,
            protocol_info_addr: 0,
            kernel_load_paddr: 0,
            phys_virt_offset: 0,
            dtb_addr: 0,
            rsdp_addr: 0,
            ramdisk_addr: 0,
            ramdisk_size: 0,
            cmdline_addr: 0,
            cmdline_len: 0,
            cpu_id: 0,
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

    pub fn cmdline(&self) -> Option<&str> {
        if self.cmdline_addr == 0 || self.cmdline_len == 0 {
            return None;
        }

        unsafe {
            let slice =
                core::slice::from_raw_parts(self.cmdline_addr as *const u8, self.cmdline_len);
            core::str::from_utf8(slice).ok()
        }
    }

    #[inline]
    pub const fn with_dtb(mut self, addr: usize) -> Self {
        self.dtb_addr = addr;
        self
    }

    #[inline]
    pub const fn with_rsdp(mut self, addr: usize) -> Self {
        self.rsdp_addr = addr;
        self
    }

    #[inline]
    pub const fn with_ramdisk(mut self, addr: usize, size: usize) -> Self {
        self.ramdisk_addr = addr;
        self.ramdisk_size = size;
        self
    }

    #[inline]
    pub const fn with_cpu_id(mut self, id: usize) -> Self {
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
            .field("rsdp_addr", &format_args!("{:#x}", self.rsdp_addr))
            .field(
                "ramdisk",
                &format_args!(
                    "{:#x}..{:#x}",
                    self.ramdisk_addr,
                    self.ramdisk_addr + self.ramdisk_size
                ),
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
        unsafe { self.read_u64(X86_LINUX_BOOT_PARAMS_ACPI_RSDP_ADDR_OFFSET) }
    }

    #[inline]
    pub fn e820_entries(self) -> usize {
        usize::min(
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
        Some(unsafe { ptr::read_unaligned(entry_ptr) })
    }

    #[inline]
    pub fn payload_offset(self) -> Option<u32> {
        let payload_offset = unsafe { self.read_u32(X86_LINUX_BOOT_PARAMS_PAYLOAD_OFFSET_OFFSET) };
        let payload_length = unsafe { self.read_u32(X86_LINUX_BOOT_PARAMS_PAYLOAD_LENGTH_OFFSET) };
        if payload_offset == 0 || payload_length == 0 {
            None
        } else {
            Some(payload_offset)
        }
    }

    #[inline]
    pub fn payload_length(self) -> Option<u32> {
        let payload_offset = unsafe { self.read_u32(X86_LINUX_BOOT_PARAMS_PAYLOAD_OFFSET_OFFSET) };
        let payload_length = unsafe { self.read_u32(X86_LINUX_BOOT_PARAMS_PAYLOAD_LENGTH_OFFSET) };
        if payload_offset == 0 || payload_length == 0 {
            None
        } else {
            Some(payload_length)
        }
    }

    #[inline]
    unsafe fn read_u8(self, offset: usize) -> u8 {
        unsafe { ptr::read_unaligned((self.addr + offset) as *const u8) }
    }

    #[inline]
    unsafe fn read_u64(self, offset: usize) -> u64 {
        unsafe { ptr::read_unaligned((self.addr + offset) as *const u64) }
    }

    #[inline]
    unsafe fn read_u32(self, offset: usize) -> u32 {
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

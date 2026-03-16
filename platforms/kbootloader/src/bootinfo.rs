// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unified boot information structure for all architectures and boot protocols.
//!
//! This module provides a standardized interface between bootloaders and the kernel,
//! abstracting away the details of different boot protocols (Multiboot, UEFI, Device Tree, etc.).
//!
//! # Design Principles
//!
//! - **Architecture Agnostic**: Works on x86_64, aarch64, riscv64, loongarch64
//! - **Protocol Agnostic**: Supports Multiboot, UEFI, OpenSBI, U-Boot, etc.
//! - **FFI Safe**: Uses `#[repr(C)]` for cross-language compatibility
//! - **Extensible**: Version field allows future expansion
//!
//! # Usage
//!
//! ## Bootloader Side (construct BootInfo)
//!
//! ```rust,ignore
//! let boot_info = BootInfo::new(BootProtocol::Multiboot1)
//!     .with_dtb(dtb_addr)
//!     .with_cpu_id(0);
//! ```
//!
//! ## Kernel Side (consume BootInfo)
//!
//! ```rust,ignore
//! pub fn entry(boot_info: &'static BootInfo) {
//!     assert!(boot_info.is_valid());
//!     // ... initialize kernel
//! }
//! ```

use core::fmt;

/// Magic number for BootInfo structure validation.
///
/// ASCII: "BOOTINFO" = 0x424f4f54494e464f
const BOOT_INFO_MAGIC: u64 = 0x424f_4f54_494e_464f;

/// Current BootInfo structure version.
///
/// Increment this when making **incompatible** changes.
/// Bootloader and kernel must have matching major version.
const BOOT_INFO_VERSION: u32 = 1;

/// Unified boot information passed from bootloader to kernel.
///
/// # Memory Layout
///
/// This structure is designed to be placed in a known memory location
/// or passed via register (depending on architecture):
///
/// - **x86_64**: Address in `rdi` register
/// - **aarch64**: Address in `x0` register
/// - **riscv64**: Address in `a0` register
/// - **loongarch64**: Address in `$a0` register
///
/// # Lifetime
///
/// The BootInfo and all referenced data (strings, etc.)
/// must remain valid for the entire kernel lifetime.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BootInfo {
    /// Magic number for structure validation.
    /// Must be [`BOOT_INFO_MAGIC`].
    pub magic: u64,

    /// Structure version. Must match [`BOOT_INFO_VERSION`].
    pub version: u32,

    /// Reserved for future use. Must be 0.
    pub _reserved: u32,

    /// Boot protocol used by the bootloader.
    pub protocol: BootProtocol,

    /// Architecture-specific flags (reserved).
    pub arch_flags: u32,

    /// Kernel physical load address (where bootloader placed the kernel).
    ///
    /// This is the **actual** physical address, not the linked address.
    /// Kernel can use this to calculate relocation offset.
    pub kernel_load_paddr: usize,

    /// Kernel virtual address offset (phys_virt_offset).
    ///
    /// For higher-half kernels: `virt_addr = phys_addr + phys_virt_offset`
    ///
    /// Example:
    /// - x86_64: `0xffff_8000_0000_0000`
    /// - aarch64: `0xffff_0000_0000_0000`
    /// - riscv64: `0xffff_ffc0_0000_0000` (sv39)
    pub phys_virt_offset: usize,

    /// Device Tree Blob (DTB) physical address, if available.
    ///
    /// Set to 0 if not provided (e.g., x86_64 BIOS/UEFI without DTB).
    pub dtb_addr: usize,

    /// ACPI RSDP (Root System Description Pointer) address, if available.
    ///
    /// Set to 0 if not provided (e.g., device tree platforms).
    pub rsdp_addr: usize,

    /// Initial RAM disk (initrd/initramfs) physical address.
    ///
    /// Set to 0 if no ramdisk provided.
    pub ramdisk_addr: usize,

    /// Ramdisk size in bytes.
    pub ramdisk_size: usize,

    /// Command line string physical address (null-terminated).
    ///
    /// Set to 0 if no command line provided.
    pub cmdline_addr: usize,

    /// Command line string length (excluding null terminator).
    pub cmdline_len: usize,

    /// Boot CPU ID (MPIDR on ARM, APIC ID on x86, Hart ID on RISC-V).
    pub cpu_id: usize,

    /// Total number of CPU cores detected by bootloader.
    ///
    /// May be 0 if unknown. Kernel should probe actual count.
    pub cpu_count: usize,

    /// Framebuffer information (if graphics available).
    pub framebuffer: Option<FrameBufferInfo>,
}

impl BootInfo {
    /// Creates a new BootInfo with the given boot protocol.
    ///
    /// All optional fields are initialized to safe default values (0 or None).
    pub const fn new(protocol: BootProtocol) -> Self {
        Self {
            magic: BOOT_INFO_MAGIC,
            version: BOOT_INFO_VERSION,
            _reserved: 0,
            protocol,
            arch_flags: 0,
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

    /// Validates the BootInfo structure.
    ///
    /// # Returns
    ///
    /// - `true` if magic and version are correct
    /// - `false` otherwise (corrupted or incompatible)
    #[inline]
    pub const fn is_valid(&self) -> bool {
        self.magic == BOOT_INFO_MAGIC && self.version == BOOT_INFO_VERSION
    }

    /// Returns the boot protocol used.
    #[inline]
    pub const fn protocol(&self) -> BootProtocol {
        self.protocol
    }

    /// Returns the command line string, if provided.
    ///
    /// # Returns
    ///
    /// - `Some(&str)` if cmdline is valid UTF-8
    /// - `None` if not provided or invalid
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

    /// Builder pattern: set DTB address.
    #[inline]
    pub const fn with_dtb(mut self, addr: usize) -> Self {
        self.dtb_addr = addr;
        self
    }

    /// Builder pattern: set RSDP address.
    #[inline]
    pub const fn with_rsdp(mut self, addr: usize) -> Self {
        self.rsdp_addr = addr;
        self
    }

    /// Builder pattern: set ramdisk.
    #[inline]
    pub const fn with_ramdisk(mut self, addr: usize, size: usize) -> Self {
        self.ramdisk_addr = addr;
        self.ramdisk_size = size;
        self
    }

    /// Builder pattern: set CPU ID.
    #[inline]
    pub const fn with_cpu_id(mut self, id: usize) -> Self {
        self.cpu_id = id;
        self
    }

    /// Builder pattern: set kernel load address.
    #[inline]
    pub const fn with_kernel_load_paddr(mut self, addr: usize) -> Self {
        self.kernel_load_paddr = addr;
        self
    }

    /// Builder pattern: set phys_virt_offset.
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

// ===== Boot Protocol Types =====

/// Boot protocol identifier.
///
/// Indicates which firmware/bootloader interface was used.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootProtocol {
    /// Unknown/unspecified protocol.
    Unknown    = 0,

    /// Multiboot v1 (used by GRUB legacy).
    Multiboot1 = 1,

    /// Multiboot v2 (modern GRUB).
    Multiboot2 = 2,

    /// UEFI Boot Services (x86_64, aarch64).
    Uefi       = 3,

    /// Device Tree (ARM, RISC-V, LoongArch).
    DeviceTree = 4,

    /// Linux Boot Protocol (x86_64).
    LinuxBoot  = 5,

    /// OpenSBI (RISC-V).
    OpenSBI    = 6,

    /// U-Boot (ARM, RISC-V).
    UBoot      = 7,

    /// BIOS (legacy x86).
    Bios       = 8,
}

/// Pixel format for framebuffer.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// RGB (red, green, blue).
    Rgb       = 0,

    /// BGR (blue, green, red).
    Bgr       = 1,

    /// Grayscale.
    Grayscale = 2,
}

/// Framebuffer configuration.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FrameBufferInfo {
    /// Physical address of framebuffer.
    pub addr: usize,

    /// Width in pixels.
    pub width: u32,

    /// Height in pixels.
    pub height: u32,

    /// Pitch/stride in bytes (bytes per scanline).
    pub pitch: u32,

    /// Bits per pixel.
    pub bpp: u16,

    /// Pixel format.
    pub format: PixelFormat,

    /// Reserved.
    pub _reserved: u8,
}

// ===== Safety Assertions =====

// Ensure FFI safety and layout stability
const _: () = {
    assert!(core::mem::size_of::<BootInfo>().is_multiple_of(8));
    assert!(core::mem::align_of::<BootInfo>() == 8);
    assert!(core::mem::size_of::<BootProtocol>() == 1);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bootinfo_creation() {
        let boot_info = BootInfo::new(BootProtocol::Multiboot1);
        assert!(boot_info.is_valid());
        assert_eq!(boot_info.protocol(), BootProtocol::Multiboot1);
    }

    #[test]
    fn test_bootinfo_builder() {
        let boot_info = BootInfo::new(BootProtocol::Uefi)
            .with_dtb(0x8000000)
            .with_cpu_id(0);

        assert_eq!(boot_info.dtb_addr, 0x8000000);
        assert_eq!(boot_info.cpu_id, 0);
    }

    #[test]
    fn test_bootinfo_layout() {
        use core::mem::{align_of, size_of};
        // BootInfo 必须是 8 字节对齐
        assert_eq!(align_of::<BootInfo>(), 8);
        // 大小必须是 8 的倍数 (便于 FFI)
        assert_eq!(size_of::<BootInfo>() % 8, 0);
        // 枚举必须是 1 字节
        assert_eq!(size_of::<BootProtocol>(), 1);
        println!("BootInfo size: {} bytes", size_of::<BootInfo>());
    }

    #[test]
    fn test_bootinfo_defaults() {
        let info = BootInfo::new(BootProtocol::Multiboot1);
        assert!(info.is_valid());
        assert_eq!(info.dtb_addr, 0);
        assert_eq!(info.rsdp_addr, 0);
    }
}

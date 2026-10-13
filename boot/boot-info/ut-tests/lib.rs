// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::fmt::{self, Write};

use unittest::def_test;

use super::*;

const LINUX_BOOT_BLOB_LEN: usize = X86_LINUX_BOOT_PARAMS_E820_TABLE_OFFSET
    + X86_LINUX_BOOT_E820_MAX_ENTRIES * mem::size_of::<X86LinuxE820Entry>();

struct ContainsWriter<'a> {
    needle: &'a str,
    matched: usize,
    found: bool,
}

struct LinuxBootBlobSpec<'a> {
    rsdp: u64,
    e820_entries: u8,
    payload_offset: u32,
    payload_length: u32,
    cmdline_ptr: u32,
    cmdline_size: u32,
    e820: &'a [X86LinuxE820Entry],
}

impl<'a> ContainsWriter<'a> {
    const fn new(needle: &'a str) -> Self {
        Self {
            needle,
            matched: 0,
            found: needle.is_empty(),
        }
    }

    const fn found(&self) -> bool {
        self.found
    }
}

impl Write for ContainsWriter<'_> {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        for byte in value.bytes() {
            if self.found {
                break;
            }
            let needle = self.needle.as_bytes();
            if byte == needle[self.matched] {
                self.matched += 1;
                self.found = self.matched == needle.len();
            } else {
                self.matched = usize::from(byte == needle[0]);
            }
        }
        Ok(())
    }
}

fn le_u32(value: u32) -> [u8; 4] {
    value.to_le_bytes()
}

fn le_u64(value: u64) -> [u8; 8] {
    value.to_le_bytes()
}

fn build_linux_boot_blob(blob: &mut [u8; LINUX_BOOT_BLOB_LEN], spec: LinuxBootBlobSpec<'_>) -> usize {
    blob.fill(0);
    blob[X86_LINUX_BOOT_PARAMS_ACPI_RSDP_ADDR_OFFSET
        ..X86_LINUX_BOOT_PARAMS_ACPI_RSDP_ADDR_OFFSET + 8]
        .copy_from_slice(&le_u64(spec.rsdp));
    blob[X86_LINUX_BOOT_PARAMS_E820_ENTRIES_OFFSET] = spec.e820_entries;
    blob[X86_LINUX_BOOT_PARAMS_CMD_LINE_PTR_OFFSET..X86_LINUX_BOOT_PARAMS_CMD_LINE_PTR_OFFSET + 4]
        .copy_from_slice(&le_u32(spec.cmdline_ptr));
    blob[X86_LINUX_BOOT_PARAMS_CMDLINE_SIZE_OFFSET..X86_LINUX_BOOT_PARAMS_CMDLINE_SIZE_OFFSET + 4]
        .copy_from_slice(&le_u32(spec.cmdline_size));
    blob[X86_LINUX_BOOT_PARAMS_PAYLOAD_OFFSET_OFFSET
        ..X86_LINUX_BOOT_PARAMS_PAYLOAD_OFFSET_OFFSET + 4]
        .copy_from_slice(&le_u32(spec.payload_offset));
    blob[X86_LINUX_BOOT_PARAMS_PAYLOAD_LENGTH_OFFSET
        ..X86_LINUX_BOOT_PARAMS_PAYLOAD_LENGTH_OFFSET + 4]
        .copy_from_slice(&le_u32(spec.payload_length));
    for (index, entry) in spec.e820.iter().enumerate() {
        let offset =
            X86_LINUX_BOOT_PARAMS_E820_TABLE_OFFSET + index * mem::size_of::<X86LinuxE820Entry>();
        blob[offset..offset + 8].copy_from_slice(&entry.addr.to_le_bytes());
        blob[offset + 8..offset + 16].copy_from_slice(&entry.size.to_le_bytes());
        blob[offset + 16..offset + 20].copy_from_slice(&entry.entry_type.to_le_bytes());
        blob[offset + 20..offset + 24].copy_from_slice(&entry.attr.to_le_bytes());
    }
    blob.as_ptr() as usize
}

#[def_test]
fn boot_info_builder_and_validation_helpers() {
    let cmdline = b"console=ttyS0 root=/dev/vda\0";
    let cmdline_len = cmdline.len() - 1;
    let cmdline_addr = cmdline.as_ptr() as usize;
    let info = BootInfo::new(BootProtocol::LinuxBoot)
        .with_memory_description_root(MemoryDescriptionRoot::X86BootProtocol)
        .with_hardware_description_root(HardwareDescriptionRoot::Acpi)
        .with_dtb(0x1000, 0x2000)
        .with_uefi_memmap(0x3000, 0x4000)
        .with_rsdp(0x5000)
        .with_ramdisk(0x6000, 0x7000)
        .with_boot_runtime(0x8000, 0x9000)
        .with_cmdline(cmdline_addr, cmdline_len)
        .with_boot_console_mmio(0xa000, 0x100, 0xb000)
        .with_cpu_id(LogicalCpuId::new(3))
        .with_cpu_count(8)
        .with_protocol_info_addr(0xc000)
        .with_kernel_load_paddr(0xd000)
        .with_phys_virt_offset(0xffff_0000);

    assert!(info.is_valid());
    assert_eq!(info.protocol(), BootProtocol::LinuxBoot);
    assert_eq!(
        info.memory_description_root(),
        MemoryDescriptionRoot::X86BootProtocol
    );
    assert_eq!(
        info.hardware_description_root(),
        HardwareDescriptionRoot::Acpi
    );
    assert_eq!(info.cmdline(), Some("console=ttyS0 root=/dev/vda"));
    assert!(info.has_dtb());
    assert_eq!(info.dtb_ptr(), Some(0x2000 as *const u8));
    assert!(info.has_uefi_memmap());
    assert_eq!(info.uefi_memmap_ptr(), Some(0x4000 as *const u8));
    assert!(info.has_acpi());
    assert_eq!(info.boot_console_transport, BootConsoleTransport::Mmio);
    assert_eq!(info.boot_runtime_paddr(), 0x8000);
    assert_eq!(info.boot_runtime_size(), 0x9000);
    assert_eq!(info.cpu_id, LogicalCpuId::new(3));
    assert_eq!(info.cpu_count, 8);

    let mut invalid = info;
    invalid.magic = 0;
    assert!(!invalid.is_valid());
    invalid = info;
    invalid.version = 0;
    assert!(!invalid.is_valid());
}

#[def_test]
fn boot_info_cmdline_and_console_edge_cases() {
    let invalid_utf8 = [0xff, 0xfe];
    let invalid_utf8_addr = invalid_utf8.as_ptr() as usize;
    let info = BootInfo::new(BootProtocol::Unknown)
        .with_cmdline(invalid_utf8_addr, 2)
        .with_boot_console_mmio(0, 0x100, 0x2000);
    assert_eq!(info.cmdline(), None);
    assert_eq!(info.boot_console_transport, BootConsoleTransport::None);
    assert_eq!(info.dtb_ptr(), None);
    assert_eq!(info.uefi_memmap_ptr(), None);

    let ioport = BootInfo::new(BootProtocol::Bios).with_boot_console_ioport(0x3f8);
    assert_eq!(ioport.boot_console_transport, BootConsoleTransport::IoPort);
    assert_eq!(ioport.boot_console_addr, 0x3f8);
    assert_eq!(ioport.boot_console_size, 1);

    let none = BootInfo::new(BootProtocol::Bios).with_boot_console_ioport(0);
    assert_eq!(none.boot_console_transport, BootConsoleTransport::None);
    let mut debug = ContainsWriter::new("boot_console_transport");
    write!(&mut debug, "{info:?}").unwrap();
    assert!(debug.found());
    // Keep the bytes behind `invalid_utf8_addr` live through the `cmdline()` read above.
    core::hint::black_box(&invalid_utf8);
}

#[def_test]
fn linux_boot_params_parse_fields_and_entries() {
    let e820 = [
        X86LinuxE820Entry {
            addr: 0x1000,
            size: 0x2000,
            entry_type: X86LinuxE820EntryType::Ram as u32,
            attr: 1,
        },
        X86LinuxE820Entry {
            addr: 0x4000,
            size: 0x1000,
            entry_type: X86LinuxE820EntryType::Reserved as u32,
            attr: 2,
        },
    ];
    let mut blob = [0; LINUX_BOOT_BLOB_LEN];
    let blob_addr = build_linux_boot_blob(
        &mut blob,
        LinuxBootBlobSpec {
            rsdp: 0x8877_6655_4433_2211,
            e820_entries: e820.len() as u8,
            payload_offset: 0x1200,
            payload_length: 0x3400,
            cmdline_ptr: 0x1234_5678,
            cmdline_size: 32,
            e820: &e820,
        },
    );
    // SAFETY: `blob_addr` points into `blob`, which remains live and unchanged
    // until the final `black_box(&blob)` after all `params` reads below. The
    // fake `cmd_line_ptr` is never dereferenced because this test does not call
    // `cmdline()`.
    let params = unsafe { LinuxBootParams::new(blob_addr) }.unwrap();

    assert!(params.addr() != 0);
    assert_eq!(params.acpi_rsdp_addr(), 0x8877_6655_4433_2211);
    assert_eq!(params.e820_entries(), 2);
    assert_eq!(params.e820_entry(0).unwrap().addr, 0x1000);
    assert!(params.e820_entry(0).unwrap().is_usable_ram());
    assert_eq!(params.e820_entry(1).unwrap().entry_type, 2);
    assert!(params.e820_entry(2).is_none());
    assert_eq!(params.cmdline_ptr(), Some(0x1234_5678));
    assert_eq!(params.cmdline_size(), Some(32));
    assert_eq!(params.cmdline_range(), Some((0x1234_5678, 33)));
    assert_eq!(params.payload_offset(), Some(0x1200));
    assert_eq!(params.payload_length(), Some(0x3400));
    let mut debug = ContainsWriter::new("payload_offset");
    write!(&mut debug, "{params:?}").unwrap();
    assert!(debug.found());
    core::hint::black_box(&blob);
}

#[def_test]
fn linux_boot_params_handle_legacy_and_invalid_cases() {
    // SAFETY: a zero address imposes no obligations and yields `None`.
    assert!(unsafe { LinuxBootParams::new(0) }.is_none());

    let mut blob = [0; LINUX_BOOT_BLOB_LEN];
    let blob_addr = build_linux_boot_blob(
        &mut blob,
        LinuxBootBlobSpec {
            rsdp: 0,
            e820_entries: (X86_LINUX_BOOT_E820_MAX_ENTRIES as u16 + 5) as u8,
            payload_offset: 0,
            payload_length: 0x3400,
            cmdline_ptr: 0x2000_0000,
            cmdline_size: 0,
            e820: &[],
        },
    );
    // SAFETY: `blob_addr` points into `blob`, which remains live and unchanged
    // until the final `black_box(&blob)` after all `params` reads below. The
    // fake `cmd_line_ptr` is never dereferenced because this test does not call
    // `cmdline()`.
    let params = unsafe { LinuxBootParams::new(blob_addr) }.unwrap();
    assert_eq!(params.e820_entries(), X86_LINUX_BOOT_E820_MAX_ENTRIES);
    assert_eq!(params.cmdline_ptr(), Some(0x2000_0000));
    assert_eq!(params.cmdline_size(), None);
    assert_eq!(params.cmdline_range(), Some((0x2000_0000, 256)));
    assert_eq!(params.payload_offset(), None);
    assert_eq!(params.payload_length(), None);
    core::hint::black_box(&blob);
}

#[def_test]
fn enums_and_framebuffer_helpers_are_stable() {
    assert_eq!(MemoryDescriptionRoot::Unknown.name(), "unknown");
    assert_eq!(MemoryDescriptionRoot::DeviceTree.name(), "device tree");
    assert_eq!(MemoryDescriptionRoot::UefiMemmap.name(), "uefi memmap");
    assert_eq!(HardwareDescriptionRoot::DeviceTree.name(), "device tree");
    assert_eq!(HardwareDescriptionRoot::Acpi.name(), "acpi");

    let zero_ram = X86LinuxE820Entry {
        addr: 0,
        size: 0,
        entry_type: X86LinuxE820EntryType::Ram as u32,
        attr: 0,
    };
    assert!(!zero_ram.is_usable_ram());

    let framebuffer = FrameBufferInfo {
        addr: 0xdead_beef,
        width: 800,
        height: 600,
        pitch: 3200,
        bpp: 32,
        format: PixelFormat::Rgb,
        _reserved: 0,
    };
    assert_eq!(framebuffer.width, 800);
    assert_eq!(framebuffer.format, PixelFormat::Rgb);
}

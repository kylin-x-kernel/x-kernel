// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]
#![no_main]

use core::{arch::global_asm, ptr, slice};

use boot_info::{BOOT_INFO_MAGIC, BootInfo, BootProtocol, LinuxBootParams, X86_LINUX_BOOT_MAGIC};
use kaddr_layout::PAGE_OFFSET;
use multiboot2::{BootInformation, BootInformationHeader, MAGIC, MemoryAreaType};
use x86_boot_common::{
    ALIGN_2M, BootPagingError, PageAllocator, align_up, build_boot_info, build_page_tables,
    jump_to_kernel, kernel_image_size, load_kernel_elf, overlapping_reserved_end,
    switch_page_table,
};

const CR0: u64 = (1 << 0) | (1 << 1) | (1 << 5) | (1 << 16) | (1 << 31);
const CR4: u64 = (1 << 5) | (1 << 7) | (1 << 9) | (1 << 10);
const EFER: u64 = (1 << 8) | (1 << 11);
const IA32_EFER_MSR: u32 = 0xC000_0080;
const BOOT_STACK_SIZE: usize = 64 * 1024;
const MAX_KERNEL_PT: usize = 128;
const FINAL_PT_POOL_PAGES: usize = 5 + MAX_KERNEL_PT;
const UART_DATA_OFFSET: u16 = 0;
const UART_LSR_OFFSET: u16 = 5;
const UART_LSR_THR_EMPTY: u8 = 1 << 5;

#[unsafe(link_section = ".bss.stack")]
static mut BOOT_STACK: [u8; BOOT_STACK_SIZE] = [0; BOOT_STACK_SIZE];

static mut BOOT_INFO: BootInfo = BootInfo::new(BootProtocol::Unknown);
static mut FINAL_PT_POOL: [BootPage; FINAL_PT_POOL_PAGES] =
    [BootPage([0; 512]); FINAL_PT_POOL_PAGES];

unsafe extern "C" {
    static __image_start: u8;
    static __image_end: u8;
}

#[derive(Clone, Copy)]
struct BootCmdline {
    addr: usize,
    len: usize,
}

global_asm!(
    include_str!("boot.S"),
    entry = sym rust_entry,
    boot_stack = sym BOOT_STACK,
    boot_stack_size = const BOOT_STACK_SIZE,
    mb_magic = const MAGIC,
    linux_magic = const X86_LINUX_BOOT_MAGIC,
    boot_console_data_port = const boot_console_data_port(),
    boot_console_lsr_port = const boot_console_lsr_port(),
    cr0 = const CR0,
    cr4 = const CR4,
    efer_msr = const IA32_EFER_MSR,
    efer = const EFER,
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    serial_putc(b'P');
    loop {
        unsafe { core::arch::asm!("hlt", options(nostack, nomem)) }
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn rust_entry(magic: usize, mbi: usize, source_image_paddr: usize) -> ! {
    let (loaded, protocol, protocol_info_paddr, rsdp_paddr, cmdline) = if magic == MAGIC as usize {
        let (loaded, protocol, protocol_info_paddr, rsdp_paddr) =
            load_multiboot_kernel(mbi, source_image_paddr);
        (loaded, protocol, protocol_info_paddr, rsdp_paddr, None)
    } else if magic == X86_LINUX_BOOT_MAGIC as usize {
        load_linuxboot_kernel(mbi, source_image_paddr)
    } else {
        panic!("unsupported boot magic {magic:#x}");
    };
    let mut page_allocator = StaticPageAllocator { next: 0 };
    let pml4 = match build_page_tables(
        &mut page_allocator,
        loaded.load_paddr,
        loaded.vaddr_range,
        0,
    ) {
        Ok(pml4) => pml4,
        Err(BootPagingError::Alloc(StaticPageAllocError::OutOfPages)) => {
            panic!("bootstub page-table pool exhausted")
        }
        Err(BootPagingError::KernelSpansMultiplePdptEntries { .. }) => {
            panic!("kernel image spans multiple PDPT entries")
        }
    };
    unsafe {
        let boot_runtime_start = (&raw const __image_start as usize) & !0xfff;
        let boot_runtime_end = align_up(&raw const __image_end as u64, 0x1000) as usize;
        BOOT_INFO = build_boot_info(
            protocol,
            protocol_info_paddr,
            loaded.load_paddr as usize,
            0,
            kbuild_config::CPU_NUM,
        )
        .with_boot_runtime(boot_runtime_start, boot_runtime_end - boot_runtime_start);
        if rsdp_paddr != 0 {
            BOOT_INFO = BOOT_INFO.with_rsdp(rsdp_paddr);
        }
        if let Some(cmdline) = cmdline {
            BOOT_INFO = BOOT_INFO.with_cmdline(cmdline.addr + PAGE_OFFSET, cmdline.len);
        }
        switch_page_table(pml4, 0);
        jump_to_kernel(
            loaded.entry_vaddr,
            BOOT_INFO_MAGIC,
            (core::ptr::addr_of!(BOOT_INFO) as u64) + PAGE_OFFSET as u64,
            loaded.boot_stack_top_vaddr,
        )
    }
}

fn load_multiboot_kernel(
    mbi: usize,
    source_image_paddr: usize,
) -> (LoadedKernel, BootProtocol, usize, usize) {
    let info = unsafe { BootInformation::load(mbi as *const BootInformationHeader) }
        .expect("invalid multiboot2 boot information");
    let module = info.module_tags().next().expect("missing kernel module");
    let module_start = module.start_address() as u64;
    let module_end = module.end_address() as u64;
    let module_bytes = unsafe {
        slice::from_raw_parts(
            module_start as *const u8,
            (module_end - module_start) as usize,
        )
    };

    (
        load_multiboot_elf_image(
            module_bytes,
            &info,
            mbi as u64,
            module_start,
            module_end,
            source_image_paddr as u64,
        ),
        BootProtocol::Multiboot2,
        mbi,
        0,
    )
}

fn load_linuxboot_kernel(
    boot_params_paddr: usize,
    source_image_paddr: usize,
) -> (
    LoadedKernel,
    BootProtocol,
    usize,
    usize,
    Option<BootCmdline>,
) {
    let params = LinuxBootParams::new(boot_params_paddr).expect("missing linux boot params");
    let payload_offset = params
        .payload_offset()
        .expect("missing linux payload offset");
    let payload_length = params
        .payload_length()
        .expect("missing linux payload length");
    let payload_start = source_image_paddr as u64 + payload_offset as u64;
    let payload_end = payload_start + payload_length as u64;
    let payload =
        unsafe { slice::from_raw_parts(payload_start as *const u8, payload_length as usize) };

    let loaded = load_linuxboot_elf_image(
        payload,
        params,
        boot_params_paddr as u64,
        payload_start,
        payload_end,
        source_image_paddr as u64,
    );

    let cmdline = params.cmdline().map(|cmdline| BootCmdline {
        addr: cmdline.as_ptr() as usize,
        len: cmdline.len(),
    });

    (
        loaded,
        BootProtocol::LinuxBoot,
        boot_params_paddr,
        params.acpi_rsdp_addr() as usize,
        cmdline,
    )
}

fn serial_putc(byte: u8) {
    if boot_console_io_port().is_none() {
        return;
    }
    unsafe {
        loop {
            let mut ready: u8;
            core::arch::asm!(
                "in al, dx",
                in("dx") boot_console_lsr_port(),
                out("al") ready,
                options(nomem, nostack, preserves_flags)
            );
            if ready & UART_LSR_THR_EMPTY != 0 {
                break;
            }
        }
        core::arch::asm!(
            "out dx, al",
            in("dx") boot_console_data_port(),
            in("al") byte,
            options(nomem, nostack, preserves_flags)
        );
    }
}

fn boot_console_io_port() -> Option<u16> {
    if kbuild_config::BOOT_CONSOLE_TYPE != "ioport" || kbuild_config::BOOT_CONSOLE_ADDR == 0 {
        return None;
    }
    Some(kbuild_config::BOOT_CONSOLE_ADDR as u16)
}

const fn boot_console_data_port() -> u16 {
    kbuild_config::BOOT_CONSOLE_ADDR as u16 + UART_DATA_OFFSET
}

const fn boot_console_lsr_port() -> u16 {
    boot_console_data_port() + UART_LSR_OFFSET
}

struct LoadedKernel {
    entry_vaddr: u64,
    boot_stack_top_vaddr: u64,
    load_paddr: u64,
    vaddr_range: (u64, u64),
}

#[derive(Clone, Copy)]
#[repr(C, align(4096))]
struct BootPage([u64; 512]);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StaticPageAllocError {
    OutOfPages,
}

struct StaticPageAllocator {
    next: usize,
}

impl PageAllocator for StaticPageAllocator {
    type Error = StaticPageAllocError;

    fn alloc_page(&mut self) -> Result<u64, Self::Error> {
        if self.next >= FINAL_PT_POOL_PAGES {
            return Err(StaticPageAllocError::OutOfPages);
        }
        let page = unsafe { ptr::addr_of_mut!(FINAL_PT_POOL[self.next]) as u64 };
        self.next += 1;
        Ok(page)
    }
}

fn load_multiboot_elf_image(
    image: &[u8],
    info: &BootInformation<'_>,
    mbi_paddr: u64,
    module_start: u64,
    module_end: u64,
    source_image_paddr: u64,
) -> LoadedKernel {
    let image_size = kernel_image_size(image).expect("invalid kernel ELF");
    let mbi_end = info.end_address() as u64;
    let linked_image_start = &raw const __image_start as u64;
    let linked_image_end = &raw const __image_end as u64;
    let source_image_end = source_image_paddr + stub_image_size() as u64;
    let reserved = [
        (mbi_paddr, align_up(mbi_end, 0x1000)),
        (module_start, align_up(module_end, 0x1000)),
        (linked_image_start, align_up(linked_image_end, 0x1000)),
        (source_image_paddr, align_up(source_image_end, 0x1000)),
    ];
    let load_paddr = choose_load_paddr_multiboot(info, ALIGN_2M, image_size, &reserved);
    let loaded = load_kernel_elf(image, load_paddr).expect("failed to load kernel ELF");
    LoadedKernel {
        entry_vaddr: loaded.entry_vaddr,
        boot_stack_top_vaddr: loaded.boot_stack_top_vaddr,
        load_paddr: loaded.load_paddr,
        vaddr_range: loaded.image_vaddr_range,
    }
}

fn load_linuxboot_elf_image(
    image: &[u8],
    params: LinuxBootParams,
    boot_params_paddr: u64,
    payload_start: u64,
    payload_end: u64,
    source_image_paddr: u64,
) -> LoadedKernel {
    let image_size = kernel_image_size(image).expect("invalid kernel ELF");
    let linked_image_start = &raw const __image_start as u64;
    let linked_image_end = &raw const __image_end as u64;
    let source_image_end = payload_end;
    let reserved = [
        (
            boot_params_paddr,
            align_up(boot_params_paddr + 0x1000, 0x1000),
        ),
        (payload_start, align_up(payload_end, 0x1000)),
        (linked_image_start, align_up(linked_image_end, 0x1000)),
        (source_image_paddr, align_up(source_image_end, 0x1000)),
    ];
    let load_paddr = choose_load_paddr_linuxboot(params, ALIGN_2M, image_size, &reserved);
    let loaded = load_kernel_elf(image, load_paddr).expect("failed to load kernel ELF");
    LoadedKernel {
        entry_vaddr: loaded.entry_vaddr,
        boot_stack_top_vaddr: loaded.boot_stack_top_vaddr,
        load_paddr: loaded.load_paddr,
        vaddr_range: loaded.image_vaddr_range,
    }
}

fn stub_image_size() -> usize {
    (&raw const __image_end as usize) - (&raw const __image_start as usize)
}

fn choose_load_paddr_multiboot(
    info: &BootInformation<'_>,
    min_paddr: u64,
    image_size: u64,
    reserved: &[(u64, u64)],
) -> u64 {
    let mmap = info.memory_map_tag().expect("missing memory map");
    for area in mmap.memory_areas() {
        if MemoryAreaType::from(area.typ()) != MemoryAreaType::Available {
            continue;
        }
        let end = area.start_address() + area.size();
        let mut start = align_up(area.start_address().max(min_paddr), ALIGN_2M);
        while end >= start && end - start >= image_size {
            let candidate_end = start + image_size;
            if let Some(next_start) = overlapping_reserved_end(start, candidate_end, reserved) {
                start = align_up(next_start, ALIGN_2M);
                continue;
            }
            return start;
        }
    }
    panic!("no RAM range for kernel image");
}

fn choose_load_paddr_linuxboot(
    params: LinuxBootParams,
    min_paddr: u64,
    image_size: u64,
    reserved: &[(u64, u64)],
) -> u64 {
    for index in 0..params.e820_entries() {
        let area = params
            .e820_entry(index)
            .expect("linux boot params e820 entry out of range");
        if !area.is_usable_ram() {
            continue;
        }
        let end = area.addr + area.size;
        let mut start = align_up(area.addr.max(min_paddr), ALIGN_2M);
        while end >= start && end - start >= image_size {
            let candidate_end = start + image_size;
            if let Some(next_start) = overlapping_reserved_end(start, candidate_end, reserved) {
                start = align_up(next_start, ALIGN_2M);
                continue;
            }
            return start;
        }
    }
    panic!("no RAM range for kernel image");
}

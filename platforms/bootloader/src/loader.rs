use alloc::{string::String, vec::Vec};
use core::{mem, ptr};

use log::{error, info};
use uefi::{
    boot::AllocateType,
    mem::memory_map::MemoryType,
    prelude::*,
    proto::media::file::{File, FileAttribute, FileInfo, FileMode, FileType, RegularFile},
};

use crate::pages_for;

pub(crate) fn load_kernel(image: Handle, kernel_paths: &[String]) -> Result<Vec<u8>, Status> {
    let mut fs = uefi::boot::get_image_file_system(image).map_err(|e| e.status())?;
    let mut root = fs.open_volume().map_err(|e| e.status())?;

    for path in kernel_paths {
        let path16 =
            uefi::CString16::try_from(path.as_str()).map_err(|_| Status::INVALID_PARAMETER)?;
        info!("trying kernel path: {}", path);
        match root.open(&path16, FileMode::Read, FileAttribute::empty()) {
            Ok(handle) => {
                let file = match handle.into_type().map_err(|e| e.status())? {
                    FileType::Regular(f) => f,
                    _ => return Err(Status::UNSUPPORTED),
                };
                info!("kernel opened: {}", path);
                return read_file(file);
            }
            Err(err) => {
                info!("open failed for {}: {:?}", path, err.status());
                continue;
            }
        }
    }

    error!("kernel file not found in EFI root");
    Err(Status::NOT_FOUND)
}

fn read_file(mut file: RegularFile) -> Result<Vec<u8>, Status> {
    let info = file.get_boxed_info::<FileInfo>().map_err(|e| e.status())?;
    let file_size = info.file_size() as usize;
    info!("kernel file size = {} bytes", file_size);
    let mut data = alloc::vec![0u8; file_size];
    let mut read = 0usize;
    while read < file_size {
        let slice = &mut data[read..];
        let len = file.read(slice).map_err(|e| e.status())?;
        if len == 0 {
            break;
        }
        read += len;
    }
    data.truncate(read);
    info!("kernel file read = {} bytes", read);
    Ok(data)
}

pub(crate) fn load_kernel_image(
    image: &[u8],
    kernel_base_paddr: u64,
    phys_virt_offset: u64,
) -> Result<(u64, (u64, u64)), Status> {
    info!("kernel image size = {} bytes", image.len());
    let elf = is_elf(image);
    info!("kernel image is_elf = {}", elf);
    if elf {
        load_elf(image, phys_virt_offset)
    } else {
        load_bin(image, kernel_base_paddr, phys_virt_offset)
    }
}

fn is_elf(image: &[u8]) -> bool {
    image.len() >= 4 && image[0] == 0x7f && image[1] == b'E' && image[2] == b'L' && image[3] == b'F'
}

fn load_bin(
    image: &[u8],
    kernel_base_paddr: u64,
    phys_virt_offset: u64,
) -> Result<(u64, (u64, u64)), Status> {
    let paddr = kernel_base_paddr;
    let vaddr = kernel_base_paddr + phys_virt_offset;
    let size = image.len() as u64;
    let pages = pages_for(size);
    info!(
        "load_bin: paddr={:#x} vaddr={:#x} size={} pages={}",
        paddr, vaddr, size, pages
    );

    match uefi::boot::allocate_pages(AllocateType::Address(paddr), MemoryType::LOADER_DATA, pages) {
        Ok(_) => info!("load_bin: allocate_pages ok"),
        Err(e) => {
            error!("load_bin: allocate_pages failed: {:?}", e.status());
            return Err(e.status());
        }
    }

    unsafe {
        let dst = paddr as *mut u8;
        ptr::copy_nonoverlapping(image.as_ptr(), dst, image.len());
    }

    Ok((vaddr, (vaddr, vaddr + size)))
}

fn load_elf(image: &[u8], phys_virt_offset: u64) -> Result<(u64, (u64, u64)), Status> {
    let hdr = Elf64Header::parse(image).ok_or(Status::LOAD_ERROR)?;
    info!(
        "elf header: entry={:#x} phoff={:#x} phentsize={} phnum={} flags={:#x}",
        hdr.e_entry, hdr.e_phoff, hdr.e_phentsize, hdr.e_phnum, hdr.e_flags
    );
    if hdr.e_phentsize as usize != mem::size_of::<Elf64Phdr>() {
        return Err(Status::LOAD_ERROR);
    }

    let mut min_vaddr = u64::MAX;
    let mut max_vaddr = 0u64;
    let mut allocations: Vec<(u64, u64)> = Vec::new();

    for idx in 0..hdr.e_phnum {
        let ph = hdr.phdr(image, idx).ok_or(Status::LOAD_ERROR)?;
        info!(
            "phdr[{}]: type={} flags={:#x} off={:#x} vaddr={:#x} paddr={:#x} filesz={:#x} \
             memsz={:#x}",
            idx,
            ph.p_type,
            ph.p_flags,
            ph.p_offset,
            ph.p_vaddr,
            ph.p_paddr,
            ph.p_filesz,
            ph.p_memsz
        );
        if ph.p_type != 1 {
            continue;
        }
        let (paddr, vaddr) = phdr_to_phys(ph, phys_virt_offset);
        let memsz = ph.p_memsz as u64;
        let filesz = ph.p_filesz as u64;

        let paddr_aligned = paddr & !0xfff;
        let vaddr_aligned = vaddr & !0xfff;
        let page_offset = (paddr - paddr_aligned) as usize;
        let memsz_aligned = memsz + page_offset as u64;
        let pages = pages_for(memsz_aligned);

        info!(
            "load_elf: segment vaddr={:#x} -> paddr={:#x} memsz={:#x} filesz={:#x} pages={} \
             (aligned paddr={:#x} offset={:#x})",
            vaddr, paddr, memsz, filesz, pages, paddr_aligned, page_offset
        );

        let file_end = ph.p_offset as u64 + filesz;
        if file_end as usize > image.len() {
            return Err(Status::LOAD_ERROR);
        }

        let alloc_end = paddr_aligned + (pages as u64) * 0x1000;
        let mut alloc_start = paddr_aligned;
        let mut covered = false;
        let mut overlap_end = alloc_start;
        for (start, end) in &allocations {
            if *start <= alloc_start && *end >= alloc_end {
                covered = true;
                break;
            }
            if *start <= alloc_start && *end > overlap_end {
                overlap_end = *end;
            }
        }

        if covered {
            info!("load_elf: allocation already covered");
        } else {
            if overlap_end > alloc_start {
                alloc_start = overlap_end;
            }
            if alloc_start < alloc_end {
                let tail_size = alloc_end - alloc_start;
                let tail_pages = pages_for(tail_size);
                info!(
                    "load_elf: allocating tail {:#x}..{:#x} pages={}",
                    alloc_start,
                    alloc_start + (tail_pages as u64) * 0x1000,
                    tail_pages
                );
                match uefi::boot::allocate_pages(
                    AllocateType::Address(alloc_start),
                    MemoryType::LOADER_DATA,
                    tail_pages,
                ) {
                    Ok(_) => info!("load_elf: allocate_pages ok"),
                    Err(e) => {
                        error!(
                            "load_elf: allocate_pages failed at address {:#x}, pages={}, \
                             size={:#x}, error={:?}",
                            alloc_start,
                            tail_pages,
                            tail_size,
                            e.status()
                        );
                        return Err(e.status());
                    }
                }
                allocations.push((alloc_start, alloc_start + (tail_pages as u64) * 0x1000));
            } else {
                info!("load_elf: allocation fully overlapped, no new pages");
            }
        }

        unsafe {
            let dst = (paddr_aligned as *mut u8).add(page_offset);
            ptr::write_bytes(dst, 0, memsz as usize);
            let src = &image[ph.p_offset as usize..file_end as usize];
            ptr::copy_nonoverlapping(src.as_ptr(), dst, filesz as usize);
        }

        min_vaddr = min_vaddr.min(vaddr_aligned);
        max_vaddr = max_vaddr.max(vaddr_aligned + memsz_aligned);
    }

    Ok((hdr.e_entry, (min_vaddr, max_vaddr)))
}

pub(crate) fn virt_to_phys(vaddr: u64, phys_virt_offset: u64) -> (u64, u64) {
    if vaddr >= phys_virt_offset {
        let paddr = vaddr - phys_virt_offset;
        (paddr, vaddr)
    } else {
        (vaddr, vaddr)
    }
}

fn phdr_to_phys(ph: Elf64Phdr, phys_virt_offset: u64) -> (u64, u64) {
    let vaddr = if ph.p_vaddr != 0 {
        ph.p_vaddr
    } else {
        ph.p_paddr
    };
    info!("phdr_to_phys: vaddr={:#x} paddr={:#x}", vaddr, ph.p_paddr);
    virt_to_phys(vaddr, phys_virt_offset)
}

#[repr(C)]
struct Elf64Header {
    e_ident: [u8; 16],
    e_type: u16,
    e_machine: u16,
    e_version: u32,
    e_entry: u64,
    e_phoff: u64,
    e_shoff: u64,
    e_flags: u32,
    e_ehsize: u16,
    e_phentsize: u16,
    e_phnum: u16,
    e_shentsize: u16,
    e_shnum: u16,
    e_shstrndx: u16,
}

impl Elf64Header {
    fn parse(image: &[u8]) -> Option<Self> {
        if image.len() < mem::size_of::<Elf64Header>() {
            return None;
        }
        let hdr = unsafe { ptr::read_unaligned(image.as_ptr() as *const Elf64Header) };
        if &hdr.e_ident[0..4] != b"\x7fELF" {
            return None;
        }
        if hdr.e_ident[4] != 2 {
            return None;
        }
        Some(hdr)
    }

    fn phdr(&self, image: &[u8], idx: u16) -> Option<Elf64Phdr> {
        let off = self.e_phoff as usize + idx as usize * self.e_phentsize as usize;
        let end = off + mem::size_of::<Elf64Phdr>();
        if end > image.len() {
            return None;
        }
        Some(unsafe { ptr::read_unaligned(image[off..end].as_ptr() as *const Elf64Phdr) })
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Elf64Phdr {
    p_type: u32,
    p_flags: u32,
    p_offset: u64,
    p_vaddr: u64,
    p_paddr: u64,
    p_filesz: u64,
    p_memsz: u64,
    p_align: u64,
}

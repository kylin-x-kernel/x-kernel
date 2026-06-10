// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]

use core::{mem, ptr};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadSegment<'a> {
    pub vaddr: u64,
    pub memsz: u64,
    pub file_data: &'a [u8],
}

pub struct KernelElf<'a> {
    image: &'a [u8],
    header: Elf64Header,
    image_vaddr_range: (u64, u64),
}

impl<'a> KernelElf<'a> {
    pub fn parse(image: &'a [u8]) -> Result<Self, &'static str> {
        let header = Elf64Header::parse(image).ok_or("invalid ELF header")?;
        if header.e_phentsize as usize != mem::size_of::<Elf64Phdr>() {
            return Err("invalid program header size");
        }

        let mut min_vaddr = u64::MAX;
        let mut max_vaddr = 0u64;
        for idx in 0..header.e_phnum {
            let ph = header.phdr(image, idx).ok_or("invalid program header")?;
            if ph.p_type != PT_LOAD {
                continue;
            }
            let (vaddr, memsz) = load_segment_layout(&ph)?;
            let file_end = ph
                .p_offset
                .checked_add(ph.p_filesz)
                .ok_or("segment file range overflow")?;
            if file_end as usize > image.len() {
                return Err("segment file range out of bounds");
            }
            let seg_end = vaddr
                .checked_add(memsz)
                .ok_or("segment memory range overflow")?;
            min_vaddr = min_vaddr.min(vaddr & !0xfff);
            max_vaddr = max_vaddr.max(align_up(seg_end, 0x1000)?);
        }
        if min_vaddr == u64::MAX || max_vaddr <= min_vaddr {
            return Err("missing PT_LOAD segments");
        }

        Ok(Self {
            image,
            header,
            image_vaddr_range: (min_vaddr, max_vaddr),
        })
    }

    pub fn image_vaddr_range(&self) -> (u64, u64) {
        self.image_vaddr_range
    }

    pub fn image_size(&self) -> u64 {
        self.image_vaddr_range.1 - self.image_vaddr_range.0
    }

    pub fn entry_point_vaddr(&self) -> u64 {
        self.header.e_entry
    }

    pub fn paddr_for_vaddr(&self, load_paddr: u64, vaddr: u64) -> Option<u64> {
        let (start, end) = self.image_vaddr_range;
        if !(start..end).contains(&vaddr) {
            return None;
        }
        Some(load_paddr + (vaddr - start))
    }

    pub fn find_symbol_value(&self, name: &str) -> Option<u64> {
        for idx in 0..self.header.e_shnum {
            let shdr = self.header.shdr(self.image, idx)?;
            if shdr.sh_type != SHT_SYMTAB || shdr.sh_entsize as usize != mem::size_of::<Elf64Sym>()
            {
                continue;
            }
            let strtab = self.header.shdr(self.image, shdr.sh_link as u16)?;
            let strtab_start = strtab.sh_offset as usize;
            let strtab_end = strtab_start.checked_add(strtab.sh_size as usize)?;
            let sym_start = shdr.sh_offset as usize;
            let sym_end = sym_start.checked_add(shdr.sh_size as usize)?;
            if strtab_end > self.image.len() || sym_end > self.image.len() {
                return None;
            }

            let strtab_bytes = &self.image[strtab_start..strtab_end];
            let sym_bytes = &self.image[sym_start..sym_end];
            let count = sym_bytes.len() / mem::size_of::<Elf64Sym>();
            for i in 0..count {
                let off = i * mem::size_of::<Elf64Sym>();
                let sym = unsafe {
                    ptr::read_unaligned(sym_bytes[off..off + mem::size_of::<Elf64Sym>()].as_ptr()
                        as *const Elf64Sym)
                };
                let name_off = sym.st_name as usize;
                if name_off >= strtab_bytes.len() {
                    continue;
                }
                let end = strtab_bytes[name_off..]
                    .iter()
                    .position(|&b| b == 0)
                    .map(|v| name_off + v)?;
                if strtab_bytes[name_off..end] == *name.as_bytes() {
                    return Some(sym.st_value);
                }
            }
        }
        None
    }

    pub fn load_segments(&self) -> LoadSegments<'_> {
        LoadSegments { elf: self, idx: 0 }
    }

    /// # Safety
    ///
    /// `load_paddr..load_paddr + image_size()` must be writable RAM and must not
    /// overlap unrelated live data.
    pub unsafe fn load_to(&self, load_paddr: u64) -> Result<(), &'static str> {
        let dst_start = load_paddr;
        let dst_end = dst_start
            .checked_add(self.image_size())
            .ok_or("load range overflow")?;
        let src_start = self.image.as_ptr() as usize as u64;
        let src_end = src_start
            .checked_add(self.image.len() as u64)
            .ok_or("source image range overflow")?;

        if dst_start < src_end && src_start < dst_end {
            for seg in self.load_segments() {
                let seg_paddr = load_paddr + (seg.vaddr - self.image_vaddr_range.0);
                unsafe {
                    ptr::copy(
                        seg.file_data.as_ptr(),
                        seg_paddr as *mut u8,
                        seg.file_data.len(),
                    );
                    if seg.memsz > seg.file_data.len() as u64 {
                        ptr::write_bytes(
                            (seg_paddr + seg.file_data.len() as u64) as *mut u8,
                            0,
                            (seg.memsz - seg.file_data.len() as u64) as usize,
                        );
                    }
                }
            }
        } else {
            unsafe {
                ptr::write_bytes(load_paddr as *mut u8, 0, self.image_size() as usize);
            }
            for seg in self.load_segments() {
                let seg_paddr = load_paddr + (seg.vaddr - self.image_vaddr_range.0);
                unsafe {
                    ptr::copy_nonoverlapping(
                        seg.file_data.as_ptr(),
                        seg_paddr as *mut u8,
                        seg.file_data.len(),
                    );
                }
            }
        }
        Ok(())
    }
}

pub struct LoadSegments<'a> {
    elf: &'a KernelElf<'a>,
    idx: u16,
}

impl<'a> Iterator for LoadSegments<'a> {
    type Item = LoadSegment<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        while self.idx < self.elf.header.e_phnum {
            let ph = self.elf.header.phdr(self.elf.image, self.idx)?;
            self.idx += 1;
            if ph.p_type != PT_LOAD {
                continue;
            }
            let (vaddr, memsz) = load_segment_layout(&ph).ok()?;
            let file_start = ph.p_offset as usize;
            let file_end = file_start.checked_add(ph.p_filesz as usize)?;
            return Some(LoadSegment {
                vaddr,
                memsz,
                file_data: self.elf.image.get(file_start..file_end)?,
            });
        }
        None
    }
}

const PT_LOAD: u32 = 1;
const SHT_SYMTAB: u32 = 2;

fn load_segment_layout(ph: &Elf64Phdr) -> Result<(u64, u64), &'static str> {
    let vaddr = if ph.p_vaddr != 0 {
        ph.p_vaddr
    } else {
        ph.p_paddr
    };
    let memsz = if ph.p_vaddr == 0 && ph.p_memsz > vaddr {
        ph.p_memsz - vaddr
    } else {
        ph.p_memsz
    };
    if memsz < ph.p_filesz {
        return Err("segment mem size smaller than file size");
    }
    Ok((vaddr, memsz))
}

fn align_up(value: u64, align: u64) -> Result<u64, &'static str> {
    if align == 0 || !align.is_power_of_two() {
        return Err("invalid alignment");
    }
    value
        .checked_add(align - 1)
        .map(|value| value & !(align - 1))
        .ok_or("aligned address overflow")
}

#[repr(C)]
#[derive(Clone, Copy)]
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
        if hdr.e_ident[..4] != [0x7f, b'E', b'L', b'F'] || hdr.e_ident[4] != 2 {
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

    fn shdr(&self, image: &[u8], idx: u16) -> Option<Elf64Shdr> {
        let off = self.e_shoff as usize + idx as usize * self.e_shentsize as usize;
        let end = off + mem::size_of::<Elf64Shdr>();
        if end > image.len() {
            return None;
        }
        Some(unsafe { ptr::read_unaligned(image[off..end].as_ptr() as *const Elf64Shdr) })
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

#[repr(C)]
#[derive(Clone, Copy)]
struct Elf64Shdr {
    sh_name: u32,
    sh_type: u32,
    sh_flags: u64,
    sh_addr: u64,
    sh_offset: u64,
    sh_size: u64,
    sh_link: u32,
    sh_info: u32,
    sh_addralign: u64,
    sh_entsize: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Elf64Sym {
    st_name: u32,
    st_info: u8,
    st_other: u8,
    st_shndx: u16,
    st_value: u64,
    st_size: u64,
}

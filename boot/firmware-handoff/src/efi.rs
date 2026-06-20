// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{ffi::c_void, mem::size_of, ptr, slice};

pub const LINUX_EFI_BOOT_MEMMAP_GUID: Guid = Guid::new(
    0x800f683f,
    0xd08b,
    0x423a,
    [0xa2, 0x93, 0x96, 0x5c, 0x3c, 0x6f, 0xe2, 0xb4],
);
pub const DEVICE_TREE_GUID: Guid = Guid::new(
    0xb1b621d5,
    0xf19c,
    0x41a5,
    [0x83, 0x0b, 0xd9, 0x15, 0x2c, 0x69, 0xaa, 0xe0],
);
pub const ACPI_20_TABLE_GUID: Guid = Guid::new(
    0x8868e871,
    0xe4f1,
    0x11d3,
    [0xbc, 0x22, 0x00, 0x80, 0xc7, 0x3c, 0x88, 0x81],
);
pub const ACPI_TABLE_GUID: Guid = Guid::new(
    0xeb9d2d30,
    0x2d88,
    0x11d3,
    [0x9a, 0x16, 0x00, 0x90, 0x27, 0x3f, 0xc1, 0x4d],
);

const EFI_PAGE_SIZE: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

impl Guid {
    #[must_use]
    pub const fn new(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> Self {
        Self {
            data1,
            data2,
            data3,
            data4,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct TableHeader {
    pub signature: u64,
    pub size: u32,
    pub crc32: u32,
    pub reserved: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct ConfigurationTable {
    pub vendor_guid: Guid,
    pub vendor_table: *mut c_void,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct SystemTable {
    pub header: TableHeader,
    pub firmware_vendor: *const u16,
    pub firmware_revision: u32,
    pub stdin_handle: *mut c_void,
    pub stdin: *mut c_void,
    pub stdout_handle: *mut c_void,
    pub stdout: *mut c_void,
    pub stderr_handle: *mut c_void,
    pub stderr: *mut c_void,
    pub runtime_services: *mut c_void,
    pub boot_services: *mut c_void,
    pub number_of_configuration_table_entries: usize,
    pub configuration_table: *mut ConfigurationTable,
}

impl SystemTable {
    pub const SIGNATURE: u64 = 0x5453_5953_2049_4249;
}

impl Default for SystemTable {
    fn default() -> Self {
        Self {
            header: TableHeader {
                signature: Self::SIGNATURE,
                size: size_of::<Self>() as u32,
                crc32: 0,
                reserved: 0,
            },
            firmware_vendor: ptr::null(),
            firmware_revision: 0,
            stdin_handle: ptr::null_mut(),
            stdin: ptr::null_mut(),
            stdout_handle: ptr::null_mut(),
            stdout: ptr::null_mut(),
            stderr_handle: ptr::null_mut(),
            stderr: ptr::null_mut(),
            runtime_services: ptr::null_mut(),
            boot_services: ptr::null_mut(),
            number_of_configuration_table_entries: 0,
            configuration_table: ptr::null_mut(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct MemoryType(pub u32);

impl MemoryType {
    pub const ACPI_NON_VOLATILE: Self = Self(10);
    pub const ACPI_RECLAIM: Self = Self(9);
    pub const BOOT_SERVICES_CODE: Self = Self(3);
    pub const BOOT_SERVICES_DATA: Self = Self(4);
    pub const CONVENTIONAL: Self = Self(7);
    pub const LOADER_CODE: Self = Self(1);
    pub const LOADER_DATA: Self = Self(2);
    pub const MMIO: Self = Self(11);
    pub const MMIO_PORT_SPACE: Self = Self(12);
    pub const PAL_CODE: Self = Self(13);
    pub const PERSISTENT_MEMORY: Self = Self(14);
    pub const RESERVED: Self = Self(0);
    pub const RUNTIME_SERVICES_CODE: Self = Self(5);
    pub const RUNTIME_SERVICES_DATA: Self = Self(6);
    pub const UNACCEPTED: Self = Self(15);
    pub const UNUSABLE: Self = Self(8);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(C)]
pub struct MemoryDescriptor {
    pub ty: MemoryType,
    pub phys_start: u64,
    pub virt_start: u64,
    pub page_count: u64,
    pub att: u64,
}

impl MemoryDescriptor {
    pub const VERSION: u32 = 1;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    NullPointer,
    InvalidSystemTable,
    InvalidConfigurationTable,
    InvalidBootMemmap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryRegionKind {
    Ram,
    RuntimeServices,
    Acpi,
    Persistent,
    Unusable,
    Mmio,
    MmioPortSpace,
    PalCode,
    Reserved,
}

impl MemoryRegionKind {
    #[must_use]
    pub const fn is_linear_mapping_candidate(self) -> bool {
        !matches!(self, Self::Mmio | Self::MmioPortSpace | Self::PalCode)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RawEfiContext<'a> {
    system_table: &'a SystemTable,
    configuration_table_paddr: usize,
    configuration_table_entries: usize,
}

impl<'a> RawEfiContext<'a> {
    /// # Safety
    ///
    /// `ptr` must point to a readable EFI system table and its configuration
    /// table array for the full lifetime of the returned view.
    pub unsafe fn from_ptr(ptr: *const SystemTable) -> Result<Self, ParseError> {
        // SAFETY: The caller guarantees `ptr` points to a readable EFI system
        // table for the lifetime of the returned view.
        let system_table = unsafe { ptr.as_ref() }.ok_or(ParseError::NullPointer)?;
        if system_table.header.signature != SystemTable::SIGNATURE
            || usize::try_from(system_table.header.size).ok() < Some(size_of::<SystemTable>())
        {
            return Err(ParseError::InvalidSystemTable);
        }

        let configuration_table_paddr = system_table.configuration_table as usize;
        let configuration_table_entries = system_table.number_of_configuration_table_entries;
        if configuration_table_entries != 0 && configuration_table_paddr == 0 {
            return Err(ParseError::InvalidConfigurationTable);
        }

        Ok(Self {
            system_table,
            configuration_table_paddr,
            configuration_table_entries,
        })
    }

    #[must_use]
    pub const fn system_table(&self) -> &'a SystemTable {
        self.system_table
    }

    #[must_use]
    pub const fn configuration_table_paddr(&self) -> usize {
        self.configuration_table_paddr
    }

    #[must_use]
    pub const fn configuration_table_entries(&self) -> usize {
        self.configuration_table_entries
    }

    /// # Safety
    ///
    /// `map` must convert the physical address of the EFI configuration table
    /// array into a readable pointer for the full lifetime of the returned
    /// slice.
    pub unsafe fn configuration_tables(
        &self,
        map: impl FnOnce(usize) -> *const ConfigurationTable,
    ) -> Result<&'a [ConfigurationTable], ParseError> {
        if self.configuration_table_entries == 0 {
            return Ok(&[]);
        }

        let table_ptr = map(self.configuration_table_paddr);
        if table_ptr.is_null() {
            return Err(ParseError::InvalidConfigurationTable);
        }

        // SAFETY: The caller guarantees `map` returns a readable configuration
        // table array with `configuration_table_entries` elements.
        Ok(unsafe { slice::from_raw_parts(table_ptr, self.configuration_table_entries) })
    }

    /// # Safety
    ///
    /// `map` must convert the physical address of the EFI configuration table
    /// array into a readable pointer.
    pub unsafe fn config_table(
        &self,
        guid: Guid,
        map: impl FnOnce(usize) -> *const ConfigurationTable,
    ) -> Result<Option<usize>, ParseError> {
        // SAFETY: The caller guarantees `map` yields a readable configuration
        // table array, matching the contract of `configuration_tables`.
        Ok(unsafe { self.configuration_tables(map) }?
            .iter()
            .find(|entry| entry.vendor_guid == guid)
            .and_then(|entry| {
                let addr = entry.vendor_table as usize;
                (addr != 0).then_some(addr)
            }))
    }

    /// # Safety
    ///
    /// `map` must convert the physical address of the EFI configuration table
    /// array into a readable pointer.
    pub unsafe fn linux_boot_memmap_addr(
        &self,
        map: impl FnOnce(usize) -> *const ConfigurationTable,
    ) -> Result<Option<usize>, ParseError> {
        // SAFETY: The caller guarantees `map` returns a readable EFI
        // configuration table array.
        unsafe { self.config_table(LINUX_EFI_BOOT_MEMMAP_GUID, map) }
    }

    /// # Safety
    ///
    /// `map` must convert the physical address of the EFI configuration table
    /// array into a readable pointer.
    pub unsafe fn dtb_addr(
        &self,
        map: impl FnOnce(usize) -> *const ConfigurationTable,
    ) -> Result<Option<usize>, ParseError> {
        // SAFETY: The caller guarantees `map` returns a readable EFI
        // configuration table array.
        unsafe { self.config_table(DEVICE_TREE_GUID, map) }
    }

    /// # Safety
    ///
    /// `map` must convert the physical address of the EFI configuration table
    /// array into a readable pointer.
    pub unsafe fn rsdp_addr(
        &self,
        map: impl Fn(usize) -> *const ConfigurationTable + Copy,
    ) -> Result<Option<usize>, ParseError> {
        // SAFETY: The caller guarantees `map` returns a readable EFI
        // configuration table array.
        if let Some(rsdp) = unsafe { self.config_table(ACPI_20_TABLE_GUID, map) }? {
            return Ok(Some(rsdp));
        }
        // SAFETY: The caller guarantees `map` returns a readable EFI
        // configuration table array.
        unsafe { self.config_table(ACPI_TABLE_GUID, map) }
    }
}

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct LinuxEfiBootMemmapHeader {
    pub map_size: usize,
    pub desc_size: usize,
    pub desc_ver: u32,
    pub map_key: usize,
    pub buff_size: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct BootMemmapRef<'a> {
    header: &'a LinuxEfiBootMemmapHeader,
    descriptors: &'a [u8],
}

impl<'a> BootMemmapRef<'a> {
    /// # Safety
    ///
    /// `ptr` must point to a readable Linux EFI boot memmap header followed by
    /// `map_size` bytes of descriptor data.
    pub unsafe fn from_ptr(ptr: *const LinuxEfiBootMemmapHeader) -> Result<Self, ParseError> {
        // SAFETY: The caller guarantees `ptr` points to a readable boot memmap
        // header for the lifetime of the returned view.
        let header = unsafe { ptr.as_ref() }.ok_or(ParseError::NullPointer)?;
        if header.desc_size < size_of::<MemoryDescriptor>()
            || header.buff_size < header.map_size
            || (header.desc_size != 0 && header.map_size % header.desc_size != 0)
        {
            return Err(ParseError::InvalidBootMemmap);
        }

        // SAFETY: `ptr` points to the boot memmap header, so advancing by the
        // header size reaches the first descriptor byte in the same blob.
        let desc_ptr = unsafe { ptr.cast::<u8>().add(size_of::<LinuxEfiBootMemmapHeader>()) };
        let descriptors = if header.map_size == 0 {
            &[]
        } else {
            // SAFETY: `desc_ptr` points to the descriptor payload immediately
            // following the validated header, and `map_size` bounds that payload.
            unsafe { slice::from_raw_parts(desc_ptr, header.map_size) }
        };

        Ok(Self {
            header,
            descriptors,
        })
    }

    #[must_use]
    pub const fn header(&self) -> &'a LinuxEfiBootMemmapHeader {
        self.header
    }

    #[must_use]
    pub fn entries(&self) -> BootMemmapIter<'a> {
        BootMemmapIter {
            remaining: self.descriptors,
            desc_size: self.header.desc_size,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct BootMemmapIter<'a> {
    remaining: &'a [u8],
    desc_size: usize,
}

impl<'a> Iterator for BootMemmapIter<'a> {
    type Item = MemoryDescriptorView;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }

        let desc_bytes = self.remaining.get(..self.desc_size)?;
        self.remaining = self.remaining.get(self.desc_size..)?;
        // SAFETY: Each chunk is `desc_size >= size_of::<MemoryDescriptor>()`
        // bytes long. `read_unaligned` copies the descriptor out without
        // creating an aligned reference into the raw byte blob.
        let descriptor =
            unsafe { ptr::read_unaligned(desc_bytes.as_ptr().cast::<MemoryDescriptor>()) };
        Some(MemoryDescriptorView { descriptor })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MemoryDescriptorView {
    descriptor: MemoryDescriptor,
}

impl MemoryDescriptorView {
    #[must_use]
    pub const fn descriptor(&self) -> &MemoryDescriptor {
        &self.descriptor
    }

    #[must_use]
    pub const fn memory_type(&self) -> MemoryType {
        self.descriptor.ty
    }

    #[must_use]
    pub const fn phys_start(&self) -> usize {
        self.descriptor.phys_start as usize
    }

    #[must_use]
    pub const fn phys_end(&self) -> usize {
        self.phys_start() + self.size()
    }

    #[must_use]
    pub const fn size(&self) -> usize {
        self.descriptor.page_count as usize * EFI_PAGE_SIZE
    }

    #[must_use]
    pub const fn kind(&self) -> MemoryRegionKind {
        classify_memory_type(self.descriptor.ty)
    }

    #[must_use]
    pub const fn type_name(&self) -> &'static str {
        memory_type_name(self.descriptor.ty)
    }

    #[must_use]
    pub const fn is_linear_mapping_candidate(&self) -> bool {
        self.kind().is_linear_mapping_candidate() && self.size() != 0
    }

    #[must_use]
    pub const fn is_usable_ram(&self) -> bool {
        matches!(self.kind(), MemoryRegionKind::Ram) && self.size() != 0
    }
}

#[must_use]
pub const fn classify_memory_type(memory_type: MemoryType) -> MemoryRegionKind {
    match memory_type.0 {
        1 | 2 | 3 | 4 | 7 => MemoryRegionKind::Ram,
        5 | 6 => MemoryRegionKind::RuntimeServices,
        9 | 10 => MemoryRegionKind::Acpi,
        14 => MemoryRegionKind::Persistent,
        8 | 15 => MemoryRegionKind::Unusable,
        11 => MemoryRegionKind::Mmio,
        12 => MemoryRegionKind::MmioPortSpace,
        13 => MemoryRegionKind::PalCode,
        _ => MemoryRegionKind::Reserved,
    }
}

#[must_use]
pub const fn memory_type_name(memory_type: MemoryType) -> &'static str {
    match memory_type.0 {
        0 => "reserved",
        1 => "loader code",
        2 => "loader data",
        3 => "boot services code",
        4 => "boot services data",
        5 => "runtime services code",
        6 => "runtime services data",
        7 => "conventional memory",
        8 => "unusable",
        9 => "acpi reclaimable",
        10 => "acpi nvs",
        11 => "mmio",
        12 => "mmio port space",
        13 => "pal code",
        14 => "persistent",
        15 => "unaccepted",
        0x7000_0000..=0x7fff_ffff => "oem reserved",
        0x8000_0000..=0xffff_ffff => "os loader reserved",
        _ => "firmware reserved",
    }
}

#[cfg(test)]
mod tests {
    use core::{ffi::c_void, ptr};

    use super::*;

    #[test]
    fn config_table_lookup_prefers_acpi_20() {
        let tables = [
            ConfigurationTable {
                vendor_guid: ACPI_TABLE_GUID,
                vendor_table: 0x1111usize as *mut c_void,
            },
            ConfigurationTable {
                vendor_guid: ACPI_20_TABLE_GUID,
                vendor_table: 0x2222usize as *mut c_void,
            },
            ConfigurationTable {
                vendor_guid: DEVICE_TREE_GUID,
                vendor_table: 0x3333usize as *mut c_void,
            },
        ];
        let system_table = SystemTable {
            number_of_configuration_table_entries: tables.len(),
            configuration_table: tables.as_ptr() as *mut ConfigurationTable,
            ..SystemTable::default()
        };

        let view = unsafe { RawEfiContext::from_ptr(&system_table) }.unwrap();
        let map = |addr| addr as *const ConfigurationTable;
        assert_eq!(unsafe { view.rsdp_addr(map) }.unwrap(), Some(0x2222));
        assert_eq!(unsafe { view.dtb_addr(map) }.unwrap(), Some(0x3333));
    }

    #[test]
    fn boot_memmap_iter_respects_descriptor_stride() {
        #[repr(C, align(8))]
        struct MemmapBlob {
            header: LinuxEfiBootMemmapHeader,
            descriptors: [u8; 96],
        }

        let mut blob = MemmapBlob {
            header: LinuxEfiBootMemmapHeader {
                map_size: 96,
                desc_size: 48,
                desc_ver: MemoryDescriptor::VERSION,
                map_key: 0,
                buff_size: 96,
            },
            descriptors: [0; 96],
        };

        let first = MemoryDescriptor {
            ty: MemoryType::CONVENTIONAL,
            phys_start: 0x1000,
            virt_start: 0,
            page_count: 2,
            att: 0,
        };
        let second = MemoryDescriptor {
            ty: MemoryType::ACPI_RECLAIM,
            phys_start: 0x8000,
            virt_start: 0,
            page_count: 3,
            att: 0,
        };

        unsafe {
            ptr::write(
                blob.descriptors.as_mut_ptr().cast::<MemoryDescriptor>(),
                first,
            );
            ptr::write(
                blob.descriptors
                    .as_mut_ptr()
                    .add(48)
                    .cast::<MemoryDescriptor>(),
                second,
            );
        }

        let memmap = unsafe { BootMemmapRef::from_ptr(&blob.header) }.unwrap();
        let mut entries = memmap.entries();
        let first = entries.next().unwrap();
        let second = entries.next().unwrap();
        assert!(entries.next().is_none());
        assert_eq!(first.phys_start(), 0x1000);
        assert_eq!(first.size(), 0x2000);
        assert_eq!(first.kind(), MemoryRegionKind::Ram);
        assert_eq!(second.phys_start(), 0x8000);
        assert_eq!(second.size(), 0x3000);
        assert_eq!(second.kind(), MemoryRegionKind::Acpi);
    }

    #[test]
    fn linear_mapping_filter_skips_mmio() {
        let descriptor = MemoryDescriptor {
            ty: MemoryType::MMIO,
            phys_start: 0x1000,
            virt_start: 0,
            page_count: 1,
            att: 0,
        };
        let view = MemoryDescriptorView { descriptor };
        assert!(!view.is_linear_mapping_candidate());
        assert_eq!(view.type_name(), "mmio");
    }
}

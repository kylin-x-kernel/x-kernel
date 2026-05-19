// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![cfg_attr(not(test), no_std)]

use kbuild_config::CPU_NUM;

#[cfg(target_arch = "aarch64")]
pub mod aarch64;
#[cfg(target_arch = "aarch64")]
pub use aarch64::init_boot_cpu_id_map;

#[cfg(target_arch = "x86_64")]
pub mod x86_64;
#[cfg(target_arch = "x86_64")]
pub use x86_64::init_boot_cpu_id_map;

#[cfg(target_arch = "riscv64")]
pub mod riscv64;
#[cfg(target_arch = "riscv64")]
pub use riscv64::init_boot_cpu_id_map;

#[cfg(target_arch = "loongarch64")]
pub mod loongarch64;
#[cfg(target_arch = "loongarch64")]
pub use loongarch64::init_boot_cpu_id_map;

#[cfg(target_arch = "aarch64")]
use self::aarch64 as imp;
#[cfg(target_arch = "loongarch64")]
use self::loongarch64 as imp;
#[cfg(target_arch = "riscv64")]
use self::riscv64 as imp;
#[cfg(target_arch = "x86_64")]
use self::x86_64 as imp;

const INVALID_RAW_CPU_ID: usize = usize::MAX;

#[cfg_attr(target_arch = "aarch64", unsafe(link_section = ".data"))]
pub static mut CPU_ID_MAP: CpuIdMap = CpuIdMap::new(INVALID_RAW_CPU_ID);

#[inline]
pub(crate) fn cpu_id_map_ptr() -> *const CpuIdMap {
    core::ptr::addr_of!(CPU_ID_MAP)
}

#[inline]
pub(crate) fn cpu_id_map_mut_ptr() -> *mut CpuIdMap {
    core::ptr::addr_of_mut!(CPU_ID_MAP)
}

#[inline]
pub(crate) fn cpu_map_initialized() -> bool {
    unsafe { CpuIdMap::is_initialized(cpu_id_map_ptr()) }
}

fn panic_missing_raw_cpu_id(logical_cpu_id: LogicalCpuId) -> ! {
    panic!(
        "missing raw cpu id mapping for logical cpu id {}",
        logical_cpu_id.as_usize()
    )
}

pub fn raw_cpu_id(logical_cpu_id: LogicalCpuId) -> RawCpuId {
    imp::ensure_runtime_cpu_id_map();

    unsafe { CpuIdMap::logical_to_raw(cpu_id_map_ptr(), logical_cpu_id) }
        .unwrap_or_else(|| panic_missing_raw_cpu_id(logical_cpu_id))
}

pub fn logical_cpu_id(raw_cpu_id: RawCpuId) -> Option<LogicalCpuId> {
    imp::ensure_runtime_cpu_id_map();

    let normalized_raw_cpu_id = imp::normalize_raw_id(raw_cpu_id);
    unsafe { CpuIdMap::raw_to_logical(cpu_id_map_ptr(), normalized_raw_cpu_id) }
}

pub fn for_each_present_logical_cpu(mut f: impl FnMut(LogicalCpuId)) {
    imp::ensure_runtime_cpu_id_map();

    let mut logical_cpu_index = 0;
    while logical_cpu_index < CPU_NUM {
        let logical_cpu_id = LogicalCpuId::new(logical_cpu_index);
        if unsafe { CpuIdMap::logical_to_raw(cpu_id_map_ptr(), logical_cpu_id) }.is_some() {
            f(logical_cpu_id);
        }
        logical_cpu_index += 1;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct LogicalCpuId(usize);

impl LogicalCpuId {
    #[inline]
    pub const fn new(id: usize) -> Self {
        Self(id)
    }

    #[inline]
    pub const fn as_usize(self) -> usize {
        self.0
    }
}

impl From<usize> for LogicalCpuId {
    #[inline]
    fn from(value: usize) -> Self {
        Self::new(value)
    }
}

impl From<LogicalCpuId> for usize {
    #[inline]
    fn from(value: LogicalCpuId) -> Self {
        value.as_usize()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct RawCpuId(usize);

impl RawCpuId {
    #[inline]
    pub const fn new(id: usize) -> Self {
        Self(id)
    }

    #[inline]
    pub const fn as_usize(self) -> usize {
        self.0
    }
}

impl From<usize> for RawCpuId {
    #[inline]
    fn from(value: usize) -> Self {
        Self::new(value)
    }
}

impl From<RawCpuId> for usize {
    #[inline]
    fn from(value: RawCpuId) -> Self {
        value.as_usize()
    }
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct CpuIdMap {
    raw_cpu_ids_by_logical: [usize; CPU_NUM],
    invalid_raw_cpu_id: usize,
}

impl CpuIdMap {
    #[inline]
    pub(crate) const fn new(invalid_raw_cpu_id: usize) -> Self {
        Self {
            raw_cpu_ids_by_logical: [invalid_raw_cpu_id; CPU_NUM],
            invalid_raw_cpu_id,
        }
    }

    /// Reports whether the logical-to-raw map has been populated.
    ///
    /// # Safety
    ///
    /// `map` must point to a valid [`CpuIdMap`] allocation that is readable at
    /// least for the first raw-id slot and the `invalid_raw_cpu_id` field.
    #[inline]
    pub(crate) unsafe fn is_initialized(map: *const Self) -> bool {
        // SAFETY: The caller guarantees that `map` points to a valid
        // boot-time CPU id map storage that is readable here.
        unsafe {
            core::ptr::addr_of!((*map).raw_cpu_ids_by_logical[0]).read()
                != core::ptr::addr_of!((*map).invalid_raw_cpu_id).read()
        }
    }

    /// Stores one raw CPU id at the given logical CPU index.
    ///
    /// # Safety
    ///
    /// `map` must point to a valid writable [`CpuIdMap`], and
    /// `logical_cpu_index` must be in bounds for `raw_cpu_ids_by_logical`.
    #[inline]
    pub(crate) unsafe fn insert(map: *mut Self, logical_cpu_index: usize, raw_cpu_id: RawCpuId) {
        // SAFETY: The caller guarantees that `map` points to valid CPU id map
        // storage and that `logical_cpu_index` is in range for the array.
        unsafe {
            core::ptr::addr_of_mut!((*map).raw_cpu_ids_by_logical[logical_cpu_index])
                .write(raw_cpu_id.as_usize());
        }
    }

    /// Loads logical-to-raw mappings from an iterator of raw CPU ids.
    ///
    /// Returns `true` when the iterator contains more CPUs than `CPU_NUM` and
    /// the extra entries were truncated.
    ///
    /// # Safety
    ///
    /// `map` must point to a valid writable [`CpuIdMap`]. The caller is also
    /// responsible for ensuring that reinitializing the map is valid for the
    /// current boot/runtime phase.
    pub(crate) unsafe fn from_raw_cpu_ids(
        map: *mut Self,
        raw_cpu_ids: impl IntoIterator<Item = RawCpuId>,
    ) -> bool {
        for (logical_cpu_index, raw_cpu_id) in raw_cpu_ids.into_iter().enumerate() {
            if logical_cpu_index >= CPU_NUM {
                return true;
            }

            // SAFETY: The map pointer and logical CPU index are validated by
            // the caller and the bounds check above.
            unsafe { Self::insert(map, logical_cpu_index, raw_cpu_id) };
        }

        false
    }

    #[cfg(any(
        target_arch = "aarch64",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    ))]
    /// Loads logical-to-raw mappings from enabled CPU nodes in a device tree.
    ///
    /// Returns `true` when the device tree describes more CPUs than `CPU_NUM`
    /// and the extra entries were truncated.
    ///
    /// # Safety
    ///
    /// `map` must point to a valid writable [`CpuIdMap`]. `fdt` must remain
    /// valid for the duration of the call.
    pub(crate) unsafe fn from_fdt(
        map: *mut Self,
        fdt: &of::LinuxFdt<'_>,
        normalize_raw_id: fn(RawCpuId) -> RawCpuId,
    ) -> bool {
        let raw_cpu_ids = of::enabled_cpu_nodes(fdt)
            .filter_map(of::cpu_node_reg)
            .map(|raw_cpu_id| normalize_raw_id(RawCpuId::new(raw_cpu_id as usize)));
        // SAFETY: The caller guarantees that `map` points to valid storage.
        unsafe { Self::from_raw_cpu_ids(map, raw_cpu_ids) }
    }

    #[cfg(target_arch = "x86_64")]
    /// Loads logical-to-raw mappings from enabled local APIC entries in MADT.
    ///
    /// Returns `true` when MADT describes more CPUs than `CPU_NUM` and the
    /// extra entries were truncated.
    ///
    /// # Safety
    ///
    /// `map` must point to a valid writable [`CpuIdMap`]. `entries` must come
    /// from a valid MADT iterator for the current platform.
    pub(crate) unsafe fn from_madt(
        map: *mut Self,
        entries: acpi::MadtEntryIter,
        normalize_raw_id: fn(RawCpuId) -> RawCpuId,
    ) -> bool {
        let raw_cpu_ids = entries.filter_map(|entry| match entry {
            acpi::MadtEntry::LocalApic(cpu) if cpu.enabled() => {
                Some(normalize_raw_id(RawCpuId::new(cpu.apic_id as usize)))
            }
            _ => None,
        });
        // SAFETY: The caller guarantees that `map` points to valid storage.
        unsafe { Self::from_raw_cpu_ids(map, raw_cpu_ids) }
    }

    /// Resolves a logical CPU id to the corresponding raw hardware CPU id.
    ///
    /// # Safety
    ///
    /// `map` must point to a valid readable [`CpuIdMap`].
    #[inline]
    pub(crate) unsafe fn logical_to_raw(
        map: *const Self,
        logical_cpu_id: LogicalCpuId,
    ) -> Option<RawCpuId> {
        let logical_cpu_index = logical_cpu_id.as_usize();
        if logical_cpu_index >= CPU_NUM {
            return None;
        }

        // SAFETY: The caller guarantees that `map` points to a valid CPU id
        // map storage that is readable for the checked logical CPU index.
        unsafe {
            let raw_cpu_id =
                core::ptr::addr_of!((*map).raw_cpu_ids_by_logical[logical_cpu_index]).read();
            let invalid_raw_cpu_id = core::ptr::addr_of!((*map).invalid_raw_cpu_id).read();
            (raw_cpu_id != invalid_raw_cpu_id).then(|| RawCpuId::new(raw_cpu_id))
        }
    }

    /// Resolves a raw hardware CPU id to the corresponding logical CPU id.
    ///
    /// # Safety
    ///
    /// `map` must point to a valid readable [`CpuIdMap`] for the full logical
    /// CPU range.
    #[inline]
    pub(crate) unsafe fn raw_to_logical(
        map: *const Self,
        raw_cpu_id: RawCpuId,
    ) -> Option<LogicalCpuId> {
        let mut logical_cpu_index = 0;
        while logical_cpu_index < CPU_NUM {
            // SAFETY: The caller guarantees that `map` points to a valid CPU
            // id map storage that is readable for the full logical CPU range.
            unsafe {
                if core::ptr::addr_of!((*map).raw_cpu_ids_by_logical[logical_cpu_index]).read()
                    == raw_cpu_id.as_usize()
                {
                    return Some(LogicalCpuId::new(logical_cpu_index));
                }
            }
            logical_cpu_index += 1;
        }
        None
    }
}

/// The kernel CPU mask type, parameterized by the configured maximum CPU count.
pub type KCpuMask = cpumask::CpuMask<{ CPU_NUM }>;

/// Extension trait adding [`LogicalCpuId`]-typed operations to [`KCpuMask`].
pub trait KCpuMaskExt {
    /// Sets the bit corresponding to `cpu_id`.
    fn set_logical(&mut self, cpu_id: LogicalCpuId, value: bool) -> bool;
    /// Gets the bit corresponding to `cpu_id`.
    fn get_logical(&self, cpu_id: LogicalCpuId) -> bool;
    /// Constructs a mask with a single bit set for `cpu_id`.
    fn one_shot_logical(cpu_id: LogicalCpuId) -> KCpuMask;
    /// Returns an iterator yielding [`LogicalCpuId`] values for all set bits.
    fn iter_logical(&self) -> LogicalCpuIdIter<'_>;
}

impl KCpuMaskExt for KCpuMask {
    fn set_logical(&mut self, cpu_id: LogicalCpuId, value: bool) -> bool {
        self.set(cpu_id.as_usize(), value)
    }

    fn get_logical(&self, cpu_id: LogicalCpuId) -> bool {
        self.get(cpu_id.as_usize())
    }

    fn one_shot_logical(cpu_id: LogicalCpuId) -> KCpuMask {
        KCpuMask::one_shot(cpu_id.as_usize())
    }

    fn iter_logical(&self) -> LogicalCpuIdIter<'_> {
        LogicalCpuIdIter {
            inner: self.into_iter(),
        }
    }
}

/// Iterator over set bits in a [`KCpuMask`], yielding [`LogicalCpuId`] values.
pub struct LogicalCpuIdIter<'a> {
    inner: cpumask::Iter<'a, { CPU_NUM }>,
}

impl<'a> Iterator for LogicalCpuIdIter<'a> {
    type Item = LogicalCpuId;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(LogicalCpuId::new)
    }
}

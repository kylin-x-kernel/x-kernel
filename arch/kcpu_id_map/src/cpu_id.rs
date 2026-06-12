// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::cell::UnsafeCell;

use kbuild_config::CPU_NUM;

const INVALID_RAW_CPU_ID: usize = usize::MAX;

#[cfg_attr(target_arch = "aarch64", unsafe(link_section = ".data"))]
static CPU_ID_MAP: CpuIdMapStorage = CpuIdMapStorage::new(INVALID_RAW_CPU_ID);

#[inline]
pub(crate) fn cpu_map_initialized() -> bool {
    CPU_ID_MAP.with(CpuIdMap::is_initialized)
}

#[cfg(any(
    target_arch = "aarch64",
    target_arch = "riscv64",
    target_arch = "loongarch64"
))]
pub(crate) fn load_cpu_id_map_from_fdt(
    fdt: &of::LinuxFdt<'_>,
    normalize_raw_id: fn(RawCpuId) -> RawCpuId,
) -> bool {
    CPU_ID_MAP.with_mut(|map| map.load_from_fdt(fdt, normalize_raw_id))
}

#[cfg(target_arch = "x86_64")]
pub(crate) fn load_cpu_id_map_from_madt(
    entries: acpi::MadtEntryIter,
    normalize_raw_id: fn(RawCpuId) -> RawCpuId,
) -> bool {
    CPU_ID_MAP.with_mut(|map| map.load_from_madt(entries, normalize_raw_id))
}

pub(crate) fn logical_to_raw(logical_cpu_id: LogicalCpuId) -> Option<RawCpuId> {
    CPU_ID_MAP.with(|map| map.logical_to_raw(logical_cpu_id))
}

pub(crate) fn raw_to_logical(raw_cpu_id: RawCpuId) -> Option<LogicalCpuId> {
    CPU_ID_MAP.with(|map| map.raw_to_logical(raw_cpu_id))
}

pub(crate) fn for_each_present_logical_cpu(mut f: impl FnMut(usize, LogicalCpuId, usize)) {
    CPU_ID_MAP.with(|map| map.for_each_present_logical_cpu(&mut f));
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

#[repr(transparent)]
struct CpuIdMapStorage {
    map: UnsafeCell<CpuIdMap>,
}

// SAFETY: CPU id map mutation is restricted to one initialization pass through
// `with_mut`; after initialization the map is read-only.
unsafe impl Sync for CpuIdMapStorage {}

impl CpuIdMapStorage {
    const fn new(invalid_raw_cpu_id: usize) -> Self {
        Self {
            map: UnsafeCell::new(CpuIdMap::new(invalid_raw_cpu_id)),
        }
    }

    fn map(&self) -> &CpuIdMap {
        // SAFETY: The map storage lives for the whole kernel lifetime. Shared
        // access is read-only and does not alias mutable access after
        // initialization.
        unsafe { &*self.map.get() }
    }

    fn with<R>(&self, f: impl FnOnce(&CpuIdMap) -> R) -> R {
        f(self.map())
    }

    fn with_mut<R>(&self, f: impl FnOnce(&mut CpuIdMap) -> R) -> R {
        assert!(
            !self.map().is_initialized(),
            "CPU id map is already initialized"
        );
        // SAFETY: CPU map loading helpers are used during initialization before
        // concurrent readers can observe or alias the map. The initialized
        // check above prevents later reinitialization through this storage API.
        unsafe { f(&mut *self.map.get()) }
    }
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct CpuIdMap {
    raw_cpu_ids_by_logical: [usize; CPU_NUM],
    present_count: usize,
    invalid_raw_cpu_id: usize,
}

impl CpuIdMap {
    #[inline]
    pub(crate) const fn new(invalid_raw_cpu_id: usize) -> Self {
        Self {
            raw_cpu_ids_by_logical: [invalid_raw_cpu_id; CPU_NUM],
            present_count: 0,
            invalid_raw_cpu_id,
        }
    }

    /// Reports whether the logical-to-raw map has been populated.
    #[inline]
    pub(crate) fn is_initialized(&self) -> bool {
        self.present_count != 0
    }

    /// Stores one raw CPU id at the given logical CPU index.
    #[inline]
    pub(crate) fn insert(&mut self, logical_cpu_index: usize, raw_cpu_id: RawCpuId) {
        if self.raw_cpu_ids_by_logical[logical_cpu_index] == self.invalid_raw_cpu_id {
            self.present_count += 1;
        }
        self.raw_cpu_ids_by_logical[logical_cpu_index] = raw_cpu_id.as_usize();
    }

    fn clear(&mut self) {
        self.raw_cpu_ids_by_logical.fill(self.invalid_raw_cpu_id);
        self.present_count = 0;
    }

    /// Loads logical-to-raw mappings from an iterator of raw CPU ids.
    ///
    /// Returns `true` when the iterator contains more CPUs than `CPU_NUM` and
    /// the extra entries were truncated.
    pub(crate) fn load_from_raw_cpu_ids(
        &mut self,
        raw_cpu_ids: impl IntoIterator<Item = RawCpuId>,
    ) -> bool {
        self.clear();
        for (logical_cpu_index, raw_cpu_id) in raw_cpu_ids.into_iter().enumerate() {
            if logical_cpu_index >= CPU_NUM {
                return true;
            }

            self.insert(logical_cpu_index, raw_cpu_id);
        }

        false
    }

    /// Loads logical-to-raw mappings from enabled CPU nodes in a device tree.
    ///
    /// Returns `true` when the device tree describes more CPUs than `CPU_NUM`
    /// and the extra entries were truncated.
    #[cfg(any(
        target_arch = "aarch64",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    ))]
    pub(crate) fn load_from_fdt(
        &mut self,
        fdt: &of::LinuxFdt<'_>,
        normalize_raw_id: fn(RawCpuId) -> RawCpuId,
    ) -> bool {
        let raw_cpu_ids = of::enabled_cpu_nodes(fdt)
            .filter_map(of::cpu_node_reg)
            .map(|raw_cpu_id| normalize_raw_id(RawCpuId::new(raw_cpu_id as usize)));
        self.load_from_raw_cpu_ids(raw_cpu_ids)
    }

    /// Loads logical-to-raw mappings from enabled local APIC entries in MADT.
    ///
    /// Returns `true` when MADT describes more CPUs than `CPU_NUM` and the
    /// extra entries were truncated.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn load_from_madt(
        &mut self,
        entries: acpi::MadtEntryIter,
        normalize_raw_id: fn(RawCpuId) -> RawCpuId,
    ) -> bool {
        let raw_cpu_ids = entries.filter_map(|entry| match entry {
            acpi::MadtEntry::LocalApic(cpu) if cpu.enabled() => {
                Some(normalize_raw_id(RawCpuId::new(cpu.apic_id as usize)))
            }
            _ => None,
        });
        self.load_from_raw_cpu_ids(raw_cpu_ids)
    }

    /// Resolves a logical CPU id to the corresponding raw hardware CPU id.
    #[inline]
    pub(crate) fn logical_to_raw(&self, logical_cpu_id: LogicalCpuId) -> Option<RawCpuId> {
        let logical_cpu_index = logical_cpu_id.as_usize();
        if logical_cpu_index >= CPU_NUM {
            return None;
        }

        let raw_cpu_id = self.raw_cpu_ids_by_logical[logical_cpu_index];
        (raw_cpu_id != self.invalid_raw_cpu_id).then(|| RawCpuId::new(raw_cpu_id))
    }

    /// Resolves a raw hardware CPU id to the corresponding logical CPU id.
    #[inline]
    pub(crate) fn raw_to_logical(&self, raw_cpu_id: RawCpuId) -> Option<LogicalCpuId> {
        let mut logical_cpu_index = 0;
        while logical_cpu_index < CPU_NUM {
            let logical_cpu_id = LogicalCpuId::new(logical_cpu_index);
            if self.logical_to_raw(logical_cpu_id) == Some(raw_cpu_id) {
                return Some(logical_cpu_id);
            }
            logical_cpu_index += 1;
        }
        None
    }

    /// Counts logical CPUs that have a raw CPU id mapping.
    pub(crate) fn present_count(&self) -> usize {
        self.present_count
    }

    pub(crate) fn for_each_present_logical_cpu(
        &self,
        mut f: impl FnMut(usize, LogicalCpuId, usize),
    ) {
        let present_count = self.present_count();
        let mut present_index = 0;

        for logical_cpu_index in 0..CPU_NUM {
            let logical_cpu_id = LogicalCpuId::new(logical_cpu_index);
            if self.logical_to_raw(logical_cpu_id).is_none() {
                continue;
            }

            f(present_index, logical_cpu_id, present_count);
            present_index += 1;
        }
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

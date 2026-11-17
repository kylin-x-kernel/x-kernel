// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicUsize, Ordering};

use kbuild_config::NR_CPUS;
use klazy::Once;

const INVALID_RAW_CPU_ID: usize = usize::MAX;

/// Number of machine words needed to hold one bit per configured CPU.
const ONLINE_MASK_WORDS: usize = NR_CPUS.div_ceil(usize::BITS as usize);

#[cfg_attr(target_arch = "aarch64", unsafe(link_section = ".data"))]
static CPU_ID_MAP: Once<CpuIdMap> = Once::new();
static UNINITIALIZED_CPU_ID_MAP: CpuIdMap = CpuIdMap::new(INVALID_RAW_CPU_ID);

#[inline]
pub(crate) fn cpu_map_initialized() -> bool {
    current_cpu_id_map().is_initialized()
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
    load_cpu_id_map(|map| map.load_from_fdt(fdt, normalize_raw_id))
}

#[cfg(target_arch = "x86_64")]
pub(crate) fn load_cpu_id_map_from_madt(
    entries: acpi::MadtEntryIter<'static>,
    normalize_raw_id: fn(RawCpuId) -> RawCpuId,
) -> bool {
    load_cpu_id_map(|map| map.load_from_madt(entries, normalize_raw_id))
}

pub(crate) fn logical_to_raw(logical_cpu_id: LogicalCpuId) -> Option<RawCpuId> {
    current_cpu_id_map().logical_to_raw(logical_cpu_id)
}

pub(crate) fn raw_to_logical(raw_cpu_id: RawCpuId) -> Option<LogicalCpuId> {
    current_cpu_id_map().raw_to_logical(raw_cpu_id)
}

pub(crate) fn for_each_present_logical_cpu(mut f: impl FnMut(usize, LogicalCpuId, usize)) {
    current_cpu_id_map().for_each_present_logical_cpu(&mut f);
}

pub(crate) fn for_each_online_logical_cpu(mut f: impl FnMut(usize, LogicalCpuId, usize)) {
    current_cpu_id_map().for_each_online_logical_cpu(&mut f);
}

pub(crate) fn is_online(logical_cpu_id: LogicalCpuId) -> bool {
    current_cpu_id_map().is_online(logical_cpu_id)
}

pub(crate) fn set_online(logical_cpu_id: LogicalCpuId, is_online: bool) -> Option<bool> {
    let map = current_cpu_id_map();
    map.logical_to_raw(logical_cpu_id)?;
    Some(map.set_online(logical_cpu_id, is_online))
}

pub(crate) fn online_count() -> usize {
    current_cpu_id_map().online_count()
}

/// Returns the number of present logical CPUs discovered from the device tree
/// or ACPI MADT. Returns 0 before the CPU id map has been populated.
pub(crate) fn present_count() -> usize {
    current_cpu_id_map().present_count()
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
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

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
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

fn current_cpu_id_map() -> &'static CpuIdMap {
    CPU_ID_MAP.get().unwrap_or(&UNINITIALIZED_CPU_ID_MAP)
}

pub(crate) fn load_cpu_id_map(load: impl FnOnce(&mut CpuIdMap) -> bool) -> bool {
    assert!(
        CPU_ID_MAP.get().is_none(),
        "CPU id map is already initialized"
    );

    let mut map = CpuIdMap::new(INVALID_RAW_CPU_ID);
    let truncated = load(&mut map);
    CPU_ID_MAP.call_once(|| map);
    truncated
}

#[repr(C)]
pub(crate) struct CpuIdMap {
    raw_cpu_ids_by_logical: [usize; NR_CPUS],
    /// One bit per logical CPU. Present CPUs start online and only an
    /// explicit hotplug request clears a bit, so this is *not* derivable
    /// from `raw_cpu_ids_by_logical`.
    online_words: [AtomicUsize; ONLINE_MASK_WORDS],
    present_count: usize,
    invalid_raw_cpu_id: usize,
}

impl CpuIdMap {
    #[inline]
    pub(crate) const fn new(invalid_raw_cpu_id: usize) -> Self {
        Self {
            raw_cpu_ids_by_logical: [invalid_raw_cpu_id; NR_CPUS],
            online_words: [const { AtomicUsize::new(0) }; ONLINE_MASK_WORDS],
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
        // Every CPU described by the firmware starts online; hotplug can take
        // it offline later.
        self.online_words[logical_cpu_index / usize::BITS as usize].fetch_or(
            1 << (logical_cpu_index % usize::BITS as usize),
            Ordering::Relaxed,
        );
    }

    fn clear(&mut self) {
        self.raw_cpu_ids_by_logical.fill(self.invalid_raw_cpu_id);
        for word in &self.online_words {
            word.store(0, Ordering::Relaxed);
        }
        self.present_count = 0;
    }

    /// Loads logical-to-raw mappings from an iterator of raw CPU ids.
    ///
    /// Returns `true` when the iterator contains more CPUs than `NR_CPUS` and
    /// the extra entries were truncated.
    pub(crate) fn load_from_raw_cpu_ids(
        &mut self,
        raw_cpu_ids: impl IntoIterator<Item = RawCpuId>,
    ) -> bool {
        self.clear();
        for (logical_cpu_index, raw_cpu_id) in raw_cpu_ids.into_iter().enumerate() {
            if logical_cpu_index >= NR_CPUS {
                return true;
            }

            self.insert(logical_cpu_index, raw_cpu_id);
        }

        false
    }

    /// Loads logical-to-raw mappings from enabled CPU nodes in a device tree.
    ///
    /// Returns `true` when the device tree describes more CPUs than `NR_CPUS`
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
    /// Returns `true` when MADT describes more CPUs than `NR_CPUS` and the
    /// extra entries were truncated.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn load_from_madt(
        &mut self,
        entries: acpi::MadtEntryIter<'static>,
        normalize_raw_id: fn(RawCpuId) -> RawCpuId,
    ) -> bool {
        let raw_cpu_ids = entries.filter_map(|entry| match entry {
            Ok(acpi::MadtEntry::LocalApic(cpu)) if cpu.enabled() => {
                Some(normalize_raw_id(RawCpuId::new(cpu.apic_id as usize)))
            }
            Ok(_) => None,
            Err(error) => {
                log::warn!(
                    "ACPI MADT parse error at offset {}: {:?}; remaining CPUs ignored",
                    error.offset,
                    error.kind
                );
                None
            }
        });
        self.load_from_raw_cpu_ids(raw_cpu_ids)
    }

    /// Resolves a logical CPU id to the corresponding raw hardware CPU id.
    #[inline]
    pub(crate) fn logical_to_raw(&self, logical_cpu_id: LogicalCpuId) -> Option<RawCpuId> {
        let logical_cpu_index = logical_cpu_id.as_usize();
        if logical_cpu_index >= NR_CPUS {
            return None;
        }

        let raw_cpu_id = self.raw_cpu_ids_by_logical[logical_cpu_index];
        (raw_cpu_id != self.invalid_raw_cpu_id).then(|| RawCpuId::new(raw_cpu_id))
    }

    /// Resolves a raw hardware CPU id to the corresponding logical CPU id.
    #[inline]
    pub(crate) fn raw_to_logical(&self, raw_cpu_id: RawCpuId) -> Option<LogicalCpuId> {
        let mut logical_cpu_index = 0;
        while logical_cpu_index < NR_CPUS {
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

    /// Reports whether a present logical CPU is currently online.
    #[inline]
    pub(crate) fn is_online(&self, logical_cpu_id: LogicalCpuId) -> bool {
        let logical_cpu_index = logical_cpu_id.as_usize();
        if logical_cpu_index >= NR_CPUS {
            return false;
        }

        self.online_words[logical_cpu_index / usize::BITS as usize].load(Ordering::Relaxed)
            & (1 << (logical_cpu_index % usize::BITS as usize))
            != 0
    }

    /// Sets the online bit and returns its previous value.
    ///
    /// The caller must already have established that the CPU is present;
    /// presence is immutable here because it comes from firmware tables.
    pub(crate) fn set_online(&self, logical_cpu_id: LogicalCpuId, is_online: bool) -> bool {
        let logical_cpu_index = logical_cpu_id.as_usize();
        if logical_cpu_index >= NR_CPUS {
            return false;
        }
        let bit = 1 << (logical_cpu_index % usize::BITS as usize);
        let word = &self.online_words[logical_cpu_index / usize::BITS as usize];
        let previous = if is_online {
            word.fetch_or(bit, Ordering::AcqRel)
        } else {
            word.fetch_and(!bit, Ordering::AcqRel)
        };
        previous & bit != 0
    }

    /// Counts logical CPUs that are currently online.
    pub(crate) fn online_count(&self) -> usize {
        self.online_words
            .iter()
            .map(|word| word.load(Ordering::Relaxed).count_ones() as usize)
            .sum()
    }

    pub(crate) fn for_each_online_logical_cpu(
        &self,
        mut f: impl FnMut(usize, LogicalCpuId, usize),
    ) {
        let online_count = self.online_count();
        let mut online_index = 0;

        for logical_cpu_index in 0..NR_CPUS {
            let logical_cpu_id = LogicalCpuId::new(logical_cpu_index);
            if !self.is_online(logical_cpu_id) {
                continue;
            }

            f(online_index, logical_cpu_id, online_count);
            online_index += 1;
        }
    }

    pub(crate) fn for_each_present_logical_cpu(
        &self,
        mut f: impl FnMut(usize, LogicalCpuId, usize),
    ) {
        let present_count = self.present_count();
        let mut present_index = 0;

        for logical_cpu_index in 0..NR_CPUS {
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
pub type KCpuMask = cpumask::CpuMask<{ NR_CPUS }>;

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
    inner: cpumask::Iter<'a, { NR_CPUS }>,
}

impl<'a> Iterator for LogicalCpuIdIter<'a> {
    type Item = LogicalCpuId;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(LogicalCpuId::new)
    }
}

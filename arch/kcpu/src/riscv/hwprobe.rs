// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RISC-V CPU capability snapshot for Linux `riscv_hwprobe`.

use kcpu_id_map::{KCpuMask, KCpuMaskExt, LogicalCpuId, RawCpuId};
use klazy::Once;

const IMA_EXT_FD: u64 = 1 << 0;
const IMA_EXT_C: u64 = 1 << 1;
const IMA_EXT_V: u64 = 1 << 2;
const IMA_EXT_ZBA: u64 = 1 << 3;
const IMA_EXT_ZBB: u64 = 1 << 4;
const IMA_EXT_ZBS: u64 = 1 << 5;
const IMA_EXT_ZICBOZ: u64 = 1 << 6;
const IMA_EXT_ZBC: u64 = 1 << 7;
const IMA_EXT_ZBKB: u64 = 1 << 8;
const IMA_EXT_ZBKC: u64 = 1 << 9;
const IMA_EXT_ZBKX: u64 = 1 << 10;
const IMA_EXT_ZKND: u64 = 1 << 11;
const IMA_EXT_ZKNE: u64 = 1 << 12;
const IMA_EXT_ZKNH: u64 = 1 << 13;
const IMA_EXT_ZKSED: u64 = 1 << 14;
const IMA_EXT_ZKSH: u64 = 1 << 15;
const IMA_EXT_ZKT: u64 = 1 << 16;
const IMA_EXT_ZVBB: u64 = 1 << 17;
const IMA_EXT_ZVBC: u64 = 1 << 18;
const IMA_EXT_ZVKB: u64 = 1 << 19;
const IMA_EXT_ZVKG: u64 = 1 << 20;
const IMA_EXT_ZVKNED: u64 = 1 << 21;
const IMA_EXT_ZVKNHA: u64 = 1 << 22;
const IMA_EXT_ZVKNHB: u64 = 1 << 23;
const IMA_EXT_ZVKSED: u64 = 1 << 24;
const IMA_EXT_ZVKSH: u64 = 1 << 25;
const IMA_EXT_ZVKT: u64 = 1 << 26;
const IMA_EXT_ZFH: u64 = 1 << 27;
const IMA_EXT_ZFHMIN: u64 = 1 << 28;
const IMA_EXT_ZIHINTNTL: u64 = 1 << 29;
const IMA_EXT_ZVFH: u64 = 1 << 30;
const IMA_EXT_ZVFHMIN: u64 = 1 << 31;
const IMA_EXT_ZFA: u64 = 1 << 32;
const IMA_EXT_ZTSO: u64 = 1 << 33;
const IMA_EXT_ZACAS: u64 = 1 << 34;
const IMA_EXT_ZICOND: u64 = 1 << 35;
const IMA_EXT_ZIHINTPAUSE: u64 = 1 << 36;
const IMA_EXT_ZVE32X: u64 = 1 << 37;
const IMA_EXT_ZVE32F: u64 = 1 << 38;
const IMA_EXT_ZVE64X: u64 = 1 << 39;
const IMA_EXT_ZVE64F: u64 = 1 << 40;
const IMA_EXT_ZVE64D: u64 = 1 << 41;
const IMA_EXT_ZIMOP: u64 = 1 << 42;
const IMA_EXT_ZCA: u64 = 1 << 43;
const IMA_EXT_ZCB: u64 = 1 << 44;
const IMA_EXT_ZCD: u64 = 1 << 45;
const IMA_EXT_ZCF: u64 = 1 << 46;
const IMA_EXT_ZCMOP: u64 = 1 << 47;

const BASE_BEHAVIOR_IMA: u64 = 1 << 0;
const MISALIGNED_SCALAR_UNKNOWN: u64 = 0;

const RISCV_HWPROBE_KEY_MVENDORID: i64 = 0;
const RISCV_HWPROBE_KEY_MARCHID: i64 = 1;
const RISCV_HWPROBE_KEY_MIMPID: i64 = 2;
const RISCV_HWPROBE_KEY_BASE_BEHAVIOR: i64 = 3;
const RISCV_HWPROBE_KEY_IMA_EXT_0: i64 = 4;
const RISCV_HWPROBE_KEY_CPUPERF_0: i64 = 5;
const RISCV_HWPROBE_KEY_ZICBOZ_BLOCK_SIZE: i64 = 6;
const RISCV_HWPROBE_KEY_HIGHEST_VIRT_ADDRESS: i64 = 7;
const RISCV_HWPROBE_KEY_TIME_CSR_FREQ: i64 = 8;
const RISCV_HWPROBE_KEY_MISALIGNED_SCALAR_PERF: i64 = 9;
const RISCV_HWPROBE_KEY_MISALIGNED_VECTOR_PERF: i64 = 10;
const RISCV_HWPROBE_KEY_VENDOR_EXT_THEAD_0: i64 = 11;
const RISCV_HWPROBE_KEY_ZICBOM_BLOCK_SIZE: i64 = 12;
const RISCV_HWPROBE_KEY_VENDOR_EXT_SIFIVE_0: i64 = 13;
const RISCV_HWPROBE_KEY_VENDOR_EXT_MIPS_0: i64 = 14;
const RISCV_HWPROBE_KEY_ZICBOP_BLOCK_SIZE: i64 = 15;
const RISCV_HWPROBE_KEY_IMA_EXT_1: i64 = 16;

/// Linux raw key value used to mark an unknown `riscv_hwprobe` pair.
pub const RISCV_HWPROBE_UNKNOWN_KEY: i64 = -1;

#[derive(Clone, Copy)]
enum HwprobeKey {
    Mvendorid,
    Marchid,
    Mimpid,
    BaseBehavior,
    ImaExt0,
    Cpuperf0,
    ZicbozBlockSize,
    HighestVirtualAddress,
    TimeCsrFreq,
    MisalignedScalarPerf,
    MisalignedVectorPerf,
    VendorExtThead0,
    ZicbomBlockSize,
    VendorExtSifive0,
    VendorExtMips0,
    ZicbopBlockSize,
    ImaExt1,
}

struct RiscvHwprobe {
    cpus: Once<[RiscvCpuHwprobe; kbuild_config::NR_CPUS]>,
}

static RISCV_HWPROBE: RiscvHwprobe = RiscvHwprobe::new();

/// Initializes the RISC-V hwprobe CPU snapshot from the firmware device tree.
pub fn init_hwprobe_from_fdt(fdt: &of::LinuxFdt<'_>) {
    RISCV_HWPROBE.init_from_fdt(fdt);
}

/// Returns whether `raw_key` is a recognized Linux `riscv_hwprobe` key.
///
/// Used by the syscall adapter to distinguish unknown keys (reported back as
/// `key = -1, value = 0`) from known ones without performing an aggregation.
pub fn hwprobe_key_is_known(raw_key: i64) -> bool {
    hwprobe_key(raw_key).is_some()
}

/// Aggregates a Linux `riscv_hwprobe` value across `cpu_mask`.
///
/// Returns `None` when `raw_key` is not a recognized key, in which case the
/// caller must report the pair back as `key = -1, value = 0`. The aggregation
/// rule depends on the key class:
///
/// - Bitmask keys (`BASE_BEHAVIOR`, `IMA_EXT_0`/`IMA_EXT_1`, `CPUPERF_0`,
///   vendor-extension keys) are combined with a bitwise AND: the result reports
///   the bits set on **every** selected CPU.
/// - Architecture-ID keys (`MVENDORID`, `MARCHID`, `MIMPID`) require every
///   selected CPU to report the same value; on mismatch the result is
///   `u64::MAX`, matching Linux `hwprobe_arch_id`'s `-1ULL`.
/// - The remaining scalar keys (block sizes, `TIME_CSR_FREQ`,
///   `HIGHEST_VIRT_ADDRESS`) are aggregated the same way but assume a uniform
///   boot snapshot; Linux exposes them as global values, so a hypothetical
///   heterogeneous snapshot is a known divergence.
pub fn hwprobe_aggregate_value(raw_key: i64, cpu_mask: &KCpuMask) -> Option<u64> {
    RISCV_HWPROBE.aggregate_value(raw_key, cpu_mask)
}

/// Returns whether `cpu_id` satisfies a `RISCV_HWPROBE_WHICH_CPUS` request.
///
/// Returns `None` when `raw_key` is not a recognized key. The match rule
/// mirrors Linux `riscv_hwprobe_pair_cmp` and depends on the key class:
///
/// - Bitmask keys match when `value & requested == requested` (the CPU offers
///   every requested bit).
/// - Scalar keys match when `value == requested`.
pub fn hwprobe_cpu_matches(raw_key: i64, cpu_id: LogicalCpuId, requested: u64) -> Option<bool> {
    RISCV_HWPROBE.cpu_matches(raw_key, cpu_id, requested)
}

impl RiscvHwprobe {
    const fn new() -> Self {
        Self { cpus: Once::new() }
    }

    fn init_from_fdt(&self, fdt: &of::LinuxFdt<'_>) {
        if self.cpus.get().is_some() {
            return;
        }

        let mut cpus = [RiscvCpuHwprobe::conservative_default(); kbuild_config::NR_CPUS];

        for node in of::enabled_cpu_nodes(fdt) {
            let Some(raw_cpu_id) = of::cpu_node_reg(node) else {
                continue;
            };
            let Some(logical_cpu_id) =
                kcpu_id_map::logical_cpu_id(RawCpuId::new(raw_cpu_id as usize))
            else {
                continue;
            };
            let index = logical_cpu_id.as_usize();
            if index >= kbuild_config::NR_CPUS {
                continue;
            }

            cpus[index] = parse_cpu_node(node);
        }

        self.cpus.call_once(|| cpus);
    }

    fn aggregate_value(&self, raw_key: i64, cpu_mask: &KCpuMask) -> Option<u64> {
        let key = hwprobe_key(raw_key)?;
        let mut value = None;
        for cpu in cpu_mask.iter_logical() {
            let cpu_value = key.cpu_value(self, cpu);
            value = Some(match value {
                None => cpu_value,
                Some(prev) => key.aggregate_pair(prev, cpu_value),
            });
        }

        Some(value.unwrap_or(0))
    }

    fn cpu_matches(&self, raw_key: i64, cpu_id: LogicalCpuId, requested: u64) -> Option<bool> {
        let key = hwprobe_key(raw_key)?;
        Some(key.cpu_matches(self, cpu_id, requested))
    }

    fn cpu_snapshot(&self, cpu_id: LogicalCpuId) -> RiscvCpuHwprobe {
        self.ensure_initialized();
        self.cpus
            .get()
            .and_then(|cpus| cpus.get(cpu_id.as_usize()).copied())
            .unwrap_or_else(RiscvCpuHwprobe::conservative_default)
    }

    fn ensure_initialized(&self) {
        if self.cpus.get().is_some() {
            return;
        }

        self.cpus
            .call_once(|| [RiscvCpuHwprobe::conservative_default(); kbuild_config::NR_CPUS]);
    }
}

impl HwprobeKey {
    /// Whether this key is a bitmask (set-membership) key.
    ///
    /// Bitmask keys are aggregated with a bitwise AND and matched with
    /// `value & requested == requested`. Every other key is scalar: aggregated
    /// by equality (mismatch yields `u64::MAX`) and matched with
    /// `value == requested`. Centralizing the classification keeps the two
    /// rule sites in sync as new keys are added.
    fn is_bitmask_aggregate(self) -> bool {
        // Matches Linux `hwprobe_key_is_bitmask` (arch/riscv/include/asm/hwprobe.h).
        // CPUPERF_0 is a bitmask key; MISALIGNED_*_PERF is not.
        matches!(
            self,
            Self::BaseBehavior
                | Self::ImaExt0
                | Self::ImaExt1
                | Self::Cpuperf0
                | Self::VendorExtThead0
                | Self::VendorExtSifive0
                | Self::VendorExtMips0
        )
    }

    fn aggregate_pair(self, prev: u64, next: u64) -> u64 {
        if self.is_bitmask_aggregate() {
            prev & next
        } else if prev == next {
            prev
        } else {
            u64::MAX
        }
    }

    fn cpu_matches(self, hwprobe: &RiscvHwprobe, cpu_id: LogicalCpuId, requested: u64) -> bool {
        let value = self.cpu_value(hwprobe, cpu_id);
        if self.is_bitmask_aggregate() {
            value & requested == requested
        } else {
            value == requested
        }
    }

    fn cpu_value(self, hwprobe: &RiscvHwprobe, cpu_id: LogicalCpuId) -> u64 {
        let cpu = hwprobe.cpu_snapshot(cpu_id);
        match self {
            Self::Mvendorid => cpu.mvendorid,
            Self::Marchid => cpu.marchid,
            Self::Mimpid => cpu.mimpid,
            Self::BaseBehavior => cpu.base_behavior,
            Self::ImaExt0 => cpu.ima_ext_0,
            Self::Cpuperf0 | Self::MisalignedScalarPerf => cpu.cpuperf_0,
            Self::ZicbozBlockSize => cpu.zicboz_block_size,
            Self::HighestVirtualAddress => cpu.highest_virtual_address,
            Self::TimeCsrFreq => cpu.time_csr_freq,
            Self::MisalignedVectorPerf
            | Self::VendorExtThead0
            | Self::ZicbomBlockSize
            | Self::VendorExtSifive0
            | Self::VendorExtMips0
            | Self::ZicbopBlockSize
            | Self::ImaExt1 => 0,
        }
    }
}

fn hwprobe_key(raw_key: i64) -> Option<HwprobeKey> {
    let key = match raw_key {
        RISCV_HWPROBE_KEY_MVENDORID => HwprobeKey::Mvendorid,
        RISCV_HWPROBE_KEY_MARCHID => HwprobeKey::Marchid,
        RISCV_HWPROBE_KEY_MIMPID => HwprobeKey::Mimpid,
        RISCV_HWPROBE_KEY_BASE_BEHAVIOR => HwprobeKey::BaseBehavior,
        RISCV_HWPROBE_KEY_IMA_EXT_0 => HwprobeKey::ImaExt0,
        RISCV_HWPROBE_KEY_CPUPERF_0 => HwprobeKey::Cpuperf0,
        RISCV_HWPROBE_KEY_ZICBOZ_BLOCK_SIZE => HwprobeKey::ZicbozBlockSize,
        RISCV_HWPROBE_KEY_HIGHEST_VIRT_ADDRESS => HwprobeKey::HighestVirtualAddress,
        RISCV_HWPROBE_KEY_TIME_CSR_FREQ => HwprobeKey::TimeCsrFreq,
        RISCV_HWPROBE_KEY_MISALIGNED_SCALAR_PERF => HwprobeKey::MisalignedScalarPerf,
        RISCV_HWPROBE_KEY_MISALIGNED_VECTOR_PERF => HwprobeKey::MisalignedVectorPerf,
        RISCV_HWPROBE_KEY_VENDOR_EXT_THEAD_0 => HwprobeKey::VendorExtThead0,
        RISCV_HWPROBE_KEY_ZICBOM_BLOCK_SIZE => HwprobeKey::ZicbomBlockSize,
        RISCV_HWPROBE_KEY_VENDOR_EXT_SIFIVE_0 => HwprobeKey::VendorExtSifive0,
        RISCV_HWPROBE_KEY_VENDOR_EXT_MIPS_0 => HwprobeKey::VendorExtMips0,
        RISCV_HWPROBE_KEY_ZICBOP_BLOCK_SIZE => HwprobeKey::ZicbopBlockSize,
        RISCV_HWPROBE_KEY_IMA_EXT_1 => HwprobeKey::ImaExt1,
        _ => return None,
    };
    Some(key)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RiscvCpuHwprobe {
    mvendorid: u64,
    marchid: u64,
    mimpid: u64,
    base_behavior: u64,
    ima_ext_0: u64,
    cpuperf_0: u64,
    time_csr_freq: u64,
    zicboz_block_size: u64,
    highest_virtual_address: u64,
}

impl RiscvCpuHwprobe {
    const fn conservative_default() -> Self {
        Self {
            mvendorid: 0,
            marchid: 0,
            mimpid: 0,
            base_behavior: BASE_BEHAVIOR_IMA,
            ima_ext_0: 0,
            cpuperf_0: MISALIGNED_SCALAR_UNKNOWN,
            time_csr_freq: 0,
            zicboz_block_size: 0,
            highest_virtual_address: highest_user_address() as u64,
        }
    }
}

const fn highest_user_address() -> usize {
    kaddr_layout::USER_SPACE_BASE + kaddr_layout::USER_SPACE_SIZE - 1
}

fn parse_cpu_node(node: of::FdtNode<'_, '_>) -> RiscvCpuHwprobe {
    let mut cpu = RiscvCpuHwprobe::conservative_default();

    cpu.mvendorid = property_u64(node, "riscv,mvendorid").unwrap_or(0);
    cpu.marchid = property_u64(node, "riscv,marchid").unwrap_or(0);
    cpu.mimpid = property_u64(node, "riscv,mimpid").unwrap_or(0);
    cpu.time_csr_freq = parent_property_u64(node, "timebase-frequency").unwrap_or(0);

    if let Some(isa) = node.property_str("riscv,isa") {
        cpu.ima_ext_0 |= parse_legacy_isa(isa);
    }

    if let Some(exts) = node.property("riscv,isa-extensions") {
        cpu.ima_ext_0 |= parse_extension_list(exts.value);
    }

    cpu.zicboz_block_size = property_u64(node, "riscv,cboz-block-size")
        .or_else(|| property_u64(node, "cboz-block-size"))
        .unwrap_or(0);

    cpu
}

fn property_u64(node: of::FdtNode<'_, '_>, name: &str) -> Option<u64> {
    let value = node.property(name)?.value;
    parse_u64_cells(value)
}

fn parent_property_u64(node: of::FdtNode<'_, '_>, name: &str) -> Option<u64> {
    let value = node.parent_property(name)?.value;
    parse_u64_cells(value)
}

fn parse_u64_cells(value: &[u8]) -> Option<u64> {
    match value.len() {
        4 => Some(u32::from_be_bytes(value.try_into().ok()?) as u64),
        8 => Some(u64::from_be_bytes(value.try_into().ok()?)),
        _ => None,
    }
}

fn parse_extension_list(mut value: &[u8]) -> u64 {
    let mut bits = 0;
    while !value.is_empty() {
        let Some(end) = value.iter().position(|&byte| byte == 0) else {
            break;
        };
        if let Ok(ext) = core::str::from_utf8(&value[..end]) {
            bits |= extension_bit(ext);
        }
        value = &value[end + 1..];
    }
    bits
}

fn parse_legacy_isa(isa: &str) -> u64 {
    let mut bits = 0;
    let mut has_f = false;
    let mut has_d = false;
    let bytes = isa.as_bytes();
    let mut index = if bytes.len() >= 4 { 4 } else { 0 };

    while index < bytes.len() {
        match bytes[index].to_ascii_lowercase() {
            b'f' => has_f = true,
            b'd' => has_d = true,
            b'c' => bits |= IMA_EXT_C,
            b'v' => bits |= IMA_EXT_V,
            b'z' | b's' | b'x' => {
                let start = index;
                index += 1;
                while index < bytes.len() && bytes[index] != b'_' {
                    index += 1;
                }
                bits |= extension_bit(&isa[start..index]);
                continue;
            }
            _ => {}
        }
        index += 1;
    }

    if has_f && has_d {
        bits |= IMA_EXT_FD;
    }
    bits
}

fn extension_bit(ext: &str) -> u64 {
    match ext {
        _ if ext.eq_ignore_ascii_case("zba") => IMA_EXT_ZBA,
        _ if ext.eq_ignore_ascii_case("zbb") => IMA_EXT_ZBB,
        _ if ext.eq_ignore_ascii_case("zbs") => IMA_EXT_ZBS,
        _ if ext.eq_ignore_ascii_case("zicboz") => IMA_EXT_ZICBOZ,
        _ if ext.eq_ignore_ascii_case("zbc") => IMA_EXT_ZBC,
        _ if ext.eq_ignore_ascii_case("zbkb") => IMA_EXT_ZBKB,
        _ if ext.eq_ignore_ascii_case("zbkc") => IMA_EXT_ZBKC,
        _ if ext.eq_ignore_ascii_case("zbkx") => IMA_EXT_ZBKX,
        _ if ext.eq_ignore_ascii_case("zknd") => IMA_EXT_ZKND,
        _ if ext.eq_ignore_ascii_case("zkne") => IMA_EXT_ZKNE,
        _ if ext.eq_ignore_ascii_case("zknh") => IMA_EXT_ZKNH,
        _ if ext.eq_ignore_ascii_case("zksed") => IMA_EXT_ZKSED,
        _ if ext.eq_ignore_ascii_case("zksh") => IMA_EXT_ZKSH,
        _ if ext.eq_ignore_ascii_case("zkt") => IMA_EXT_ZKT,
        _ if ext.eq_ignore_ascii_case("zvbb") => IMA_EXT_ZVBB,
        _ if ext.eq_ignore_ascii_case("zvbc") => IMA_EXT_ZVBC,
        _ if ext.eq_ignore_ascii_case("zvkb") => IMA_EXT_ZVKB,
        _ if ext.eq_ignore_ascii_case("zvkg") => IMA_EXT_ZVKG,
        _ if ext.eq_ignore_ascii_case("zvkned") => IMA_EXT_ZVKNED,
        _ if ext.eq_ignore_ascii_case("zvknha") => IMA_EXT_ZVKNHA,
        _ if ext.eq_ignore_ascii_case("zvknhb") => IMA_EXT_ZVKNHB,
        _ if ext.eq_ignore_ascii_case("zvksed") => IMA_EXT_ZVKSED,
        _ if ext.eq_ignore_ascii_case("zvksh") => IMA_EXT_ZVKSH,
        _ if ext.eq_ignore_ascii_case("zvkt") => IMA_EXT_ZVKT,
        _ if ext.eq_ignore_ascii_case("zfh") => IMA_EXT_ZFH,
        _ if ext.eq_ignore_ascii_case("zfhmin") => IMA_EXT_ZFHMIN,
        _ if ext.eq_ignore_ascii_case("zihintntl") => IMA_EXT_ZIHINTNTL,
        _ if ext.eq_ignore_ascii_case("zvfh") => IMA_EXT_ZVFH,
        _ if ext.eq_ignore_ascii_case("zvfhmin") => IMA_EXT_ZVFHMIN,
        _ if ext.eq_ignore_ascii_case("zfa") => IMA_EXT_ZFA,
        _ if ext.eq_ignore_ascii_case("ztso") => IMA_EXT_ZTSO,
        _ if ext.eq_ignore_ascii_case("zacas") => IMA_EXT_ZACAS,
        _ if ext.eq_ignore_ascii_case("zicond") => IMA_EXT_ZICOND,
        _ if ext.eq_ignore_ascii_case("zihintpause") => IMA_EXT_ZIHINTPAUSE,
        _ if ext.eq_ignore_ascii_case("zve32x") => IMA_EXT_ZVE32X,
        _ if ext.eq_ignore_ascii_case("zve32f") => IMA_EXT_ZVE32F,
        _ if ext.eq_ignore_ascii_case("zve64x") => IMA_EXT_ZVE64X,
        _ if ext.eq_ignore_ascii_case("zve64f") => IMA_EXT_ZVE64F,
        _ if ext.eq_ignore_ascii_case("zve64d") => IMA_EXT_ZVE64D,
        _ if ext.eq_ignore_ascii_case("zimop") => IMA_EXT_ZIMOP,
        _ if ext.eq_ignore_ascii_case("zca") => IMA_EXT_ZCA,
        _ if ext.eq_ignore_ascii_case("zcb") => IMA_EXT_ZCB,
        _ if ext.eq_ignore_ascii_case("zcd") => IMA_EXT_ZCD,
        _ if ext.eq_ignore_ascii_case("zcf") => IMA_EXT_ZCF,
        _ if ext.eq_ignore_ascii_case("zcmop") => IMA_EXT_ZCMOP,
        _ => 0,
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn test_parse_legacy_isa_reports_common_extensions() {
        let bits = parse_legacy_isa("rv64imafdc_zba_zbb_zicboz");

        assert_ne!(bits & IMA_EXT_FD, 0);
        assert_ne!(bits & IMA_EXT_C, 0);
        assert_ne!(bits & IMA_EXT_ZBA, 0);
        assert_ne!(bits & IMA_EXT_ZBB, 0);
        assert_ne!(bits & IMA_EXT_ZICBOZ, 0);
    }

    #[def_test]
    fn test_parse_extension_list_maps_known_extensions() {
        // NUL-separated extension names, as found in `riscv,isa-extensions`.
        let bytes = b"zba\0zbb\0zicboz\0zba\0";
        let bits = parse_extension_list(bytes);

        assert_ne!(bits & IMA_EXT_ZBA, 0);
        assert_ne!(bits & IMA_EXT_ZBB, 0);
        assert_ne!(bits & IMA_EXT_ZICBOZ, 0);
        // Unknown extensions and duplicates must not corrupt the result.
        let unknown = parse_extension_list(b"zzz_unknown\0");
        assert_eq!(unknown, 0);
    }

    #[def_test]
    fn test_hwprobe_key_classification_separates_bitmask_and_scalar() {
        // Bitmask keys, per Linux `hwprobe_key_is_bitmask`.
        for bitmask in [
            HwprobeKey::BaseBehavior,
            HwprobeKey::ImaExt0,
            HwprobeKey::ImaExt1,
            HwprobeKey::Cpuperf0,
            HwprobeKey::VendorExtThead0,
            HwprobeKey::VendorExtSifive0,
            HwprobeKey::VendorExtMips0,
        ] {
            assert!(bitmask.is_bitmask_aggregate());
        }

        for scalar in [
            HwprobeKey::Mvendorid,
            HwprobeKey::Marchid,
            HwprobeKey::Mimpid,
            HwprobeKey::ZicbozBlockSize,
            HwprobeKey::HighestVirtualAddress,
            HwprobeKey::TimeCsrFreq,
            HwprobeKey::MisalignedScalarPerf,
        ] {
            assert!(!scalar.is_bitmask_aggregate());
        }
    }

    #[def_test]
    fn test_hwprobe_key_is_known_rejects_unknown_keys() {
        for key in 0..=RISCV_HWPROBE_KEY_IMA_EXT_1 {
            assert!(hwprobe_key_is_known(key));
        }
        assert!(!hwprobe_key_is_known(-1));
        assert!(!hwprobe_key_is_known(RISCV_HWPROBE_KEY_IMA_EXT_1 + 1));
    }

    #[def_test]
    fn test_aggregate_pair_ands_bitmasks_and_detects_scalar_mismatch() {
        // Bitmask keys intersect across CPUs.
        assert_eq!(HwprobeKey::ImaExt0.aggregate_pair(0b1010, 0b0110), 0b0010);
        // Scalar keys keep the common value, else u64::MAX on mismatch.
        assert_eq!(HwprobeKey::Mvendorid.aggregate_pair(42, 42), 42);
        assert_eq!(HwprobeKey::Mvendorid.aggregate_pair(42, 7), u64::MAX);
    }

    #[def_test(serial)]
    fn test_aggregate_and_match_against_snapshot() {
        // A single-CPU mask selects the conservative default snapshot values
        // (mvendorid = 0, base_behavior = IMA bit 0).
        let mut mask = KCpuMask::new();
        mask.set(0, true);
        let cpu0 = LogicalCpuId::new(0);

        // Scalar aggregation: MVENDORID is 0 on the default snapshot.
        assert_eq!(
            hwprobe_aggregate_value(RISCV_HWPROBE_KEY_MVENDORID, &mask),
            Some(0)
        );
        // Scalar matching: requested 0 matches, requested 1 does not.
        assert_eq!(
            hwprobe_cpu_matches(RISCV_HWPROBE_KEY_MVENDORID, cpu0, 0),
            Some(true)
        );
        assert_eq!(
            hwprobe_cpu_matches(RISCV_HWPROBE_KEY_MVENDORID, cpu0, 1),
            Some(false)
        );

        // Bitmask aggregation: BASE_BEHAVIOR is the IMA bit.
        assert_eq!(
            hwprobe_aggregate_value(RISCV_HWPROBE_KEY_BASE_BEHAVIOR, &mask),
            Some(BASE_BEHAVIOR_IMA)
        );
        // Bitmask matching: requesting the present bit matches; an absent bit
        // does not.
        assert_eq!(
            hwprobe_cpu_matches(RISCV_HWPROBE_KEY_BASE_BEHAVIOR, cpu0, BASE_BEHAVIOR_IMA),
            Some(true)
        );
        assert_eq!(
            hwprobe_cpu_matches(
                RISCV_HWPROBE_KEY_BASE_BEHAVIOR,
                cpu0,
                BASE_BEHAVIOR_IMA << 1
            ),
            Some(false)
        );
    }

    #[def_test(serial)]
    fn test_aggregate_value_reports_unknown_keys_as_none() {
        let mask = KCpuMask::new();
        assert_eq!(hwprobe_aggregate_value(-1, &mask), None);
        assert_eq!(hwprobe_cpu_matches(999, LogicalCpuId::new(0), 0), None);
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-CPU identity and usable instruction capabilities for x86_64.
//!
//! Follows Linux v6.12 `arch/x86/kernel/cpu/common.c`: identify each CPU at
//! bring-up, then apply OS support constraints before exposing capabilities.
//! Formatting and Linux flag names belong to the procfs adapter.

use raw_cpuid::{CpuId, CpuIdReader, CpuIdReaderNative};
use x86_64::registers::control::{Cr0, Cr0Flags, Cr4, Cr4Flags};

/// Selected CPUID capabilities; presence means supported by the CPU and the
/// kernel's current state-management policy. Absence does not prove that the
/// hardware lacks the instruction. This is not a complete CPUID interface.
#[repr(usize)]
#[derive(Clone, Copy)]
pub enum CpuFeature {
    /// The `fpu` capability (Linux cpuinfo spelling).
    Fpu        = 0,
    /// The `tsc` capability (Linux cpuinfo spelling).
    Tsc        = 4,
    /// The `msr` capability (Linux cpuinfo spelling).
    Msr        = 5,
    /// The `cx8` capability (Linux cpuinfo spelling).
    Cx8        = 8,
    /// The `cmov` capability (Linux cpuinfo spelling).
    Cmov       = 15,
    /// The `clflush` capability (Linux cpuinfo spelling).
    Clflush    = 19,
    /// The `mmx` capability (Linux cpuinfo spelling).
    Mmx        = 23,
    /// The `fxsr` capability (Linux cpuinfo spelling).
    Fxsr       = 24,
    /// The `sse` capability (Linux cpuinfo spelling).
    Sse        = 25,
    /// The `sse2` capability (Linux cpuinfo spelling).
    Sse2       = 26,
    /// The `pni` capability (Linux cpuinfo spelling).
    Sse3       = 32,
    /// The `pclmulqdq` capability (Linux cpuinfo spelling).
    Pclmulqdq  = 33,
    /// The `ssse3` capability (Linux cpuinfo spelling).
    Ssse3      = 41,
    /// The `fma` capability (Linux cpuinfo spelling).
    Fma        = 44,
    /// The `cx16` capability (Linux cpuinfo spelling).
    Cx16       = 45,
    /// The `sse4_1` capability (Linux cpuinfo spelling).
    Sse4_1     = 51,
    /// The `sse4_2` capability (Linux cpuinfo spelling).
    Sse4_2     = 52,
    /// The `movbe` capability (Linux cpuinfo spelling).
    Movbe      = 54,
    /// The `popcnt` capability (Linux cpuinfo spelling).
    Popcnt     = 55,
    /// The `aes` capability (Linux cpuinfo spelling).
    Aes        = 57,
    /// The `xsave` capability (Linux cpuinfo spelling).
    Xsave      = 58,
    /// The `avx` capability (Linux cpuinfo spelling).
    Avx        = 60,
    /// The `f16c` capability (Linux cpuinfo spelling).
    F16c       = 61,
    /// The `rdrand` capability (Linux cpuinfo spelling).
    Rdrand     = 62,
    /// The `hypervisor` capability (Linux cpuinfo spelling).
    Hypervisor = 63,
    /// The `bmi1` capability (Linux cpuinfo spelling).
    Bmi1       = 67,
    /// The `avx2` capability (Linux cpuinfo spelling).
    Avx2       = 69,
    /// The `bmi2` capability (Linux cpuinfo spelling).
    Bmi2       = 72,
    /// The `erms` capability (Linux cpuinfo spelling).
    Erms       = 73,
    /// The `rdseed` capability (Linux cpuinfo spelling).
    Rdseed     = 82,
    /// The `adx` capability (Linux cpuinfo spelling).
    Adx        = 83,
    /// The `sha_ni` capability (Linux cpuinfo spelling).
    Sha        = 93,
    /// The `rdpid` capability (Linux cpuinfo spelling).
    Rdpid      = 118,
}

/// Immutable identity and filtered capability snapshot of one logical CPU.
///
/// CPUID strings are restricted to printable ASCII so firmware/hypervisors
/// cannot inject additional procfs lines. Unknown strings are reported as
/// `unknown`. No live CPUID query is performed by the accessors.
#[derive(Clone, Copy)]
pub struct CpuInfo {
    vendor: [u8; 12],
    brand: [u8; 48],
    family_model_stepping: Option<(u32, u32, u32)>,
    capabilities: [u32; 4],
}

#[derive(Clone, Copy)]
enum StateSupport {
    Disabled,
    Fxsave,
    Xsave,
    Avx,
}

/// Runs on each CPU during trap initialization, after per-CPU publication,
/// before scheduling is enabled. The boot caller must not migrate.
pub(super) fn init_cpu_info() {
    crate::cpuinfo::init_current_cpu_info(|| {
        let state_support = if cfg!(feature = "fp-simd")
            && Cr4::read().contains(Cr4Flags::OSFXSR)
            && !Cr0::read().contains(Cr0Flags::EMULATE_COPROCESSOR)
        {
            match super::xstate::enabled_xstate_mask() {
                7 => StateSupport::Avx,
                3 => StateSupport::Xsave,
                _ => StateSupport::Fxsave,
            }
        } else {
            StateSupport::Disabled
        };
        CpuInfo::detect(CpuIdReaderNative, state_support)
    });
}

impl CpuInfo {
    fn detect(reader: impl CpuIdReader, state: StateSupport) -> Self {
        let cpuid = CpuId::with_cpuid_reader(reader.clone());
        let mut info = Self {
            vendor: [0; 12],
            brand: [0; 48],
            family_model_stepping: None,
            capabilities: [0; 4],
        };
        if let Some(vendor) = cpuid.get_vendor_info() {
            // Invalid UTF-8 becomes "InvalidVendorString" in raw-cpuid; its
            // length exceeds this field, so the bounded copy keeps it unknown.
            copy_cpuid_text(&mut info.vendor, vendor.as_str());
        }
        if let Some(features) = cpuid.get_feature_info() {
            // raw-cpuid returns family_id as u8 and adds these fields in u8.
            // Reject an unrepresentable signature before calling into the library.
            if features
                .base_family_id()
                .checked_add(features.extended_family_id())
                .is_some()
            {
                info.family_model_stepping = Some((
                    u32::from(features.family_id()),
                    u32::from(features.model_id()),
                    u32::from(features.stepping_id()),
                ));
            }
            // Typed feature objects do not expose their raw flags. Re-read
            // supported leaves during bring-up to retain the existing bitmap
            // and OS policy without a second per-feature mapping table.
            let leaf = reader.cpuid2(1, 0);
            info.capabilities[0] = leaf.edx;
            info.capabilities[1] = leaf.ecx;
        }
        if cpuid.get_extended_feature_info().is_some() {
            let leaf = reader.cpuid2(7, 0);
            info.capabilities[2] = leaf.ebx;
            info.capabilities[3] = leaf.ecx;
        }
        if let Some(brand) = cpuid.get_processor_brand_string()
            && brand_is_printable(&reader)
            && brand.as_str() != "Invalid Processor Brand String"
        {
            copy_cpuid_text(&mut info.brand, brand.as_str());
        }
        info.apply_state_policy(state);
        info
    }

    fn apply_state_policy(&mut self, state: StateSupport) {
        if !matches!(state, StateSupport::Xsave | StateSupport::Avx) {
            self.clear(CpuFeature::Xsave);
        }
        if !matches!(state, StateSupport::Avx)
            || !self.has_feature(CpuFeature::Avx)
            || !self.has_feature(CpuFeature::Xsave)
        {
            for feature in [
                CpuFeature::Avx,
                CpuFeature::Avx2,
                CpuFeature::Fma,
                CpuFeature::F16c,
            ] {
                self.clear(feature);
            }
        }
        if matches!(state, StateSupport::Disabled)
            || !self.has_feature(CpuFeature::Fpu)
            || !self.has_feature(CpuFeature::Fxsr)
        {
            for feature in [
                CpuFeature::Fpu,
                CpuFeature::Mmx,
                CpuFeature::Fxsr,
                CpuFeature::Sse,
                CpuFeature::Sse2,
            ] {
                self.clear(feature);
            }
        }
        if !self.has_feature(CpuFeature::Sse) {
            self.clear(CpuFeature::Sse2);
        }
        if !self.has_feature(CpuFeature::Sse2) {
            for feature in [
                CpuFeature::Sse3,
                CpuFeature::Pclmulqdq,
                CpuFeature::Ssse3,
                CpuFeature::Sse4_1,
                CpuFeature::Sse4_2,
                CpuFeature::Aes,
                CpuFeature::Sha,
                CpuFeature::Xsave,
                CpuFeature::Avx,
                CpuFeature::Avx2,
                CpuFeature::Fma,
                CpuFeature::F16c,
            ] {
                self.clear(feature);
            }
        }
    }

    fn clear(&mut self, feature: CpuFeature) {
        let bit = feature as usize;
        self.capabilities[bit / 32] &= !(1 << (bit % 32));
    }

    /// Whether this CPU advertises a capability after kernel policy filtering.
    pub fn has_feature(&self, feature: CpuFeature) -> bool {
        let bit = feature as usize;
        self.capabilities[bit / 32] & (1 << (bit % 32)) != 0
    }

    /// CPUID leaf 0 vendor string, in EBX/EDX/ECX order.
    pub fn vendor_id(&self) -> &str {
        cpuid_text(&self.vendor)
    }

    /// CPUID extended brand string, or `unknown` if unavailable or malformed.
    pub fn model_name(&self) -> &str {
        cpuid_text(&self.brand)
    }

    /// Display family, model and stepping, including extended family/model
    /// fields per raw-cpuid's vendor rules. `None` means leaf 1 is unavailable
    /// or the base/extended-family sum exceeds the parser's u8 range.
    pub fn family_model_stepping(&self) -> Option<(u32, u32, u32)> {
        self.family_model_stepping
    }
}

fn copy_cpuid_text(output: &mut [u8], text: &str) {
    let text = cpuid_text(text.as_bytes()).as_bytes();
    if let Some(destination) = output.get_mut(..text.len()) {
        destination.copy_from_slice(text);
    }
}

fn brand_is_printable(reader: &impl CpuIdReader) -> bool {
    // as_str() trims before we can validate, hiding leading/trailing control
    // bytes. Only scan the original bytes here; raw-cpuid owns string assembly
    // and has already checked that all three brand leaves exist.
    (0x8000_0002..=0x8000_0004)
        .flat_map(|leaf| {
            let result = reader.cpuid2(leaf, 0);
            [result.eax, result.ebx, result.ecx, result.edx]
        })
        .flat_map(u32::to_le_bytes)
        .take_while(|byte| *byte != 0)
        .all(|byte| matches!(byte, b' '..=b'~'))
}

fn cpuid_text(bytes: &[u8]) -> &str {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    let bytes = &bytes[..end];
    if !bytes.iter().all(|byte| matches!(byte, b' '..=b'~')) {
        return "unknown";
    }
    let text = core::str::from_utf8(bytes).unwrap_or("").trim();
    if text.is_empty() { "unknown" } else { text }
}

unittest::path_mod_test!(tests, "../../ut-tests/x86_cpuinfo.rs");

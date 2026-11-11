// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux cpuinfo presentation over architecture-owned CPU snapshots.

use alloc::string::String;
use core::fmt::Write;

use kcpu_id_map::for_each_present_logical_cpu;
use kvfs::{VfsError, VfsResult};

pub(super) fn read() -> VfsResult<String> {
    let mut output = String::new();
    let mut result = Ok(());
    for_each_present_logical_cpu(|index, cpu_id, count| {
        result = result.and_then(|()| {
            if index != 0 {
                output.push('\n');
            }
            writeln!(output, "processor\t: {}", cpu_id.as_usize())?;
            write_arch(&mut output, kcpu::cpu_info(cpu_id))?;
            // Preserve the historical topology fields; cpu cores currently
            // means total present logical CPUs, not cores in a package.
            writeln!(
                output,
                "cpu cores\t: {count}\narchitecture\t: {}",
                kbuild_config::ARCH
            )
        });
    });
    // String formatting is infallible today; retain propagation if the output
    // sink or the formatted types ever acquire a fallible implementation.
    result.map_err(|_| VfsError::InvalidInput)?;
    Ok(output)
}

#[cfg(target_arch = "x86_64")]
fn write_arch(output: &mut impl Write, info: Option<&kcpu::CpuInfo>) -> core::fmt::Result {
    let Some(info) = info else {
        // A present CPU may not have started. Do not borrow another CPU's
        // identity or capabilities while its own snapshot is unavailable.
        return writeln!(
            output,
            "vendor_id\t: unknown\nmodel name\t: unknown\nflags\t\t:"
        );
    };
    writeln!(output, "vendor_id\t: {}", info.vendor_id())?;
    if let Some((family, model, stepping)) = info.family_model_stepping() {
        writeln!(
            output,
            "cpu family\t: {family}\nmodel\t\t: {model}\nstepping\t: {stepping}"
        )?;
    }
    writeln!(output, "model name\t: {}", info.model_name())?;
    write_capabilities(output, "flags\t\t:", Some(info))
}

#[cfg(target_arch = "aarch64")]
fn write_arch(output: &mut impl Write, info: Option<&kcpu::CpuInfo>) -> core::fmt::Result {
    write_capabilities(output, "Features\t:", info)?;
    if let Some(info) = info {
        let (implementer, variant, part, revision) = info.identity();
        writeln!(
            output,
            "CPU implementer\t: 0x{implementer:02x}\nCPU architecture: 8\nCPU variant\t: \
             0x{variant:x}\nCPU part\t: 0x{part:03x}\nCPU revision\t: {revision}"
        )
    } else {
        writeln!(
            output,
            "CPU implementer\t: unknown\nCPU architecture: 8\nCPU variant\t: unknown\nCPU part\t: \
             unknown\nCPU revision\t: unknown"
        )
    }
}

#[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))]
fn write_arch(output: &mut impl Write, info: Option<&kcpu::CpuInfo>) -> core::fmt::Result {
    write!(output, "isa\t\t:")?;
    if let Some(info) = info.filter(|info| info.has_feature(kcpu::CpuFeature::I)) {
        write!(output, " rv{}", usize::BITS)?;
        for &(feature, name) in CPU_FLAGS {
            if info.has_feature(feature) {
                write!(output, "{name}")?;
            }
        }
    }
    writeln!(output)?;
    let ids = info.map_or([None; 3], kcpu::CpuInfo::architecture_ids);
    for (name, value) in ["mvendorid", "marchid\t", "mimpid\t"].into_iter().zip(ids) {
        if let Some(value) = value {
            writeln!(output, "{name}\t: 0x{value:x}")?;
        } else {
            writeln!(output, "{name}\t: unknown")?;
        }
    }
    Ok(())
}

#[cfg(target_arch = "loongarch64")]
fn write_arch(output: &mut impl Write, info: Option<&kcpu::CpuInfo>) -> core::fmt::Result {
    if let Some(info) = info {
        let processor_id = info.processor_id();
        writeln!(
            output,
            "CPU Family\t: {}\nModel Name\t: LoongArch (PRID 0x{processor_id:08x})\nCPU \
             Revision\t: 0x{:02x}\nISA\t\t: loongarch64",
            info.family(),
            processor_id & 0xff
        )?;
    } else {
        writeln!(
            output,
            "CPU Family\t: unknown\nModel Name\t: unknown\nCPU Revision\t: unknown\nISA\t\t:"
        )?;
    }
    write_capabilities(output, "Features\t:", info)
}

#[cfg(not(any(target_arch = "riscv32", target_arch = "riscv64")))]
fn write_capabilities(
    output: &mut impl Write,
    label: &str,
    info: Option<&kcpu::CpuInfo>,
) -> core::fmt::Result {
    write!(output, "{label}")?;
    for &(feature, name) in CPU_FLAGS {
        if info.is_some_and(|info| info.has_feature(feature)) {
            write!(output, " {name}")?;
        }
    }
    writeln!(output)
}

// Linux v6.12 arch/x86/include/asm/cpufeatures.h supplies the ABI spellings.
// This is an intentional subset; an absent name makes no hardware claim.
// kcpu filters XSAVE-dependent features against the enabled state contract.
#[cfg(target_arch = "x86_64")]
const CPU_FLAGS: &[(kcpu::CpuFeature, &str)] = &[
    (kcpu::CpuFeature::Fpu, "fpu"),
    (kcpu::CpuFeature::Tsc, "tsc"),
    (kcpu::CpuFeature::Msr, "msr"),
    (kcpu::CpuFeature::Cx8, "cx8"),
    (kcpu::CpuFeature::Cmov, "cmov"),
    (kcpu::CpuFeature::Clflush, "clflush"),
    (kcpu::CpuFeature::Mmx, "mmx"),
    (kcpu::CpuFeature::Fxsr, "fxsr"),
    (kcpu::CpuFeature::Sse, "sse"),
    (kcpu::CpuFeature::Sse2, "sse2"),
    (kcpu::CpuFeature::Sse3, "pni"),
    (kcpu::CpuFeature::Pclmulqdq, "pclmulqdq"),
    (kcpu::CpuFeature::Ssse3, "ssse3"),
    (kcpu::CpuFeature::Fma, "fma"),
    (kcpu::CpuFeature::Cx16, "cx16"),
    (kcpu::CpuFeature::Sse4_1, "sse4_1"),
    (kcpu::CpuFeature::Sse4_2, "sse4_2"),
    (kcpu::CpuFeature::Movbe, "movbe"),
    (kcpu::CpuFeature::Popcnt, "popcnt"),
    (kcpu::CpuFeature::Aes, "aes"),
    (kcpu::CpuFeature::Xsave, "xsave"),
    (kcpu::CpuFeature::Avx, "avx"),
    (kcpu::CpuFeature::F16c, "f16c"),
    (kcpu::CpuFeature::Rdrand, "rdrand"),
    (kcpu::CpuFeature::Hypervisor, "hypervisor"),
    (kcpu::CpuFeature::Bmi1, "bmi1"),
    (kcpu::CpuFeature::Avx2, "avx2"),
    (kcpu::CpuFeature::Bmi2, "bmi2"),
    (kcpu::CpuFeature::Erms, "erms"),
    (kcpu::CpuFeature::Rdseed, "rdseed"),
    (kcpu::CpuFeature::Adx, "adx"),
    (kcpu::CpuFeature::Sha, "sha_ni"),
    (kcpu::CpuFeature::Rdpid, "rdpid"),
];

// Linux v6.12 arch/arm64/kernel/cpuinfo.c supplies the ABI spellings.
#[cfg(target_arch = "aarch64")]
const CPU_FLAGS: &[(kcpu::CpuFeature, &str)] = &[
    (kcpu::CpuFeature::Fp, "fp"),
    (kcpu::CpuFeature::Asimd, "asimd"),
    (kcpu::CpuFeature::Aes, "aes"),
    (kcpu::CpuFeature::Pmull, "pmull"),
    (kcpu::CpuFeature::Sha1, "sha1"),
    (kcpu::CpuFeature::Sha2, "sha2"),
    (kcpu::CpuFeature::Crc32, "crc32"),
    (kcpu::CpuFeature::Atomics, "atomics"),
];

// Linux ISA strings keep base letters contiguous and separate long extensions.
#[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))]
const CPU_FLAGS: &[(kcpu::CpuFeature, &str)] = &[
    (kcpu::CpuFeature::I, "i"),
    (kcpu::CpuFeature::M, "m"),
    (kcpu::CpuFeature::A, "a"),
    (kcpu::CpuFeature::F, "f"),
    (kcpu::CpuFeature::D, "d"),
    (kcpu::CpuFeature::C, "c"),
    (kcpu::CpuFeature::Zicsr, "_zicsr"),
    (kcpu::CpuFeature::Zifencei, "_zifencei"),
    (kcpu::CpuFeature::Zba, "_zba"),
    (kcpu::CpuFeature::Zbb, "_zbb"),
    (kcpu::CpuFeature::Zbs, "_zbs"),
];

// Linux v6.12 arch/loongarch/kernel/proc.c supplies the ABI spellings.
#[cfg(target_arch = "loongarch64")]
const CPU_FLAGS: &[(kcpu::CpuFeature, &str)] = &[
    (kcpu::CpuFeature::Cpucfg, "cpucfg"),
    (kcpu::CpuFeature::Lam, "lam"),
    (kcpu::CpuFeature::Ual, "ual"),
    (kcpu::CpuFeature::Fpu, "fpu"),
    (kcpu::CpuFeature::Crc32, "crc32"),
];

unittest::path_mod_test!(tests, "../../ut-tests/cpuinfo.rs");

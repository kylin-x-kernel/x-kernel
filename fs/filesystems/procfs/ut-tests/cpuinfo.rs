// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

#[def_test]
fn cpuinfo_has_one_section_per_present_logical_cpu() {
    let text = super::read().unwrap();
    let mut sections = text.trim_end().split("\n\n");
    let mut cpus = alloc::vec::Vec::new();
    kcpu_id_map::for_each_present_logical_cpu(|_, cpu_id, count| cpus.push((cpu_id, count)));
    for (cpu_id, count) in cpus {
        let section = sections.next().unwrap();
        assert!(section.starts_with(&alloc::format!("processor\t: {}\n", cpu_id.as_usize())));
        assert!(section.contains(&alloc::format!("cpu cores\t: {count}\n")));
        assert!(section.ends_with(kbuild_config::ARCH));
        #[cfg(target_arch = "aarch64")]
        for field in [
            "Features\t:",
            "CPU implementer\t:",
            "CPU architecture: 8",
            "CPU variant\t:",
            "CPU part\t:",
            "CPU revision\t:",
        ] {
            assert!(section.lines().any(|line| line.starts_with(field)));
        }
        #[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))]
        for field in ["isa\t\t:", "mvendorid\t:", "marchid\t\t:", "mimpid\t\t:"] {
            assert!(section.lines().any(|line| line.starts_with(field)));
        }
        #[cfg(target_arch = "loongarch64")]
        for field in [
            "CPU Family\t:",
            "Model Name\t:",
            "CPU Revision\t:",
            "ISA\t\t:",
            "Features\t:",
        ] {
            assert!(section.lines().any(|line| line.starts_with(field)));
        }
        #[cfg(target_arch = "x86_64")]
        {
            let flags = section
                .lines()
                .find(|line| line.starts_with("flags\t"))
                .unwrap();
            let flags: alloc::vec::Vec<_> = flags
                .split_once(':')
                .unwrap()
                .1
                .split_whitespace()
                .collect();
            assert_eq!(
                flags.contains(&"pni"),
                kcpu::cpu_info(cpu_id).is_some_and(|info| info.has_feature(kcpu::CpuFeature::Sse3))
            );
            for unsupported in ["sse3", "osxsave"] {
                assert!(!flags.contains(&unsupported));
            }
            for (name, feature) in [
                ("avx", kcpu::CpuFeature::Avx),
                ("avx2", kcpu::CpuFeature::Avx2),
                ("xsave", kcpu::CpuFeature::Xsave),
            ] {
                assert_eq!(
                    flags.contains(&name),
                    kcpu::cpu_info(cpu_id).is_some_and(|info| info.has_feature(feature))
                );
            }
            let vendor = kcpu::cpu_info(cpu_id).map_or("unknown", kcpu::CpuInfo::vendor_id);
            assert!(section.contains(&alloc::format!("vendor_id\t: {vendor}\n")));
        }
    }
    assert_eq!(sections.next(), None);
}

#[def_test]
fn unidentified_cpu_has_unknown_identity_and_no_capabilities() {
    let mut text = alloc::string::String::new();
    super::write_arch(&mut text, None).unwrap();
    #[cfg(target_arch = "x86_64")]
    assert_eq!(
        text,
        "vendor_id\t: unknown\nmodel name\t: unknown\nflags\t\t:\n"
    );
    #[cfg(target_arch = "aarch64")]
    {
        assert!(text.starts_with("Features\t:\n"));
        assert!(text.contains("CPU implementer\t: unknown\n"));
    }
    #[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))]
    {
        assert!(text.starts_with("isa\t\t:\n"));
        assert!(text.contains("mvendorid\t: unknown\n"));
    }
    #[cfg(target_arch = "loongarch64")]
    {
        assert!(text.starts_with("CPU Family\t: unknown\nModel Name\t: unknown\n"));
        assert!(text.ends_with("Features\t:\n"));
    }
}

#[def_test]
fn linux_names_are_unique_and_use_the_architecture_abi() {
    for (index, &(feature, name)) in super::CPU_FLAGS.iter().enumerate() {
        assert!(
            !super::CPU_FLAGS[..index]
                .iter()
                .any(|&(earlier_feature, earlier_name)| earlier_name == name
                    || earlier_feature as usize == feature as usize)
        );
        #[cfg(target_arch = "x86_64")]
        if matches!(feature, kcpu::CpuFeature::Sse3) {
            assert_eq!(name, "pni");
        }
    }
}

#[cfg(not(any(target_arch = "riscv32", target_arch = "riscv64")))]
#[def_test]
fn rendered_capabilities_match_each_published_cpu_policy() {
    let mut cpus = alloc::vec::Vec::new();
    kcpu_id_map::for_each_present_logical_cpu(|_, cpu_id, _| cpus.push(cpu_id));
    for cpu_id in cpus {
        let info = kcpu::cpu_info(cpu_id);
        let mut text = alloc::string::String::new();
        super::write_arch(&mut text, info).unwrap();
        let flags = text
            .lines()
            .find(|line| line.starts_with("flags\t") || line.starts_with("Features\t"))
            .unwrap();
        let names: alloc::vec::Vec<_> = flags
            .split_once(':')
            .unwrap()
            .1
            .split_whitespace()
            .collect();
        for &(feature, name) in super::CPU_FLAGS {
            assert_eq!(
                names.contains(&name),
                info.is_some_and(|info| info.has_feature(feature))
            );
        }
        for unsupported in ["sve", "sve2", "lsx", "lasx"] {
            assert!(!names.contains(&unsupported));
        }
    }
}

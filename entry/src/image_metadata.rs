// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Early-boot display of metadata embedded in the kernel image.

/// Prints the embedded build info, substituting the log level that is
/// actually active at boot (the embedded `log_level` line only records the
/// Kconfig default; a `loglevel=` command-line override may differ).
pub(crate) fn print() {
    match kernel_image_metadata::embedded_build_info() {
        Ok(build_info) => {
            for line in build_info.trim_end_matches('\n').lines() {
                if line.starts_with("log_level = ") {
                    kprintln!(
                        "log_level = {}",
                        klogger::level_name(klogger::current_log_level())
                    );
                } else {
                    kprintln!("{line}");
                }
            }
        }
        Err(error) => kprintln!("build_info = unavailable ({error})"),
    }

    match kernel_image_metadata::embedded_build_id() {
        Ok(build_id) => {
            kprint!("build_id = ");
            for byte in build_id {
                kprint!("{byte:02x}");
            }
            kprintln!();
        }
        Err(error) => kprintln!("build_id = unavailable ({error})"),
    }

    kprintln!("smp = {}", kcpu_id_map::nr_cpus());
}

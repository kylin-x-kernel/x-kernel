// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Workspace rustdoc orchestration.

use xconfig::build_config::{
    KernelBuildFiles, generate_kernel_build_files, prepare_kernel_config, resolve_kernel_config,
};

use crate::{cli::DocArgs, context, error::Result, process::Process};

const PLATFORM_CHECK_CFG: &str = "cfg(k_plat_name, values(\"kplat-aarch64\", \
                                  \"kplat-loongarch64\", \"kplat-riscv64\", \
                                  \"kplat-x86_64\"))";

pub(crate) fn generate(args: &DocArgs) -> Result<()> {
    let workspace_root = context::workspace_root()?;
    let config_path = workspace_root.join(&args.workspace.config);
    context::ensure_config_exists(&config_path)?;

    let kconfig_path = workspace_root.join("Kconfig");
    let config = if args.workspace.dry_run {
        resolve_kernel_config(&config_path, &kconfig_path, &workspace_root)?
    } else {
        prepare_kernel_config(&config_path, &kconfig_path, &workspace_root)?
    };
    let target_dir = workspace_root.join(&args.workspace.target_dir);
    let platform = config.platform();
    let profile = config.profile().as_str();
    let build_files = KernelBuildFiles {
        rust_const_dir: target_dir.join("kbuild").join(platform),
        linker_script: target_dir
            .join(config.target())
            .join(profile)
            .join(format!("linker_{platform}.lds")),
        unittest: false,
        dwarf: config.is_enabled("KFEAT_DWARF"),
    };
    if !args.workspace.dry_run {
        generate_kernel_build_files(&config, &build_files)?;
        crate::linker::generate(
            &config,
            &build_files.linker_script,
            args.workspace.verbosity,
        )?;
    }

    let features = config
        .cargo_features()
        .iter()
        .map(|feature| format!("kfeat/{feature}"))
        .collect::<Vec<_>>();
    let mut command = Process::new("cargo", args.workspace.dry_run, args.workspace.verbosity);
    command
        .current_dir(&workspace_root)
        .args(["doc", "--no-deps", "--workspace"])
        .arg("--target")
        .arg(config.target())
        .arg("--target-dir")
        .arg(&target_dir)
        .arg("--config")
        .arg(rustdoc_flags_config(args.check_missing))
        .env("RUSTC_BOOTSTRAP", "1")
        .env("TARGET_DIR", &target_dir)
        .env("K_ARCH", config.arch().as_str())
        .env("K_TARGET", config.target())
        .env("K_PLAT_NAME", platform)
        .env("K_MODE", profile)
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS");
    if !features.is_empty() {
        command.arg("--features").arg(features.join(","));
    }
    if args.open {
        command.arg("--open");
    }
    match args.workspace.verbosity {
        0 => {}
        1 => {
            command.arg("-v");
        }
        _ => {
            command.arg("-vv");
        }
    }
    command.args(args.cargo_args.iter()).run()?;

    if !args.workspace.dry_run {
        println!(
            "Generated documentation at {}",
            target_dir
                .join(config.target())
                .join("doc/index.html")
                .display()
        );
    }
    Ok(())
}

fn rustdoc_flags_config(check_missing: bool) -> String {
    let mut flags = vec![
        "--cfg",
        "doc",
        "-Z",
        "unstable-options",
        "--enable-index-page",
        "-D",
        "rustdoc::broken_intra_doc_links",
        "--check-cfg",
        "cfg(unittest)",
        "--check-cfg",
        PLATFORM_CHECK_CFG,
    ];
    if check_missing {
        flags.extend(["-D", "missing-docs"]);
    }
    let values = flags
        .into_iter()
        .map(|flag| toml::Value::String(flag.to_string()))
        .collect();
    format!("build.rustdocflags={}", toml::Value::Array(values))
}

#[cfg(test)]
mod tests {
    use super::rustdoc_flags_config;

    #[test]
    fn missing_docs_mode_preserves_baseline_rustdoc_checks() {
        let config = rustdoc_flags_config(true);

        assert!(config.contains("rustdoc::broken_intra_doc_links"));
        assert!(config.contains("cfg(unittest)"));
        assert!(config.contains("missing-docs"));
    }

    #[test]
    fn normal_docs_do_not_deny_missing_docs() {
        assert!(!rustdoc_flags_config(false).contains("missing-docs"));
    }
}

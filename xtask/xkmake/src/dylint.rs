// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Build and run the repository's pinned Dylint lint library.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use serde::Deserialize;
use xconfig::build_config::BuildProfile;

use crate::{
    build::{apply_kernel_env, generate_build_configuration, kernel_feature_flags},
    cli::DylintArgs,
    context::BuildContext,
    error::{Error, IoResultExt, Result},
    hygiene::{DYLINT_VERSION, ensure_dylint_tools},
    process::Process,
};

const LINT_DIR: &str = "tools/lints";
const LINT_LIB: &str = "xkernel_lints";
/// Placeholder printed by `--dry-run`, which must not spawn `rustc` to
/// detect the host triple.
const DRY_RUN_HOST: &str = "<host>";
const SANITIZED_ENV: &[&str] = &[
    "CARGO_BUILD_TARGET",
    "CARGO_TARGET_DIR",
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "RUSTUP_TOOLCHAIN",
];

#[derive(Deserialize)]
struct ToolchainFile {
    toolchain: Toolchain,
}

#[derive(Deserialize)]
struct Toolchain {
    channel: String,
}

pub(crate) fn run(args: &DylintArgs) -> Result<()> {
    let context = BuildContext::create(&args.build)?;
    let lint_dir = context.workspace_root.join(LINT_DIR);
    let toolchain_path = lint_dir.join("rust-toolchain.toml");
    let toolchain = read_toolchain(&toolchain_path)?;
    let host = probe_host(context.dry_run, || {
        let host = rustc_host(&toolchain)?;
        ensure_dylint_tools()?;
        ensure_rustc_dev(&toolchain)?;
        if !args.self_test {
            ensure_target(&toolchain, context.config.target())?;
        }
        Ok(host)
    })?;

    let driver_path = prepare_driver_path(&context)?;
    let lint_manifest = lint_dir.join("Cargo.toml");
    let lint_target_dir = context.target_dir.join("tools/lints");
    let mut lint = Process::new("cargo", context.dry_run, context.verbosity);
    lint.current_dir(&lint_dir)
        .arg(format!("+{toolchain}"))
        .arg(if args.self_test { "test" } else { "build" })
        .arg("--locked");
    if !args.self_test {
        lint.arg("--release");
    }
    lint.arg("--manifest-path")
        .arg(&lint_manifest)
        .arg("--target-dir")
        .arg(&lint_target_dir);
    sanitize_env(&mut lint);
    set_host_tools(&mut lint, &host);
    if let Some(path) = &driver_path {
        lint.env("DYLINT_DRIVER_PATH", path);
    }
    lint.run()?;

    if args.self_test {
        return Ok(());
    }

    let library = lint_library_path(&lint_target_dir, &toolchain, &host);
    if !context.dry_run && !library.is_file() {
        return Err(Error::Message(format!(
            "Dylint library not found at {}; ensure dylint-link {DYLINT_VERSION} is installed \
             (run make install-tools)",
            library.display()
        )));
    }

    generate_build_configuration(&context)?;

    let mut scan = Process::new("cargo", context.dry_run, context.verbosity);
    scan.current_dir(&context.workspace_root)
        .arg(format!("+{toolchain}"))
        .args(["dylint", "--no-metadata", "--lib-path"])
        .arg(&library)
        .arg("--manifest-path")
        .arg(&context.app_manifest)
        .arg("--")
        .arg("--target")
        .arg(context.config.target());
    if context.config.profile() == BuildProfile::Release {
        scan.arg("--release");
    }
    let features = kernel_feature_flags(&context);
    if !features.is_empty() {
        scan.arg("--features").arg(features.join(","));
    }
    match context.verbosity {
        0 => {}
        1 => {
            scan.arg("-v");
        }
        _ => {
            scan.arg("-vv");
        }
    }
    scan.args(args.cargo_args.iter());
    apply_kernel_env(&mut scan, &context);
    sanitize_env(&mut scan);
    set_host_tools(&mut scan, &host);
    scan.env("CARGO_TARGET_DIR", context.target_dir.join("dylint"))
        .env("DYLINT_RUSTFLAGS", "--cfg unittest");
    if let Some(path) = &driver_path {
        scan.env("DYLINT_DRIVER_PATH", path);
    }
    scan.run()
}

/// Runs the host-triple and tool probes, which spawn `rustc`, `rustup` and
/// `cargo-dylint`, only for a real run: `--dry-run` prints commands without
/// executing anything, so it uses a placeholder host instead.
fn probe_host(is_dry_run: bool, probe: impl FnOnce() -> Result<String>) -> Result<String> {
    if is_dry_run {
        Ok(DRY_RUN_HOST.to_owned())
    } else {
        probe()
    }
}

/// Resolves `DYLINT_DRIVER_PATH` against the workspace root and creates it.
///
/// Dylint requires an overridden driver directory to exist before it runs,
/// for both the kernel scan and the lint self-test.
fn prepare_driver_path(context: &BuildContext) -> Result<Option<PathBuf>> {
    let Some(raw) = std::env::var_os("DYLINT_DRIVER_PATH") else {
        return Ok(None);
    };
    let path = context.workspace_root.join(raw);
    if !context.dry_run {
        fs::create_dir_all(&path).with_path(&path)?;
    }
    Ok(Some(path))
}

fn read_toolchain(path: &Path) -> Result<String> {
    let content = fs::read_to_string(path).with_path(path)?;
    parse_toolchain(&content)
        .map_err(|error| Error::Message(format!("invalid {}: {error}", path.display())))
}

fn parse_toolchain(content: &str) -> std::result::Result<String, String> {
    let parsed: ToolchainFile = toml::from_str(content).map_err(|error| error.to_string())?;
    let channel = parsed.toolchain.channel.trim();
    if channel.is_empty() {
        return Err("toolchain channel is empty".into());
    }
    Ok(channel.to_owned())
}

fn rustc_host(toolchain: &str) -> Result<String> {
    let output = Command::new("rustc")
        .arg(format!("+{toolchain}"))
        .arg("-vV")
        .env_remove("RUSTUP_TOOLCHAIN")
        .output()
        .map_err(|source| Error::Spawn {
            program: "rustc".into(),
            source,
        })?;
    if !output.status.success() {
        return Err(Error::Message(format!(
            "rustc +{toolchain} -vV failed; install the lint toolchain with rustup"
        )));
    }
    parse_host(&String::from_utf8_lossy(&output.stdout)).map_err(Error::Message)
}

fn parse_host(version_output: &str) -> std::result::Result<String, String> {
    version_output
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .filter(|host| !host.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "rustc -vV did not report a host triple".into())
}

fn ensure_rustc_dev(toolchain: &str) -> Result<()> {
    let sysroot = Command::new("rustup")
        .args(["run", toolchain, "rustc", "--print", "sysroot"])
        .env_remove("RUSTUP_TOOLCHAIN")
        .output()
        .map_err(|source| Error::Spawn {
            program: "rustup".into(),
            source,
        })?;
    let components = Command::new("rustup")
        .args(["component", "list", "--toolchain", toolchain, "--installed"])
        .env_remove("RUSTUP_TOOLCHAIN")
        .output()
        .map_err(|source| Error::Spawn {
            program: "rustup".into(),
            source,
        })?;
    let has_rustc_dev = String::from_utf8_lossy(&components.stdout)
        .lines()
        .any(|line| line.starts_with("rustc-dev-") || line == "rustc-dev");
    if !sysroot.status.success() || !components.status.success() || !has_rustc_dev {
        return Err(Error::Message(format!(
            "the {toolchain} toolchain needs rustc-dev; run rustup component add rustc-dev \
             llvm-tools --toolchain {toolchain}"
        )));
    }
    Ok(())
}

/// Fails early when the lint toolchain lacks the kernel target; otherwise the
/// scan stops on a missing `core` for that target.
fn ensure_target(toolchain: &str, target: &str) -> Result<()> {
    let targets = Command::new("rustup")
        .args(["target", "list", "--toolchain", toolchain, "--installed"])
        .env_remove("RUSTUP_TOOLCHAIN")
        .output()
        .map_err(|source| Error::Spawn {
            program: "rustup".into(),
            source,
        })?;
    let installed = String::from_utf8_lossy(&targets.stdout)
        .lines()
        .any(|line| line.trim() == target);
    if !targets.status.success() || !installed {
        return Err(Error::Message(format!(
            "the {toolchain} toolchain needs the {target} target; run rustup target add {target} \
             --toolchain {toolchain}"
        )));
    }
    Ok(())
}

fn lint_library_path(target_dir: &Path, toolchain: &str, host: &str) -> PathBuf {
    let prefix = std::env::consts::DLL_PREFIX;
    let suffix = std::env::consts::DLL_SUFFIX;
    target_dir
        .join("release")
        .join(format!("{prefix}{LINT_LIB}@{toolchain}-{host}{suffix}"))
}

fn sanitize_env(process: &mut Process) {
    for name in SANITIZED_ENV {
        process.env_remove(name);
    }
}

fn set_host_tools(process: &mut Process, host: &str) {
    let host = host.replace('-', "_");
    for (tool, command) in [("CC", "cc"), ("AR", "ar"), ("RANLIB", "ranlib")] {
        process.env(format!("{tool}_{host}"), command);
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{DRY_RUN_HOST, lint_library_path, parse_host, parse_toolchain, probe_host};

    #[test]
    fn reads_pinned_toolchain_channel() {
        assert_eq!(
            parse_toolchain("[toolchain]\nchannel = \"nightly-2026-03-08\"\n").unwrap(),
            "nightly-2026-03-08"
        );
        assert!(parse_toolchain("[toolchain]\nchannel = \"\"\n").is_err());
    }

    #[test]
    fn dry_run_does_not_probe_the_host() {
        let host = probe_host(true, || panic!("--dry-run must not spawn rustc")).unwrap();
        assert_eq!(host, DRY_RUN_HOST);
        let host = probe_host(false, || Ok("x86_64-unknown-linux-gnu".into())).unwrap();
        assert_eq!(host, "x86_64-unknown-linux-gnu");
    }

    #[test]
    fn extracts_host_from_verbose_rustc_output() {
        assert_eq!(
            parse_host("rustc 1.96.0-nightly\nhost: x86_64-unknown-linux-gnu\n").unwrap(),
            "x86_64-unknown-linux-gnu"
        );
        assert!(parse_host("rustc 1.96.0-nightly\n").is_err());
    }

    #[test]
    fn uses_dylint_link_library_name() {
        let path = lint_library_path(
            Path::new("target/tools/lints"),
            "nightly-2026-03-08",
            "x86_64-unknown-linux-gnu",
        );
        let expected = format!(
            "target/tools/lints/release/{}xkernel_lints@\
             nightly-2026-03-08-x86_64-unknown-linux-gnu{}",
            std::env::consts::DLL_PREFIX,
            std::env::consts::DLL_SUFFIX
        );
        assert_eq!(path, Path::new(&expected));
    }
}

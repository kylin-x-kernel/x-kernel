// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Repository hygiene orchestration.

use std::{
    env, io,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use crate::{
    cli::HygieneFixArgs,
    context::workspace_root,
    error::{Error, Result},
    process::Process,
};

const CARGO_SHEAR_VERSION: &str = "1.13.2";
const LICENSURE_VERSION: &str = "0.8.1";
const CARGO_WORKSPACES: &[&str] = &[".", "xtask"];
const LICENSURE_FILE_BATCH_SIZE: usize = 256;

pub(crate) fn install_tools() -> Result<()> {
    install_cargo_tool("cargo-shear", CARGO_SHEAR_VERSION)?;
    install_cargo_tool("licensure", LICENSURE_VERSION)
}

pub(crate) fn check_dependencies(args: &HygieneFixArgs) -> Result<()> {
    ensure_cargo_shear_version()?;
    let root = workspace_root()?;

    for relative_path in CARGO_WORKSPACES {
        if args.fix {
            match run_cargo_shear(&root, relative_path, true) {
                Ok(()) => continue,
                // cargo-shear reports the original findings with a non-zero status
                // even after fixing them. A clean rerun is authoritative.
                Err(Error::CommandFailed { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        run_cargo_shear(&root, relative_path, false)?;
    }
    Ok(())
}

pub(crate) fn check_headers(args: &HygieneFixArgs) -> Result<()> {
    ensure_licensure_version()?;
    let root = workspace_root()?;
    let rust_files = rust_source_files(&root)?;

    if args.fix {
        run_licensure(&root, &rust_files, false)?;
    }
    run_licensure(&root, &rust_files, true)
}

fn ensure_licensure_version() -> Result<()> {
    let output = tool_version_output("licensure", LICENSURE_VERSION)?;
    if !output.status.success() {
        return Err(Error::CommandFailed {
            program: "licensure".into(),
            status: output.status,
        });
    }

    let reported = String::from_utf8_lossy(&output.stdout);
    if reported_version(&reported) != Some(LICENSURE_VERSION) {
        return Err(unexpected_tool_version(
            "licensure",
            LICENSURE_VERSION,
            reported.trim(),
        ));
    }
    Ok(())
}

fn rust_source_files(root: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .current_dir(root)
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            "*.rs",
        ])
        .output()
        .map_err(|source| Error::Spawn {
            program: "git".into(),
            source,
        })?;
    if !output.status.success() {
        return Err(Error::CommandFailed {
            program: "git".into(),
            status: output.status,
        });
    }

    let stdout = String::from_utf8(output.stdout).map_err(|error| {
        Error::Message(format!(
            "git returned a non-UTF-8 Rust source path: {error}"
        ))
    })?;
    Ok(stdout
        .lines()
        .filter(|path| !path.is_empty() && root.join(path).is_file())
        .map(str::to_owned)
        .collect())
}

fn run_licensure(root: &Path, rust_files: &[String], check: bool) -> Result<()> {
    let mut first_check_failure = None;
    for files in rust_files.chunks(LICENSURE_FILE_BATCH_SIZE) {
        let mut process = Process::new("licensure", false, 0);
        process.current_dir(root);
        if check {
            process.arg("--check");
        } else {
            process.arg("--in-place");
        }
        match process.args(files).run() {
            Ok(()) => {}
            Err(error @ Error::CommandFailed { .. }) if check => {
                first_check_failure.get_or_insert(error);
            }
            Err(error) => return Err(error),
        }
    }
    if let Some(error) = first_check_failure {
        return Err(error);
    }
    Ok(())
}

fn ensure_cargo_shear_version() -> Result<()> {
    let output = tool_version_output("cargo-shear", CARGO_SHEAR_VERSION)?;
    if !output.status.success() {
        return Err(Error::CommandFailed {
            program: "cargo-shear".into(),
            status: output.status,
        });
    }

    let reported = String::from_utf8_lossy(&output.stdout);
    let has_expected_version = reported_version(&reported) == Some(CARGO_SHEAR_VERSION)
        || (reported_version(&reported) == Some("dev") && cargo_install_has_expected_version());
    if !has_expected_version {
        return Err(unexpected_tool_version(
            "cargo-shear",
            CARGO_SHEAR_VERSION,
            reported.trim(),
        ));
    }
    Ok(())
}

fn tool_version_output(program: &str, version: &str) -> Result<Output> {
    Command::new(program)
        .arg("--version")
        .output()
        .map_err(|source| tool_spawn_error(program, version, source))
}

fn install_cargo_tool(package: &str, version: &str) -> Result<()> {
    println!("Installing {package} {version}...");
    Process::new("cargo", false, 0)
        .args(["install", package, "--version", version, "--locked"])
        .run()
}

fn tool_spawn_error(program: &str, version: &str, source: io::Error) -> Error {
    if source.kind() == io::ErrorKind::NotFound {
        return Error::Message(format!(
            "required tool `{program}` was not found in PATH\n\nInstall the pinned repository \
             tools with:\n  make install-tools\n\nXKMake requires {program} {version}. If it is \
             already installed, ensure `~/.cargo/bin` is included in PATH."
        ));
    }

    Error::Spawn {
        program: program.to_owned(),
        source,
    }
}

fn unexpected_tool_version(program: &str, required: &str, reported: &str) -> Error {
    Error::Message(format!(
        "{program} {required} is required, but `{program} --version` reported \
         `{reported}`\n\nInstall the pinned repository tools with:\n  make install-tools"
    ))
}

fn cargo_install_has_expected_version() -> bool {
    let Some(executable) = find_cargo_shear() else {
        return false;
    };
    let Some(bin_dir) = executable.parent() else {
        return false;
    };
    if bin_dir.file_name().and_then(|name| name.to_str()) != Some("bin") {
        return false;
    }
    let Some(install_root) = bin_dir.parent() else {
        return false;
    };

    let Ok(output) = Command::new("cargo")
        .args(["install", "--list", "--root"])
        .arg(install_root)
        .output()
    else {
        return false;
    };
    output.status.success()
        && cargo_install_list_has_version(&String::from_utf8_lossy(&output.stdout))
}

fn find_cargo_shear() -> Option<PathBuf> {
    let executable = format!("cargo-shear{}", env::consts::EXE_SUFFIX);
    env::var_os("PATH").and_then(|path| {
        env::split_paths(&path)
            .map(|directory| directory.join(&executable))
            .find(|candidate| candidate.is_file())
    })
}

fn cargo_install_list_has_version(output: &str) -> bool {
    let expected = format!("cargo-shear v{CARGO_SHEAR_VERSION}:");
    output.lines().any(|line| line.trim() == expected)
}

fn reported_version(output: &str) -> Option<&str> {
    output.split_ascii_whitespace().last()
}

fn run_cargo_shear(root: &Path, relative_path: &str, fix: bool) -> Result<()> {
    let workspace = root.join(relative_path);
    println!("Checking dependencies in {}", workspace.display());

    let mut process = Process::new("cargo-shear", false, 0);
    // `--locked` is only meaningful where a `Cargo.lock` is committed: it
    // forbids the analysis from creating or updating the lockfile. The nested
    // tooling projects (`xtask`, `tee_apps`, `uapps/hello`) keep their
    // lockfiles gitignored, so on a fresh checkout no lock exists and
    // `--locked` would refuse to create one. Let cargo regenerate those on
    // demand instead.
    if cargo_lock_tracked(root, relative_path)? {
        process.arg("--locked");
    }
    if fix {
        process.arg("--fix");
    }
    process.arg(workspace.into_os_string()).run()
}

/// Whether the workspace's `Cargo.lock` is tracked by git. Only tracked
/// lockfiles can be enforced with `--locked`; gitignored tooling workspaces
/// lack a committed lock and would fail under `--locked` on a fresh checkout.
fn cargo_lock_tracked(root: &Path, relative_path: &str) -> Result<bool> {
    let lock_path = if relative_path == "." {
        "Cargo.lock".to_owned()
    } else {
        format!("{relative_path}/Cargo.lock")
    };
    let output = Command::new("git")
        .current_dir(root)
        .args(["ls-files", "--"])
        .arg(&lock_path)
        .output()
        .map_err(|source| Error::Spawn {
            program: "git".into(),
            source,
        })?;
    if !output.status.success() {
        return Err(Error::CommandFailed {
            program: "git".into(),
            status: output.status,
        });
    }
    Ok(!String::from_utf8_lossy(&output.stdout).trim().is_empty())
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::{
        cargo_install_list_has_version, reported_version, tool_spawn_error, unexpected_tool_version,
    };

    #[test]
    fn extracts_versions_from_supported_tool_output() {
        assert_eq!(reported_version("cargo-shear 1.13.2\n"), Some("1.13.2"));
        assert_eq!(reported_version("Version: 1.13.2\n"), Some("1.13.2"));
        assert_eq!(reported_version("licensure 0.8.1\n"), Some("0.8.1"));
    }

    #[test]
    fn recognizes_the_pinned_cargo_install_receipt() {
        let installed = "cargo-shear v1.13.2:\n    cargo-shear\n";
        assert!(cargo_install_list_has_version(installed));
        assert!(!cargo_install_list_has_version(
            "cargo-shear v1.13.1:\n    cargo-shear\n"
        ));
    }

    #[test]
    fn missing_tool_error_includes_installation_guidance() {
        let error = tool_spawn_error(
            "cargo-shear",
            "1.13.2",
            io::Error::from(io::ErrorKind::NotFound),
        );
        let message = error.to_string();

        assert!(message.contains("`cargo-shear` was not found in PATH"));
        assert!(message.contains("make install-tools"));
        assert!(message.contains("~/.cargo/bin"));
    }

    #[test]
    fn wrong_tool_version_points_to_the_shared_installer() {
        let message = unexpected_tool_version("licensure", "0.8.1", "licensure 0.7.0").to_string();

        assert!(message.contains("licensure 0.8.1 is required"));
        assert!(message.contains("make install-tools"));
    }
}

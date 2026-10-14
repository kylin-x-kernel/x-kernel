// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Repository hygiene orchestration.

use std::{
    collections::HashSet,
    env, fs, io,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde::Deserialize;

use crate::{
    cli::HygieneFixArgs,
    context::workspace_root,
    error::{Error, IoResultExt, Result},
    process::Process,
};

const CARGO_SHEAR_VERSION: &str = "1.13.2";
const LICENSURE_VERSION: &str = "0.8.1";
pub(crate) const DYLINT_VERSION: &str = "6.0.4";
const CARGO_WORKSPACES: &[&str] = &[".", "xtask"];
const LICENSURE_FILE_BATCH_SIZE: usize = 256;

pub(crate) fn install_tools() -> Result<()> {
    install_cargo_tool("cargo-shear", CARGO_SHEAR_VERSION)?;
    install_cargo_tool("licensure", LICENSURE_VERSION)?;
    install_cargo_tool("cargo-dylint", DYLINT_VERSION)?;
    install_cargo_tool("dylint-link", DYLINT_VERSION)
}

pub(crate) fn ensure_dylint_tools() -> Result<()> {
    // cargo-dylint is a Cargo subcommand and expects its subcommand name.
    let output = Command::new("cargo-dylint")
        .args(["dylint", "--version"])
        .output()
        .map_err(|source| tool_spawn_error("cargo-dylint", DYLINT_VERSION, source))?;
    if !output.status.success() {
        return Err(Error::CommandFailed {
            program: "cargo-dylint".into(),
            status: output.status,
        });
    }
    let reported = String::from_utf8_lossy(&output.stdout);
    if reported.trim() != format!("cargo-dylint {DYLINT_VERSION}") {
        return Err(unexpected_tool_version(
            "cargo-dylint",
            DYLINT_VERSION,
            reported.trim(),
        ));
    }

    // dylint-link is a linker wrapper: `--version` is forwarded to the linker,
    // so only its presence is checked here. `make install-tools` pins its version.
    if find_cargo_tool("dylint-link").is_none() {
        return Err(tool_spawn_error(
            "dylint-link",
            DYLINT_VERSION,
            io::ErrorKind::NotFound.into(),
        ));
    }
    Ok(())
}

fn find_cargo_tool(program: &str) -> Option<PathBuf> {
    let executable = format!("{program}{}", env::consts::EXE_SUFFIX);
    env::var_os("PATH").and_then(|path| {
        env::split_paths(&path)
            .map(|directory| directory.join(&executable))
            .find(|candidate| candidate.is_file())
    })
}

pub(crate) fn check_dependencies(args: &HygieneFixArgs) -> Result<()> {
    ensure_cargo_shear_version()?;
    let root = workspace_root()?;
    check_cargo_shear_ignores(&root)?;

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

/// Fails when a root workspace member does not inherit `[workspace.lints]`.
///
/// A member without `[lints] workspace = true` silently skips every workspace
/// rustc/Clippy level, including the lint-exception rules.
pub(crate) fn check_lint_inheritance() -> Result<()> {
    let root = workspace_root()?;
    let mut missing = Vec::new();
    for manifest_path in workspace_member_manifests(&root)? {
        let content = fs::read_to_string(&manifest_path).with_path(&manifest_path)?;
        if !inherits_workspace_lints(&content).map_err(|error| {
            Error::Message(format!(
                "failed to parse {}: {error}",
                manifest_path.display()
            ))
        })? {
            missing.push(
                manifest_path
                    .strip_prefix(&root)
                    .unwrap_or(&manifest_path)
                    .display()
                    .to_string(),
            );
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    missing.sort();
    Err(Error::Message(format!(
        "workspace members must inherit the root lint levels; add `[lints] workspace = true` \
         to:\n  {}",
        missing.join("\n  ")
    )))
}

/// Fails when kernel crate test sources are outside the agreed layout.
pub(crate) fn check_test_layout() -> Result<()> {
    let root = workspace_root()?;
    let mut violations = Vec::new();
    for manifest_path in workspace_member_manifests(&root)? {
        let Some(crate_root) = manifest_path.parent() else {
            continue;
        };
        let crate_name = crate_root
            .strip_prefix(&root)
            .unwrap_or(crate_root)
            .display()
            .to_string();
        for source_path in tracked_rust_files_under(&root, crate_root)? {
            let relative_path = source_path.strip_prefix(crate_root).unwrap_or(&source_path);
            let content = fs::read_to_string(&source_path).with_path(&source_path)?;
            collect_test_layout_violations(&crate_name, relative_path, &content, &mut violations);
        }
    }
    if violations.is_empty() {
        return Ok(());
    }
    violations.sort();
    Err(Error::Message(format!(
        "test source layout violations:\n  {}",
        violations.join("\n  ")
    )))
}

/// Fails when a crate with `<crate>/tests/*.rs` cannot compile those tests
/// with a bare host Cargo invocation.
pub(crate) fn check_host_tests() -> Result<()> {
    let root = workspace_root()?;
    let mut failures = Vec::new();
    for manifest_path in workspace_member_manifests(&root)? {
        let Some(crate_root) = manifest_path.parent() else {
            continue;
        };
        let rust_files = tracked_rust_files_under(&root, crate_root)?;
        if !has_cargo_integration_tests(crate_root, &rust_files) {
            continue;
        }
        let manifest_content = fs::read_to_string(&manifest_path).with_path(&manifest_path)?;
        let required_feature_tests =
            required_feature_test_targets(&manifest_content).map_err(|error| {
                Error::Message(format!(
                    "failed to parse host test targets in {}: {error}",
                    manifest_path.display()
                ))
            })?;
        let display_path = manifest_path
            .strip_prefix(&root)
            .unwrap_or(&manifest_path)
            .display();
        eprintln!("checking host Cargo tests in {display_path}");
        let status = Command::new("cargo")
            .current_dir(&root)
            .args(["test", "--manifest-path"])
            .arg(&manifest_path)
            .args(["--tests", "--no-run"])
            .status()
            .map_err(|source| Error::Spawn {
                program: "cargo".into(),
                source,
            })?;
        if !status.success() {
            failures.push(display_path.to_string());
        }
        for test in required_feature_tests {
            eprintln!(
                "checking host Cargo test {} in {display_path} with features {}",
                test.name,
                test.required_features.join(",")
            );
            let status = Command::new("cargo")
                .current_dir(&root)
                .args(["test", "--manifest-path"])
                .arg(&manifest_path)
                .args(["--test", &test.name, "--no-run", "--features"])
                .arg(test.required_features.join(","))
                .status()
                .map_err(|source| Error::Spawn {
                    program: "cargo".into(),
                    source,
                })?;
            if !status.success() {
                failures.push(format!("{display_path} ({})", test.name));
            }
        }
    }
    if failures.is_empty() {
        return Ok(());
    }
    failures.sort();
    Err(Error::Message(format!(
        "Cargo integration tests must compile under bare Cargo; fix or move tests out of \
         <crate>/tests/:\n  {}",
        failures.join("\n  ")
    )))
}

#[derive(Deserialize)]
struct CargoMetadata {
    packages: Vec<MetadataPackage>,
    workspace_members: Vec<String>,
}

#[derive(Deserialize)]
struct MetadataPackage {
    id: String,
    manifest_path: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
struct RequiredFeatureTestTarget {
    name: String,
    required_features: Vec<String>,
}

#[derive(Deserialize)]
struct HostTestManifest {
    #[serde(default)]
    test: Vec<HostTestTarget>,
}

#[derive(Deserialize)]
struct HostTestTarget {
    name: String,
    #[serde(default, rename = "required-features")]
    required_features: Vec<String>,
}

fn workspace_member_manifests(root: &Path) -> Result<Vec<PathBuf>> {
    // Path dependencies inside the workspace directory are implicit members,
    // so Cargo, not the `members` globs, is the source of truth.
    let output = Command::new("cargo")
        .current_dir(root)
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()
        .map_err(|source| Error::Spawn {
            program: "cargo".into(),
            source,
        })?;
    if !output.status.success() {
        return Err(Error::CommandFailed {
            program: "cargo metadata".into(),
            status: output.status,
        });
    }
    let metadata: CargoMetadata = serde_json::from_slice(&output.stdout)
        .map_err(|error| Error::Message(format!("failed to parse cargo metadata: {error}")))?;
    let members: HashSet<_> = metadata.workspace_members.into_iter().collect();
    Ok(metadata
        .packages
        .into_iter()
        .filter(|package| members.contains(&package.id))
        .map(|package| package.manifest_path)
        .collect())
}

fn inherits_workspace_lints(manifest: &str) -> std::result::Result<bool, toml::de::Error> {
    let manifest: toml::Table = toml::from_str(manifest)?;
    Ok(manifest
        .get("lints")
        .and_then(|lints| lints.get("workspace"))
        .and_then(toml::Value::as_bool)
        == Some(true))
}

fn tracked_rust_files_under(root: &Path, directory: &Path) -> Result<Vec<PathBuf>> {
    let output = Command::new("git")
        .current_dir(root)
        .args(["ls-files", "--cached", "--others", "--exclude-standard", "--"])
        .arg(directory)
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
        .filter(|path| path.ends_with(".rs"))
        .map(|path| root.join(path))
        .filter(|path| path.is_file())
        .collect())
}

fn has_cargo_integration_tests(crate_root: &Path, rust_files: &[PathBuf]) -> bool {
    rust_files.iter().any(|path| {
        path.strip_prefix(crate_root)
            .ok()
            .is_some_and(is_cargo_integration_test_root)
    })
}

fn is_cargo_integration_test_root(path: &Path) -> bool {
    path.parent().is_some_and(|parent| parent == Path::new("tests"))
        && path.extension().is_some_and(|extension| extension == "rs")
}

fn required_feature_test_targets(
    manifest: &str,
) -> std::result::Result<Vec<RequiredFeatureTestTarget>, toml::de::Error> {
    let manifest: HostTestManifest = toml::from_str(manifest)?;
    Ok(manifest
        .test
        .into_iter()
        .filter(|test| !test.required_features.is_empty())
        .map(|test| RequiredFeatureTestTarget {
            name: test.name,
            required_features: test.required_features,
        })
        .collect())
}

fn collect_test_layout_violations(
    crate_name: &str,
    relative_path: &Path,
    content: &str,
    violations: &mut Vec<String>,
) {
    let path = relative_path.to_string_lossy().replace('\\', "/");
    if path.starts_with("host-tests/") {
        violations.push(format!(
            "{crate_name}/{path}: host tests must live in <crate>/tests/"
        ));
    }
    if path.starts_with("src/") && is_src_test_file(relative_path) {
        violations.push(format!(
            "{crate_name}/{path}: guest unit tests must live in <crate>/ut-tests/"
        ));
    }
    if path.starts_with("src/") && has_inline_test_module(content) {
        violations.push(format!(
            "{crate_name}/{path}: use unittest::path_mod_test! with files under ut-tests/"
        ));
    }
    if path.starts_with("ut-tests/") && has_host_test_attr(content) {
        violations.push(format!(
            "{crate_name}/{path}: ut-tests must use #[def_test], not #[test]"
        ));
    }
    if path.starts_with("tests/") && has_guest_test_marker(content) {
        violations.push(format!(
            "{crate_name}/{path}: Cargo tests must use #[test], not unittest #[def_test]"
        ));
    }
    if (path.starts_with("ut-tests/") || path.starts_with("tests/"))
        && has_handwritten_panic_expect_attr(content)
    {
        violations.push(format!(
            "{crate_name}/{path}: test code must not hand-write unwrap/expect/panic \
             #[expect]; rely on the test harness lint policy"
        ));
    }
}

fn is_src_test_file(path: &Path) -> bool {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    file_name == "tests.rs" || file_name.starts_with("test_") || file_name.ends_with("_tests.rs")
}

fn has_inline_test_module(content: &str) -> bool {
    let compact: String = content.chars().filter(|ch| !ch.is_whitespace()).collect();
    compact.contains("#[cfg(test)]mod")
        || compact.contains("#[cfg(unittest)]mod")
        || compact.contains("#[unittest::mod_test]mod")
        || compact.contains("#[mod_test]mod")
        || compact.contains("include!(")
}

fn has_host_test_attr(content: &str) -> bool {
    content.contains("#[test]")
}

fn has_guest_test_marker(content: &str) -> bool {
    content.contains("#[def_test") || content.contains("unittest::def_test")
}

fn has_handwritten_panic_expect_attr(content: &str) -> bool {
    content.lines().any(|line| {
        let code = line
            .split_once("//")
            .map_or(line, |(code, _comment)| code)
            .trim();
        (code.starts_with("#[expect(") || code.starts_with("#![expect("))
            && ["unwrap_used", "expect_used", "panic"]
                .iter()
                .any(|lint| code.contains(&format!("clippy::{lint}")))
    })
}

fn check_cargo_shear_ignores(root: &Path) -> Result<()> {
    let mut violations = Vec::new();
    let mut manifests = workspace_member_manifests(root)?;
    manifests.push(root.join("Cargo.toml"));
    manifests.sort();
    manifests.dedup();
    for manifest_path in manifests {
        let content = fs::read_to_string(&manifest_path).with_path(&manifest_path)?;
        collect_cargo_shear_ignore_violations(root, &manifest_path, &content, &mut violations)?;
    }
    if violations.is_empty() {
        return Ok(());
    }
    violations.sort();
    Err(Error::Message(format!(
        "cargo-shear ignored dependency entries need an adjacent reason comment:\n  {}",
        violations.join("\n  ")
    )))
}

fn collect_cargo_shear_ignore_violations(
    root: &Path,
    manifest_path: &Path,
    content: &str,
    violations: &mut Vec<String>,
) -> Result<()> {
    let manifest: toml::Table = toml::from_str(content).map_err(|error| {
        Error::Message(format!(
            "failed to parse {}: {error}",
            manifest_path.display()
        ))
    })?;
    let has_ignored =
        has_cargo_shear_ignored(&manifest, &["package", "metadata", "cargo-shear"])
            || has_cargo_shear_ignored(&manifest, &["workspace", "metadata", "cargo-shear"]);
    if has_ignored && !ignored_entry_has_reason(content) {
        violations.push(format!(
            "{}: cargo-shear ignored entries must explain why the dependency is hidden",
            manifest_path
                .strip_prefix(root)
                .unwrap_or(manifest_path)
                .display()
        ));
    }
    Ok(())
}

fn has_cargo_shear_ignored(manifest: &toml::Table, path: &[&str]) -> bool {
    let mut value = toml::Value::Table(manifest.clone());
    for key in path {
        let Some(next) = value.get(key).cloned() else {
            return false;
        };
        value = next;
    }
    value
        .get("ignored")
        .and_then(toml::Value::as_array)
        .is_some_and(|ignored| !ignored.is_empty())
}

fn ignored_entry_has_reason(manifest: &str) -> bool {
    let mut current_section = "";
    let mut pending_comments = Vec::new();
    let mut found_ignored = false;
    let mut all_ignored_have_reasons = true;
    for raw_line in manifest.lines() {
        let line = raw_line.trim();
        if line.starts_with('[') {
            let (header, comment) = split_toml_line_comment(line);
            if let Some(section) = toml_section_name(header) {
                let section_leading_comments = std::mem::take(&mut pending_comments);
                current_section = section;
                if is_cargo_shear_section(section) {
                    pending_comments = section_leading_comments;
                    if comment.is_some_and(|comment| !comment.trim().is_empty()) {
                        pending_comments.push(line);
                    }
                }
            } else {
                pending_comments.clear();
            }
            continue;
        }
        if line.is_empty() {
            pending_comments.clear();
            continue;
        }
        if line.starts_with('#') {
            pending_comments.push(line);
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            pending_comments.clear();
            continue;
        };
        if is_cargo_shear_ignored_key(current_section, key.trim(), value) {
            found_ignored = true;
            all_ignored_have_reasons &= !pending_comments.is_empty()
                || split_toml_line_comment(value)
                    .1
                    .is_some_and(|comment| !comment.trim().is_empty());
        }
        pending_comments.clear();
    }
    found_ignored && all_ignored_have_reasons
}

fn split_toml_line_comment(line: &str) -> (&str, Option<&str>) {
    line.split_once('#')
        .map_or((line, None), |(before, comment)| (before.trim_end(), Some(comment)))
}

fn toml_section_name(header: &str) -> Option<&str> {
    header
        .strip_prefix('[')?
        .strip_suffix(']')
        .map(str::trim)
}

fn is_cargo_shear_section(section: &str) -> bool {
    matches!(
        section,
        "package.metadata.cargo-shear" | "workspace.metadata.cargo-shear"
    )
}

fn is_cargo_shear_ignored_key(section: &str, key: &str, value: &str) -> bool {
    let full_key = if section.is_empty() {
        key.to_owned()
    } else {
        format!("{section}.{key}")
    };
    if matches!(
        full_key.as_str(),
        "package.metadata.cargo-shear.ignored" | "workspace.metadata.cargo-shear.ignored"
    ) {
        return true;
    }
    matches!(
        full_key.as_str(),
        "package.metadata.cargo-shear" | "workspace.metadata.cargo-shear"
    ) && value.contains("ignored")
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
    let Some(executable) = find_cargo_tool("cargo-shear") else {
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
        cargo_install_list_has_version, collect_cargo_shear_ignore_violations,
        collect_test_layout_violations, has_cargo_integration_tests, has_inline_test_module,
        has_handwritten_panic_expect_attr, ignored_entry_has_reason,
        inherits_workspace_lints, is_src_test_file, reported_version, required_feature_test_targets,
        tool_spawn_error, unexpected_tool_version, RequiredFeatureTestTarget,
    };

    #[test]
    fn accepts_only_workspace_lint_inheritance() {
        let package = "[package]\nname = \"demo\"\n";
        assert!(
            inherits_workspace_lints(&format!("{package}[lints]\nworkspace = true\n")).unwrap()
        );
        assert!(inherits_workspace_lints(&format!("lints.workspace = true\n{package}")).unwrap());
        assert!(!inherits_workspace_lints(package).unwrap());
        assert!(
            !inherits_workspace_lints(&format!("{package}[lints]\nworkspace = false\n")).unwrap()
        );
        assert!(
            !inherits_workspace_lints(&format!("{package}[lints.clippy]\nall = \"allow\"\n"))
                .unwrap()
        );
    }

    #[test]
    fn recognizes_src_test_file_names() {
        assert!(is_src_test_file("src/copy_tests.rs".as_ref()));
        assert!(is_src_test_file("src/tests.rs".as_ref()));
        assert!(is_src_test_file("src/foo/test_copy.rs".as_ref()));
        assert!(!is_src_test_file("src/contest.rs".as_ref()));
    }

    #[test]
    fn recognizes_inline_test_modules_without_layout_sensitive_paths() {
        assert!(has_inline_test_module("#[cfg(unittest)]\nmod copy_tests;"));
        assert!(has_inline_test_module("#[cfg(test)] mod tests { }"));
        assert!(has_inline_test_module("#[mod_test]\nmod tests { }"));
        assert!(has_inline_test_module("mod tests { include!(\"../ut-tests/lib.rs\"); }"));
        assert!(!has_inline_test_module(
            "unittest::path_mod_test!(tests, \"../ut-tests/lib.rs\");"
        ));
    }

    #[test]
    fn reports_guest_tests_left_under_src() {
        let mut violations = Vec::new();
        collect_test_layout_violations(
            "core/kuaccess",
            "src/copy_tests.rs".as_ref(),
            "use unittest::def_test;\n#[def_test]\nfn copies() {}",
            &mut violations,
        );

        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("guest unit tests must live in <crate>/ut-tests/"));
    }

    #[test]
    fn reports_wrong_macro_for_each_test_directory() {
        let mut violations = Vec::new();
        collect_test_layout_violations(
            "core/demo",
            "ut-tests/host_style.rs".as_ref(),
            "#[test]\nfn host_style() {}",
            &mut violations,
        );
        collect_test_layout_violations(
            "core/demo",
            "tests/guest_style.rs".as_ref(),
            "use unittest::def_test;\n#[def_test]\nfn guest_style() {}",
            &mut violations,
        );

        assert_eq!(violations.len(), 2);
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("ut-tests must use #[def_test]"))
        );
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("Cargo tests must use #[test]"))
        );
    }

    #[test]
    fn reports_handwritten_panic_expect_attrs_in_test_sources() {
        assert!(has_handwritten_panic_expect_attr(
            "#[expect(clippy::expect_used, reason = \"test scenario: setup\")]\nfn helper() {}"
        ));
        assert!(has_handwritten_panic_expect_attr(
            "#![expect(clippy::panic, reason = \"test scenario: setup\")]\nfn helper() {}"
        ));
        assert!(!has_handwritten_panic_expect_attr(
            "#[allow(clippy::unwrap_used, reason = \"test scenario: setup\")]\nfn helper() {}"
        ));
        assert!(!has_handwritten_panic_expect_attr(
            "// #[expect(clippy::expect_used, reason = \"old text\")]\nfn helper() {}"
        ));
        assert!(!has_handwritten_panic_expect_attr(
            "#[expect(\n    clippy::expect_used,\n    reason = \"test scenario: setup\"\n)]\nfn helper() {}"
        ));

        let mut violations = Vec::new();
        collect_test_layout_violations(
            "drivers/integration/kdriver",
            "ut-tests/block_irq_unittest.rs".as_ref(),
            "#[expect(clippy::expect_used, reason = \"test scenario: setup\")]\nfn helper() {}",
            &mut violations,
        );
        collect_test_layout_violations(
            "util/kerrno",
            "tests/kerrno_host.rs".as_ref(),
            "#![expect(clippy::panic, reason = \"test scenario: setup\")]\nfn helper() {}",
            &mut violations,
        );

        assert_eq!(violations.len(), 2);
        assert!(violations.iter().all(|violation| {
            violation.contains("test code must not hand-write unwrap/expect/panic #[expect]")
        }));
    }

    #[test]
    fn recognizes_only_root_cargo_integration_tests() {
        let crate_root = std::path::Path::new("/repo/task/demo");
        let files = vec![
            crate_root.join("tests/mutex.rs"),
            crate_root.join("tests/common/mod.rs"),
            crate_root.join("src/tests.rs"),
            crate_root.join("ut-tests/lib.rs"),
        ];

        assert!(has_cargo_integration_tests(crate_root, &files));

        let support_only = vec![
            crate_root.join("tests/common/mod.rs"),
            crate_root.join("src/tests.rs"),
            crate_root.join("ut-tests/lib.rs"),
        ];
        assert!(!has_cargo_integration_tests(crate_root, &support_only));
    }

    #[test]
    fn extracts_required_feature_host_tests() {
        let targets = required_feature_test_targets(
            r#"
                [[test]]
                name = "allocator"
                path = "tests/allocator.rs"
                required-features = ["full"]

                [[test]]
                name = "plain"
                path = "tests/plain.rs"

                [[test]]
                name = "pkix_der"
                path = "tests/pkix_der.rs"
                required-features = ["pkix", "alloc"]
            "#,
        )
        .unwrap();

        assert_eq!(
            targets,
            vec![
                RequiredFeatureTestTarget {
                    name: "allocator".to_owned(),
                    required_features: vec!["full".to_owned()],
                },
                RequiredFeatureTestTarget {
                    name: "pkix_der".to_owned(),
                    required_features: vec!["pkix".to_owned(), "alloc".to_owned()],
                },
            ]
        );
    }

    #[test]
    fn cargo_shear_ignore_entries_need_reason_comments() {
        assert!(!ignored_entry_has_reason(
            "[package.metadata.cargo-shear]\nignored = [\"kirq\"]\n"
        ));
        assert!(ignored_entry_has_reason(
            "[package.metadata.cargo-shear]\n# kirq is used only by cfg(unittest) tests.\nignored = [\"kirq\"]\n"
        ));
        assert!(ignored_entry_has_reason(
            "# kirq is used only by cfg(unittest) tests.\n[package.metadata.cargo-shear]\nignored = [\"kirq\"]\n"
        ));
        assert!(ignored_entry_has_reason(
            "[package.metadata.cargo-shear] # kirq is used only by cfg(unittest) tests.\nignored = [\"kirq\"]\n"
        ));
        assert!(ignored_entry_has_reason(
            "[package.metadata.cargo-shear]\nignored = [\"kirq\"] # used only by cfg(unittest) tests\n"
        ));
        assert!(ignored_entry_has_reason(
            "[workspace.metadata.cargo-shear]\n# unittest is used only by cfg(unittest) tests.\nignored = [\"unittest\"]\n"
        ));
        assert!(!ignored_entry_has_reason(
            "[dependencies] # normal dependency section.\n[package.metadata.cargo-shear]\nignored = [\"kirq\"]\n"
        ));
        assert!(ignored_entry_has_reason(
            "[package.metadata.cargo-shear]\nignored-paths = [\"**/ut-tests/*.rs\"]\n# hidden because cfg(unittest) tests use it.\nignored = [\"unittest\"]\n"
        ));
        assert!(!ignored_entry_has_reason(
            "[package.metadata.cargo-shear]\n# paths are ignored by layout convention.\nignored-paths = [\"**/ut-tests/*.rs\"]\nignored = [\"unittest\"]\n"
        ));
        assert!(!ignored_entry_has_reason(
            "[package.metadata.cargo-shear]\nignored-paths = [\n# paths are ignored by layout convention.\n[\"**/ut-tests/*.rs\"],\n]\nignored = [\"unittest\"]\n"
        ));
        assert!(!ignored_entry_has_reason(
            "package.metadata.cargo-shear.ignored = [\"kirq\"]\n"
        ));
        assert!(ignored_entry_has_reason(
            "# kirq is used only by cfg(unittest) tests.\npackage.metadata.cargo-shear.ignored = [\"kirq\"]\n"
        ));
        assert!(!ignored_entry_has_reason(
            "package.metadata.cargo-shear = { ignored = [\"kirq\"] }\n"
        ));
        assert!(ignored_entry_has_reason(
            "package.metadata.cargo-shear = { ignored = [\"kirq\"] } # cfg(unittest) tests use kirq\n"
        ));
        assert!(!ignored_entry_has_reason(
            "[package.metadata]\ncargo-shear.ignored = [\"kirq\"]\n"
        ));
        assert!(ignored_entry_has_reason(
            "[package.metadata]\n# kirq is used only by cfg(unittest) tests.\ncargo-shear.ignored = [\"kirq\"]\n"
        ));
        assert!(!ignored_entry_has_reason(
            "[package.metadata.cargo-shear]\n# kirq is used only by cfg(unittest) tests.\nignored = [\"kirq\"]\n\n[workspace.metadata.cargo-shear]\nignored = [\"unittest\"]\n"
        ));
        assert!(ignored_entry_has_reason(
            "[package.metadata.cargo-shear]\n# kirq is used only by cfg(unittest) tests.\nignored = [\"kirq\"]\n\n[workspace.metadata.cargo-shear]\n# unittest is used only by cfg(unittest) tests.\nignored = [\"unittest\"]\n"
        ));

        let mut violations = Vec::new();
        collect_cargo_shear_ignore_violations(
            "/repo".as_ref(),
            "/repo/task/kworkng/Cargo.toml".as_ref(),
            "[package]\nname = \"demo\"\n[package.metadata.cargo-shear]\nignored = [\"kirq\"]\n",
            &mut violations,
        )
        .unwrap();

        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("ignored entries must explain why"));

        let mut workspace_violations = Vec::new();
        collect_cargo_shear_ignore_violations(
            "/repo".as_ref(),
            "/repo/Cargo.toml".as_ref(),
            "[workspace]\n[workspace.metadata.cargo-shear]\nignored = [\"unittest\"]\n",
            &mut workspace_violations,
        )
        .unwrap();

        assert_eq!(workspace_violations.len(), 1);
        assert!(workspace_violations[0].contains("Cargo.toml"));

        let mut dotted_violations = Vec::new();
        collect_cargo_shear_ignore_violations(
            "/repo".as_ref(),
            "/repo/task/kworkng/Cargo.toml".as_ref(),
            "[package]\nname = \"demo\"\nmetadata.cargo-shear.ignored = [\"kirq\"]\n",
            &mut dotted_violations,
        )
        .unwrap();

        assert_eq!(dotted_violations.len(), 1);

        let mut inline_violations = Vec::new();
        collect_cargo_shear_ignore_violations(
            "/repo".as_ref(),
            "/repo/task/kworkng/Cargo.toml".as_ref(),
            "[package]\nname = \"demo\"\nmetadata.cargo-shear = { ignored = [\"kirq\"] }\n",
            &mut inline_violations,
        )
        .unwrap();

        assert_eq!(inline_violations.len(), 1);
    }

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

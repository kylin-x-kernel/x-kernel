// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Build and run the repository's pinned Dylint lint library.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use serde::Deserialize;
use xconfig::build_config::BuildProfile;

use crate::{
    build::{apply_kernel_env, generate_build_configuration, kernel_feature_flags},
    cli::DylintArgs,
    context::{BuildContext, workspace_root},
    error::{Error, IoResultExt, Result},
    facts::{
        DEPS_ENV, FACTS_ENV, FactsIdentity, FactsRun, FailureReason, MESSAGE_FORMAT_ARG, ROOT_ENV,
        SCAN_RUSTFLAGS, WRAPPER_TARGET_ENV,
    },
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

#[derive(Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoPackage>,
}

#[derive(Deserialize)]
struct CargoPackage {
    id: String,
    name: String,
    source: Option<String>,
    manifest_path: PathBuf,
}

#[derive(Clone, Deserialize)]
struct UnitGraph {
    roots: Vec<usize>,
    units: Vec<CargoUnit>,
}

#[derive(Clone, Deserialize)]
struct CargoUnit {
    pkg_id: String,
    features: Vec<String>,
    dependencies: Vec<CargoUnitDependency>,
}

#[derive(Clone, Deserialize)]
struct CargoUnitDependency {
    index: usize,
}

struct FixSelection {
    packages: Vec<String>,
    features: Vec<String>,
}

pub(crate) fn run(args: &DylintArgs) -> Result<()> {
    let facts_out = args
        .facts_out
        .as_deref()
        .filter(|_| !args.build.workspace.dry_run);
    // A facts run that fails before `FactsRun::begin` (configuration, tool
    // probes) must not leave an earlier artifact behind.
    if let Some(dir) = facts_out {
        FactsRun::clear(&workspace_root()?.join(dir))?;
    }
    let context = BuildContext::create(&args.build)?;
    // From here on every failure of a facts run leaves a status behind.
    let facts = match facts_out {
        Some(dir) => Some(FactsRun::begin(
            &context,
            dir,
            FactsIdentity {
                commit: args.facts_commit.clone(),
                base: args.facts_base.clone(),
            },
            facts_deps(args),
        )?),
        None => None,
    };
    let lint_dir = context.workspace_root.join(LINT_DIR);
    let (toolchain, host) = match probe_tools(args, &context, &lint_dir) {
        Ok(tools) => tools,
        Err(error) => {
            if let Some(facts) = &facts {
                facts.fail(
                    FailureReason::Build,
                    &format!("scan did not start: {error}"),
                )?;
            }
            return Err(error);
        }
    };
    let result = build_and_scan(args, &context, &lint_dir, &toolchain, &host, facts.as_ref());
    let Some(facts) = facts else {
        return result.map_err(ScanError::into_inner);
    };
    match &result {
        Err(ScanError::Setup(error)) => {
            facts.fail(
                FailureReason::Build,
                &format!("scan did not start: {error}"),
            )?;
        }
        Err(ScanError::Scan(_)) | Ok(()) => {
            let config_path = context.workspace_root.join(&args.build.workspace.config);
            facts.finish(&context, &config_path, &toolchain, result.is_ok())?;
        }
    }
    result.map_err(ScanError::into_inner)
}

/// Reads the pinned lint toolchain and checks the tools the run needs;
/// returns the toolchain and the host triple.
fn probe_tools(
    args: &DylintArgs,
    context: &BuildContext,
    lint_dir: &Path,
) -> Result<(String, String)> {
    let toolchain = read_toolchain(&lint_dir.join("rust-toolchain.toml"))?;
    let host = probe_host(context.dry_run, || {
        let host = rustc_host(&toolchain)?;
        ensure_dylint_tools()?;
        ensure_rustc_dev(&toolchain)?;
        if !args.self_test {
            ensure_target(&toolchain, context.config.target())?;
        }
        Ok(host)
    })?;
    Ok((toolchain, host))
}

/// Dependency crates are exported: `--facts-deps` or `XKERNEL_FACTS_DEPS=1`.
fn facts_deps(args: &DylintArgs) -> bool {
    args.facts_deps || env_is_one(DEPS_ENV)
}

fn env_is_one(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| value == "1")
}

/// Failure of a Dylint run: before the kernel scan started, or of the scan.
enum ScanError {
    Setup(Error),
    Scan(Error),
}

impl ScanError {
    fn into_inner(self) -> Error {
        match self {
            Self::Setup(error) | Self::Scan(error) => error,
        }
    }
}

fn build_and_scan(
    args: &DylintArgs,
    context: &BuildContext,
    lint_dir: &Path,
    toolchain: &str,
    host: &str,
    facts: Option<&FactsRun>,
) -> std::result::Result<(), ScanError> {
    let driver_path = prepare_driver_path(context).map_err(ScanError::Setup)?;
    let lint_manifest = lint_dir.join("Cargo.toml");
    let lint_target_dir = context.target_dir.join("tools/lints");
    let mut lint = Process::new("cargo", context.dry_run, context.verbosity);
    lint.current_dir(lint_dir)
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
    pin_lint_build_env(&mut lint, &lint_target_dir);
    set_host_tools(&mut lint, host);
    if let Some(path) = &driver_path {
        lint.env("DYLINT_DRIVER_PATH", path);
    }
    lint.run().map_err(ScanError::Setup)?;

    if args.self_test {
        return Ok(());
    }

    let library = lint_library_path(&lint_target_dir, toolchain, host);
    if !context.dry_run && !library.is_file() {
        return Err(ScanError::Setup(Error::Message(format!(
            "Dylint library not found at {}; ensure dylint-link {DYLINT_VERSION} is installed \
             (run make install-tools)",
            library.display()
        ))));
    }

    generate_build_configuration(context).map_err(ScanError::Setup)?;

    let mut scan = Process::new("cargo", context.dry_run, context.verbosity);
    let features = kernel_feature_flags(context);
    let selection = if args.fix && !context.dry_run {
        Some(configured_fix_selection(context, toolchain, &features).map_err(ScanError::Setup)?)
    } else {
        None
    };
    if args.fix && context.dry_run {
        println!(
            "Fix package selection is resolved at run time from Cargo's configured-target \
             application unit graph; fix features are checked against the normal scan."
        );
    }
    let dry_run_packages = ["<configured-local-packages>".to_owned()];
    let packages = if let Some(selection) = &selection {
        &selection.packages[..]
    } else if args.fix && context.dry_run {
        &dry_run_packages[..]
    } else {
        &[][..]
    };
    let scan_features = selection
        .as_ref()
        .map_or(&features, |selected| &selected.features);
    scan.current_dir(&context.workspace_root)
        .args(scan_args(ScanOptions {
            toolchain,
            library: &library,
            manifest: &context.app_manifest,
            target: context.config.target(),
            release: context.config.profile() == BuildProfile::Release,
            features: scan_features,
            fix: args.fix,
            packages,
        }));
    if args.facts_out.is_some() {
        scan.arg(MESSAGE_FORMAT_ARG);
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
    apply_kernel_env(&mut scan, context);
    sanitize_env(&mut scan);
    set_host_tools(&mut scan, host);
    scan.env("CARGO_TARGET_DIR", context.target_dir.join("dylint"))
        .env("DYLINT_RUSTFLAGS", SCAN_RUSTFLAGS.join(" "));
    if let Some(path) = &driver_path {
        scan.env("DYLINT_DRIVER_PATH", path);
    }
    if args.facts_out.is_some() {
        // Without `--facts-out` the variable passes through unchanged, so a
        // caller that keeps it at `1` for every scan keeps Cargo's cache.
        scan.env(FACTS_ENV, "1");
    }
    let exporting = args.facts_out.is_some() || env_is_one(FACTS_ENV);
    if exporting {
        // Dependencies are compiled from their own package directories;
        // the exporter writes paths under the repository relative to it.
        scan.env(ROOT_ENV, &context.workspace_root);
    }
    if exporting && facts_deps(args) && !context.dry_run {
        // Cargo fingerprints the wrapper, so a caller that exports
        // dependencies keeps doing so for every scan of a target directory.
        let wrapper = std::env::current_exe().map_err(|source| {
            ScanError::Setup(Error::Io {
                path: PathBuf::from("xkmake"),
                source,
            })
        })?;
        scan.env("RUSTC_WRAPPER", wrapper)
            .env(WRAPPER_TARGET_ENV, context.config.target());
    }
    match facts {
        Some(facts) => scan.run_with_stdout(facts.messages_path()),
        None => scan.run(),
    }
    .map_err(ScanError::Scan)
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

struct ScanOptions<'a> {
    toolchain: &'a str,
    library: &'a Path,
    manifest: &'a Path,
    target: &'a str,
    release: bool,
    features: &'a [String],
    fix: bool,
    packages: &'a [String],
}

fn scan_args(options: ScanOptions<'_>) -> Vec<String> {
    let mut args = vec![
        format!("+{}", options.toolchain),
        "dylint".into(),
        "--no-metadata".into(),
        "--lib-path".into(),
        options.library.display().to_string(),
        "--manifest-path".into(),
        options.manifest.display().to_string(),
    ];
    if options.fix {
        args.push("--fix".into());
    }
    args.extend(["--".into(), "--target".into(), options.target.into()]);
    if options.fix {
        // Selecting dependency crates as Cargo roots must not enable their
        // default features unless the application graph already did so.
        args.push("--no-default-features".into());
    }
    if options.release {
        args.push("--release".into());
    }
    if !options.features.is_empty() {
        args.extend(["--features".into(), options.features.join(",")]);
    }
    if options.fix {
        for package in options.packages {
            args.extend(["-p".into(), package.clone()]);
        }
        args.push("--allow-dirty".into());
    }
    args
}

/// Cargo fix only edits selected packages. Cargo metadata's `resolve` graph
/// includes inactive optional workspace dependencies, so use Cargo's actual
/// compilation unit graph and compare the fix feature resolution before
/// allowing a source edit.
fn configured_fix_selection(
    context: &BuildContext,
    toolchain: &str,
    features: &[String],
) -> Result<FixSelection> {
    let mut command = Command::new("cargo");
    command
        .current_dir(&context.workspace_root)
        .arg(format!("+{toolchain}"))
        .args(["metadata", "--format-version", "1", "--filter-platform"])
        .arg(context.config.target())
        .arg("--manifest-path")
        .arg(&context.app_manifest);
    if !features.is_empty() {
        command.arg("--features").arg(features.join(","));
    }
    for name in SANITIZED_ENV {
        command.env_remove(name);
    }
    let output = command.output().map_err(|source| Error::Spawn {
        program: "cargo metadata".into(),
        source,
    })?;
    if !output.status.success() {
        return Err(Error::Message(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let metadata: CargoMetadata = serde_json::from_slice(&output.stdout)
        .map_err(|error| Error::Message(format!("invalid cargo metadata: {error}")))?;
    let root = application_package(&metadata, &context.app_manifest)?;
    let graph = cargo_unit_graph(context, toolchain, features, None)?;
    let selection = select_fix_packages(&metadata, &graph, &root.id, &context.workspace_root)?;
    let fix_features = fix_feature_flags(features, &root.name, selection.has_root_default);
    let fix_graph = cargo_unit_graph(context, toolchain, &fix_features, Some(&selection.packages))?;
    if package_features(&graph) != package_features(&fix_graph) {
        return Err(Error::Message(
            "dylint --fix would resolve different package features than the configured scan".into(),
        ));
    }
    Ok(FixSelection {
        packages: selection.packages,
        features: fix_features,
    })
}

fn application_package<'a>(
    metadata: &'a CargoMetadata,
    manifest: &Path,
) -> Result<&'a CargoPackage> {
    let canonical_manifest = manifest.canonicalize().with_path(manifest)?;
    metadata
        .packages
        .iter()
        .find(|package| package.manifest_path == canonical_manifest)
        .ok_or_else(|| Error::Message("configured application missing from cargo metadata".into()))
}

fn fix_feature_flags(base: &[String], root_name: &str, has_root_default: bool) -> Vec<String> {
    let mut features = base.to_vec();
    if has_root_default {
        features.push(format!("{root_name}/default"));
    }
    features.sort();
    features.dedup();
    features
}

struct SelectedPackages {
    packages: Vec<String>,
    has_root_default: bool,
}

fn select_fix_packages(
    metadata: &CargoMetadata,
    graph: &UnitGraph,
    root_id: &str,
    workspace_root: &Path,
) -> Result<SelectedPackages> {
    // Select by package id: a local package may share its name with a
    // registry dependency (for example `acpi`), which makes `-p <name>`
    // ambiguous for Cargo.
    let local: HashSet<_> = metadata
        .packages
        .iter()
        .filter(|package| {
            package.source.is_none() && package.manifest_path.starts_with(workspace_root)
        })
        .map(|package| package.id.as_str())
        .collect();
    let mut stack: Vec<_> = graph
        .roots
        .iter()
        .copied()
        .filter(|&index| {
            graph
                .units
                .get(index)
                .is_some_and(|unit| unit.pkg_id == root_id)
        })
        .collect();
    if stack.is_empty() {
        return Err(Error::Message(
            "configured application missing from Cargo unit graph".into(),
        ));
    }
    let has_root_default = stack
        .iter()
        .any(|&index| graph.units[index].features.iter().any(|f| f == "default"));
    let mut visited = HashSet::new();
    let mut selected = Vec::new();
    while let Some(index) = stack.pop() {
        if !visited.insert(index) {
            continue;
        }
        let unit = graph.units.get(index).ok_or_else(|| {
            Error::Message(format!("invalid Cargo unit graph dependency index {index}"))
        })?;
        if local.contains(unit.pkg_id.as_str()) {
            selected.push(unit.pkg_id.clone());
        }
        stack.extend(unit.dependencies.iter().map(|dependency| dependency.index));
    }
    selected.sort();
    selected.dedup();
    Ok(SelectedPackages {
        packages: selected,
        has_root_default,
    })
}

fn package_features(graph: &UnitGraph) -> BTreeMap<&str, BTreeSet<&str>> {
    let mut features = BTreeMap::<_, BTreeSet<_>>::new();
    for unit in &graph.units {
        features
            .entry(unit.pkg_id.as_str())
            .or_default()
            .extend(unit.features.iter().map(String::as_str));
    }
    features
}

fn cargo_unit_graph(
    context: &BuildContext,
    toolchain: &str,
    features: &[String],
    packages: Option<&[String]>,
) -> Result<UnitGraph> {
    let mut command = Command::new("cargo");
    command.current_dir(&context.workspace_root).args([
        format!("+{toolchain}"),
        "check".into(),
        "-Z".into(),
        "unstable-options".into(),
        "--unit-graph".into(),
        "--manifest-path".into(),
        context.app_manifest.display().to_string(),
        "--target".into(),
        context.config.target().into(),
    ]);
    if context.config.profile() == BuildProfile::Release {
        command.arg("--release");
    }
    if let Some(packages) = packages {
        command.arg("--no-default-features");
        for package in packages {
            command.arg("-p").arg(package);
        }
    }
    if !features.is_empty() {
        command.arg("--features").arg(features.join(","));
    }
    for name in SANITIZED_ENV {
        command.env_remove(name);
    }
    let output = command.output().map_err(|source| Error::Spawn {
        program: "cargo unit graph".into(),
        source,
    })?;
    if !output.status.success() {
        return Err(Error::Message(format!(
            "cargo unit graph failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| Error::Message(format!("invalid Cargo unit graph: {error}")))
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

/// Builds the lint library in the same environment for every kernel
/// configuration.
///
/// Cargo running in `tools/lints` still reads the repository's
/// `.cargo/config.toml`, which includes the generated `.xconfig.toml` and
/// its `[build] rustflags` (`--cfg k_plat_name=...`, `--cfg unittest`). The
/// library is a host tool that uses none of them, but a different set
/// would relink it, and Dylint records the library as a file dependency of
/// every crate it checks, so every scan would re-check every crate. An
/// empty `CARGO_ENCODED_RUSTFLAGS` takes precedence over all configured
/// rustflags; the kernel crates still get theirs from the scan.
///
/// `CARGO_TARGET_DIR` also reaches the library build that `dylint_testing`
/// runs inside `make dylint_test`, so that build lands in the same target
/// directory instead of `tools/lints/target`.
fn pin_lint_build_env(process: &mut Process, target_dir: &Path) {
    process
        .env("CARGO_ENCODED_RUSTFLAGS", "")
        .env("CARGO_TARGET_DIR", target_dir);
}

fn set_host_tools(process: &mut Process, host: &str) {
    let host = host.replace('-', "_");
    for (tool, command) in [("CC", "cc"), ("AR", "ar"), ("RANLIB", "ranlib")] {
        process.env(format!("{tool}_{host}"), command);
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{
        DRY_RUN_HOST,
        CargoMetadata, CargoPackage, CargoUnit, CargoUnitDependency, ScanOptions, UnitGraph,
        application_package, fix_feature_flags, lint_library_path, package_features, parse_host,
        parse_toolchain, probe_host, scan_args, select_fix_packages,
    };

    fn unit(id: &str, features: &[&str], dependencies: &[usize]) -> CargoUnit {
        CargoUnit {
            pkg_id: id.into(),
            features: features.iter().map(|feature| (*feature).into()).collect(),
            dependencies: dependencies
                .iter()
                .map(|&index| CargoUnitDependency { index })
                .collect(),
        }
    }

    #[test]
    fn selects_only_compiled_local_packages_and_build_dependencies() {
        let workspace = Path::new("/workspace");
        let package = |id: &str, name: &str| CargoPackage {
            id: id.into(),
            name: name.into(),
            source: None,
            manifest_path: PathBuf::from(format!("/workspace/{name}/Cargo.toml")),
        };
        let metadata = CargoMetadata {
            packages: vec![
                package("entry", "entry"),
                package("kernel", "kernel"),
                package("build", "build-helper"),
                package("other-platform", "kplat-x86_64"),
                package("dev", "dev-helper"),
                CargoPackage {
                    id: "external".into(),
                    name: "external".into(),
                    source: Some("registry".into()),
                    manifest_path: PathBuf::from("/registry/external/Cargo.toml"),
                },
            ],
        };
        // The inactive platform and dev units exist in metadata but are not
        // reachable from the application compilation roots.
        let graph = UnitGraph {
            roots: vec![0],
            units: vec![
                unit("entry", &["default"], &[1, 2, 5]),
                unit("kernel", &["console"], &[]),
                unit("build", &[], &[]),
                unit("other-platform", &[], &[]),
                unit("dev", &[], &[]),
                unit("external", &[], &[]),
            ],
        };
        let selection = select_fix_packages(&metadata, &graph, "entry", workspace).unwrap();
        assert_eq!(selection.packages, ["build", "entry", "kernel"]);
        assert!(selection.has_root_default);
        assert_eq!(
            fix_feature_flags(
                &["kfeat/console".into()],
                "entry",
                selection.has_root_default
            ),
            ["entry/default", "kfeat/console"]
        );
        let mut changed_features = graph.clone();
        changed_features.units[1].features.push("x86".into());
        assert_ne!(
            package_features(&graph),
            package_features(&changed_features)
        );
    }

    #[test]
    fn resolves_dot_prefixed_application_manifest() {
        let directory = tempfile::tempdir().unwrap();
        let entry = directory.path().join("entry");
        std::fs::create_dir(&entry).unwrap();
        std::fs::write(entry.join("Cargo.toml"), "[package]\nname = \"entry\"\n").unwrap();
        let manifest = entry.join("Cargo.toml").canonicalize().unwrap();
        let metadata = CargoMetadata {
            packages: vec![CargoPackage {
                id: "entry-id".into(),
                name: "entry".into(),
                source: None,
                manifest_path: manifest,
            }],
        };
        let supplied = directory.path().join("./entry/Cargo.toml");
        assert_eq!(
            application_package(&metadata, &supplied).unwrap().id,
            "entry-id"
        );
    }

    #[test]
    fn fix_selects_local_packages_with_kernel_configuration() {
        let args = scan_args(ScanOptions {
            toolchain: "nightly-2026-03-08",
            library: Path::new("liblints.so"),
            manifest: Path::new("entry/Cargo.toml"),
            target: "aarch64-unknown-none-softfloat",
            release: true,
            features: &["foo/bar".into()],
            fix: true,
            packages: &["entry".into(), "kfs".into()],
        });
        assert_eq!(
            args,
            [
                "+nightly-2026-03-08",
                "dylint",
                "--no-metadata",
                "--lib-path",
                "liblints.so",
                "--manifest-path",
                "entry/Cargo.toml",
                "--fix",
                "--",
                "--target",
                "aarch64-unknown-none-softfloat",
                "--no-default-features",
                "--release",
                "--features",
                "foo/bar",
                "-p",
                "entry",
                "-p",
                "kfs",
                "--allow-dirty"
            ]
        );
    }

    #[test]
    fn scan_keeps_entry_as_implicit_package() {
        let args = scan_args(ScanOptions {
            toolchain: "nightly-2026-03-08",
            library: Path::new("liblints.so"),
            manifest: Path::new("entry/Cargo.toml"),
            target: "x86_64-unknown-none",
            release: false,
            features: &[],
            fix: false,
            packages: &[],
        });
        assert!(!args.contains(&"--fix".into()));
        assert!(!args.contains(&"-p".into()));
        assert!(!args.contains(&"--allow-dirty".into()));
        assert_eq!(args.last().unwrap(), "x86_64-unknown-none");
    }

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

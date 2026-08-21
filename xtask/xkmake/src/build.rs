// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use xconfig::build_config::{BuildProfile, KernelArch, generate_kernel_build_files};

use crate::{
    cli::BuildArgs,
    context::BuildContext,
    error::{Error, IoResultExt, Result},
    image_metadata::{self, BuildInfo, BuildInfoRequest},
    process::Process,
    x86::X86BootInputs,
};

pub(crate) struct Bundle {
    pub(crate) directory: std::path::PathBuf,
    pub(crate) boot_artifacts: BootArtifacts,
    pub(crate) context: BuildContext,
}

pub(crate) enum BootArtifacts {
    Direct {
        kernel_bin: PathBuf,
    },
    X86 {
        linuxboot_image: PathBuf,
        uefi_image: PathBuf,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RootArtifactKind {
    Elf,
    DebugElf,
    Bin,
    Linuxboot,
    Uefi,
}

const DIRECT_ROOT_ARTIFACTS: &[RootArtifactKind] = &[
    RootArtifactKind::Elf,
    RootArtifactKind::DebugElf,
    RootArtifactKind::Bin,
];
const X86_ROOT_ARTIFACTS: &[RootArtifactKind] = &[
    RootArtifactKind::Elf,
    RootArtifactKind::DebugElf,
    RootArtifactKind::Bin,
    RootArtifactKind::Linuxboot,
    RootArtifactKind::Uefi,
];
const BUNDLE_FORMAT_VERSION: u32 = 6;

impl RootArtifactKind {
    fn source(self, context: &BuildContext) -> &std::path::Path {
        match self {
            Self::Elf => &context.bundle_elf,
            Self::DebugElf => &context.bundle_debug_elf,
            Self::Bin => &context.bundle_bin,
            Self::Linuxboot => &context.bundle_linuxboot,
            Self::Uefi => &context.bundle_uefi,
        }
    }

    const fn extension(self) -> &'static str {
        match self {
            Self::Elf => "elf",
            Self::DebugElf => "debug.elf",
            Self::Bin => "bin",
            Self::Linuxboot => "bzimg",
            Self::Uefi => "uefi.img",
        }
    }
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
struct BundleManifest {
    format_version: u32,
    package: String,
    guest_ip: String,
    gateway: String,
    unittest: bool,
    unittest_crate: Option<String>,
    build_info: BuildInfo,
    build_id: String,
    kernel_elf: BundleArtifact,
    kernel_debug_elf: BundleArtifact,
    kernel_image: BundleArtifact,
    linuxboot_image: Option<BundleArtifact>,
    uefi_image: Option<BundleArtifact>,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
struct BundleArtifact {
    path: String,
    size: u64,
}

struct PendingBundle {
    kernel_elf: PathBuf,
    kernel_debug_elf: PathBuf,
    kernel_bin: PathBuf,
    linuxboot_image: Option<PathBuf>,
    uefi_image: Option<PathBuf>,
    manifest: PathBuf,
}

struct PendingRootArtifact {
    path: PathBuf,
}

impl PendingRootArtifact {
    fn create(destination: &std::path::Path) -> Self {
        let suffix = std::process::id();
        let extension = destination
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("artifact");
        Self {
            path: destination.with_extension(format!("{extension}.tmp.{suffix}")),
        }
    }

    fn commit(self, destination: &std::path::Path) -> Result<()> {
        fs::rename(&self.path, destination).with_path(destination)
    }
}

impl Drop for PendingRootArtifact {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl PendingBundle {
    fn create(context: &BuildContext) -> Self {
        let suffix = std::process::id();
        Self {
            kernel_elf: context.bundle_dir.join(format!("kernel.elf.tmp.{suffix}")),
            kernel_debug_elf: context
                .bundle_dir
                .join(format!("kernel.debug.elf.tmp.{suffix}")),
            kernel_bin: context.bundle_dir.join(format!("kernel.bin.tmp.{suffix}")),
            linuxboot_image: (context.config.arch() == KernelArch::X86_64).then(|| {
                context
                    .bundle_dir
                    .join(format!("kernel.bzimg.tmp.{suffix}"))
            }),
            uefi_image: (context.config.arch() == KernelArch::X86_64).then(|| {
                context
                    .bundle_dir
                    .join(format!("kernel.uefi.img.tmp.{suffix}"))
            }),
            manifest: context.bundle_dir.join(format!("bundle.toml.tmp.{suffix}")),
        }
    }

    fn commit(self, context: &BuildContext) -> Result<()> {
        let manifest_path = context.bundle_dir.join("bundle.toml");
        remove_if_present(&manifest_path)?;
        remove_if_present(&context.bundle_elf)?;
        remove_if_present(&context.bundle_debug_elf)?;
        remove_if_present(&context.bundle_bin)?;
        fs::rename(&self.kernel_elf, &context.bundle_elf).with_path(&context.bundle_elf)?;
        fs::rename(&self.kernel_debug_elf, &context.bundle_debug_elf)
            .with_path(&context.bundle_debug_elf)?;
        fs::rename(&self.kernel_bin, &context.bundle_bin).with_path(&context.bundle_bin)?;
        if let Some(linuxboot_image) = &self.linuxboot_image {
            remove_if_present(&context.bundle_linuxboot)?;
            fs::rename(linuxboot_image, &context.bundle_linuxboot)
                .with_path(&context.bundle_linuxboot)?;
        }
        if let Some(uefi_image) = &self.uefi_image {
            remove_if_present(&context.bundle_uefi)?;
            fs::rename(uefi_image, &context.bundle_uefi).with_path(&context.bundle_uefi)?;
        }
        fs::rename(&self.manifest, &manifest_path).with_path(&manifest_path)
    }
}

impl Drop for PendingBundle {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.kernel_elf);
        let _ = fs::remove_file(&self.kernel_debug_elf);
        let _ = fs::remove_file(&self.kernel_bin);
        if let Some(path) = &self.linuxboot_image {
            let _ = fs::remove_file(path);
        }
        if let Some(path) = &self.uefi_image {
            let _ = fs::remove_file(path);
        }
        let _ = fs::remove_file(&self.manifest);
    }
}

pub(crate) fn build(args: &BuildArgs) -> Result<Bundle> {
    let context = BuildContext::create(args)?;
    generate_build_configuration(&context)?;
    run_cargo(&context, CargoAction::Build)?;
    let x86_inputs = if context.config.arch() == KernelArch::X86_64 {
        Some(X86BootInputs::prepare(&context)?)
    } else {
        None
    };
    let build_info = BuildInfoRequest::collect(&context, config_hash(&context))?;

    // The symbol table is generated from the first link and embedded into
    // the kernel on the second link (see `symtab::generate`). The layout is
    // stable across relinks because the blob lives in `.rodata` and the
    // table only covers `.text` symbols, which precede `.rodata` in the
    // linker script — so this converges after at most two iterations.
    for _ in 0..2 {
        let symtab_changed = create_bundle(&context, x86_inputs.as_ref(), &build_info)?;
        if !symtab_changed {
            break;
        }
        if context.verbosity > 0 {
            println!("Symbol table updated; relinking kernel");
        }
        run_cargo(&context, CargoAction::Build)?;
    }

    publish_root_artifacts(&context)?;

    Ok(bundle_from_context(context))
}

pub(crate) fn clippy(args: &BuildArgs) -> Result<()> {
    let context = BuildContext::create(args)?;
    generate_build_configuration(&context)?;
    run_cargo(&context, CargoAction::Clippy)
}

pub(crate) fn existing_bundle(args: &BuildArgs) -> Result<Bundle> {
    let context = BuildContext::create(args)?;
    // `--no-build` is an explicit instruction to run the existing bundle as-is,
    // so do not gate on manifest/provenance/mtime compatibility — that made a
    // bundle built in one CI stage unusable in another (e.g. a clean build
    // stage vs a test stage whose `disk.img`/`*-output.log` make the tree
    // dirty, flipping git_dirty). Only verify the kernel artifact exists, so
    // we fail with a clear message instead of a confusing QEMU error when
    // nothing has been built yet.
    if !context.bundle_elf.is_file() {
        return Err(crate::error::Error::Message(format!(
            "no bundle exists at {}; run `make build` first",
            context.bundle_dir.display()
        )));
    }
    Ok(bundle_from_context(context))
}

fn bundle_from_context(context: BuildContext) -> Bundle {
    let boot_artifacts = match context.config.arch() {
        KernelArch::X86_64 => BootArtifacts::X86 {
            linuxboot_image: context.bundle_linuxboot.clone(),
            uefi_image: context.bundle_uefi.clone(),
        },
        _ => BootArtifacts::Direct {
            kernel_bin: context.bundle_bin.clone(),
        },
    };
    Bundle {
        directory: context.bundle_dir.clone(),
        boot_artifacts,
        context,
    }
}

#[derive(Clone, Copy)]
enum CargoAction {
    Build,
    Clippy,
}

fn generate_build_configuration(context: &BuildContext) -> Result<()> {
    if context.dry_run {
        return Ok(());
    }

    generate_kernel_build_files(&context.config, &context.build_files())?;
    crate::linker::generate(&context.config, &context.linker_script, context.verbosity)?;
    Ok(())
}

fn run_cargo(context: &BuildContext, action: CargoAction) -> Result<()> {
    let feature_names = context
        .config
        .cargo_features()
        .iter()
        .map(|feature| format!("kfeat/{feature}"))
        .chain(context.unittest.then(|| "unittest".to_string()))
        .collect::<Vec<_>>();

    let mut command = Process::new("cargo", context.dry_run, context.verbosity);
    command
        .current_dir(&context.workspace_root)
        .arg(match action {
            CargoAction::Build => "build",
            CargoAction::Clippy => "clippy",
        })
        .arg("--manifest-path")
        .arg(&context.app_manifest)
        .arg("--target")
        .arg(context.config.target())
        .arg("--target-dir")
        .arg(&context.target_dir)
        .env("RUSTC_BOOTSTRAP", "1")
        .env("TARGET_DIR", &context.target_dir)
        .env("K_ARCH", context.config.arch().as_str())
        .env("K_TARGET", context.config.target())
        .env("K_PLAT_NAME", context.config.platform())
        .env("K_MODE", context.config.profile().as_str())
        .env("K_IP", context.guest_ip.to_string())
        .env("K_GW", context.gateway.to_string())
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env(
            "CC",
            format!("{}gcc", context.config.arch().cross_compile_prefix()),
        )
        .env(
            "AR",
            format!("{}ar", context.config.arch().cross_compile_prefix()),
        )
        .env(
            "RANLIB",
            format!("{}ranlib", context.config.arch().cross_compile_prefix()),
        );

    if context.config.profile() == BuildProfile::Release {
        command.arg("--release");
    }
    if !feature_names.is_empty() {
        command.arg("--features").arg(feature_names.join(","));
    }
    match context.verbosity {
        0 => {}
        1 => {
            command.arg("-v");
        }
        _ => {
            command.arg("-vv");
        }
    }
    if context.config.arch() == KernelArch::X86_64 {
        command.env("CFLAGS_x86_64_unknown_none", "-mcmodel=large -mno-red-zone");
    }
    if let Some(unittest_crate) = &context.unittest_crate {
        command.env("UNITTEST_CRATE", unittest_crate);
    } else {
        command.env_remove("UNITTEST_CRATE");
    }
    if matches!(action, CargoAction::Clippy) {
        command.arg("--").args([
            "-A",
            "unsafe_op_in_unsafe_fn",
            "-D",
            "clippy::undocumented_unsafe_blocks",
            "-D",
            "warnings",
            "--cfg",
            "unittest",
        ]);
    }
    command.run()
}

fn create_bundle(
    context: &BuildContext,
    x86_inputs: Option<&X86BootInputs>,
    build_info: &BuildInfoRequest,
) -> Result<bool> {
    if context.dry_run {
        return Ok(false);
    }

    // Reuse check first: `can_reuse_bundle` compares the Cargo ELF against
    // the committed bundle, and an unchanged `cargo_elf` implies an unchanged
    // symbol blob — so a cache hit must not pay for reading and parsing the
    // (hundreds of MB) debug ELF just to regenerate the table.
    if can_reuse_bundle(context, x86_inputs, build_info)? {
        if context.verbosity > 0 {
            println!("Reusing {}", context.bundle_dir.display());
        }
        return Ok(false);
    }

    // Regenerate the symbol table from the current ELF; `true` means the
    // blob changed and the kernel must be relinked to embed it.
    let symtab_changed = if context.config.is_enabled("KFEAT_SYMTAB") {
        crate::symtab::generate(context)?
    } else {
        false
    };

    fs::create_dir_all(&context.bundle_dir).with_path(&context.bundle_dir)?;
    let pending = PendingBundle::create(context);
    // `kernel.debug.elf` is the pristine unstripped Cargo artifact and the
    // symbolication input for `xkmake symbolize`; `kernel.elf` is the boot
    // ELF, which may receive DWARF injection and build metadata.
    fs::copy(&context.cargo_elf, &pending.kernel_elf).with_path(&context.cargo_elf)?;
    fs::copy(&context.cargo_elf, &pending.kernel_debug_elf).with_path(&context.cargo_elf)?;

    if context.config.is_enabled("KFEAT_DWARF") {
        embed_dwarf(context, &pending.kernel_elf)?;
    }
    let build_info = build_info.materialize()?;
    let build_id = image_metadata::finalize(&pending.kernel_elf, &build_info)?;
    create_raw_image(context, &pending.kernel_elf, &pending.kernel_bin)?;
    if let Some(inputs) = x86_inputs {
        let linuxboot_image = pending.linuxboot_image.as_deref().ok_or_else(|| {
            Error::Message("x86 pending bundle is missing its LinuxBoot path".to_string())
        })?;
        let uefi_image = pending.uefi_image.as_deref().ok_or_else(|| {
            Error::Message("x86 pending bundle is missing its UEFI path".to_string())
        })?;
        inputs.create_linuxboot_image(context, &pending.kernel_elf, linuxboot_image)?;
        inputs.create_uefi_image(context, &pending.kernel_elf, uefi_image)?;
    }
    write_manifest(context, &build_info, build_id, &pending)?;
    pending.commit(context)?;
    Ok(symtab_changed)
}

fn publish_root_artifacts(context: &BuildContext) -> Result<()> {
    if context.dry_run {
        return Ok(());
    }

    let stem = format!(
        "{}-{}",
        context.config.arch().as_str(),
        context.config.machine()
    );
    for artifact in root_artifact_kinds(context.config.arch()) {
        let source = artifact.source(context);
        let destination =
            context
                .workspace_root
                .join(format!("xkernel_{}.{}", stem, artifact.extension()));
        if copy_is_current(source, &destination)? {
            continue;
        }
        if context.verbosity > 0 {
            println!("+ copy {} -> {}", source.display(), destination.display());
        }
        let pending = PendingRootArtifact::create(&destination);
        remove_if_present(&pending.path)?;
        fs::copy(source, &pending.path).with_path(&pending.path)?;
        pending.commit(&destination)?;
    }

    Ok(())
}

const fn root_artifact_kinds(arch: KernelArch) -> &'static [RootArtifactKind] {
    match arch {
        KernelArch::X86_64 => X86_ROOT_ARTIFACTS,
        _ => DIRECT_ROOT_ARTIFACTS,
    }
}

fn embed_dwarf(context: &BuildContext, kernel_elf: &std::path::Path) -> Result<()> {
    if context.verbosity > 0 {
        println!("+ embed DWARF sections into {}", kernel_elf.display());
    }
    crate::dwarf_embed::embed_dwarf(kernel_elf)
}

fn create_raw_image(
    context: &BuildContext,
    kernel_elf: &std::path::Path,
    kernel_bin: &std::path::Path,
) -> Result<()> {
    let mut command = Process::new("rust-objcopy", false, context.verbosity);
    command
        .current_dir(&context.workspace_root)
        .arg(format!(
            "--binary-architecture={}",
            context.config.arch().as_str()
        ))
        .arg(kernel_elf)
        .arg("--strip-all")
        .arg("-O")
        .arg("binary")
        .arg(kernel_bin);
    command.run()?;

    let image_size = fs::metadata(kernel_bin).with_path(kernel_bin)?.len();
    if image_size == 0 {
        return Err(crate::error::Error::Message(format!(
            "generated kernel image is empty: {}",
            kernel_bin.display()
        )));
    }
    Ok(())
}

fn write_manifest(
    context: &BuildContext,
    build_info: &BuildInfo,
    build_id: String,
    pending: &PendingBundle,
) -> Result<()> {
    let manifest = bundle_manifest(context, build_info, build_id, pending)?;
    let content = toml::to_string_pretty(&manifest)?;
    fs::write(&pending.manifest, content).with_path(&pending.manifest)
}

fn bundle_manifest(
    context: &BuildContext,
    build_info: &BuildInfo,
    build_id: String,
    pending: &PendingBundle,
) -> Result<BundleManifest> {
    Ok(BundleManifest {
        format_version: BUNDLE_FORMAT_VERSION,
        package: context.package_name.clone(),
        guest_ip: context.guest_ip.to_string(),
        gateway: context.gateway.to_string(),
        unittest: context.unittest,
        unittest_crate: context.unittest_crate.clone(),
        build_info: build_info.clone(),
        build_id,
        kernel_elf: BundleArtifact::collect("kernel.elf", &pending.kernel_elf)?,
        kernel_debug_elf: BundleArtifact::collect("kernel.debug.elf", &pending.kernel_debug_elf)?,
        kernel_image: BundleArtifact::collect("kernel.bin", &pending.kernel_bin)?,
        linuxboot_image: pending
            .linuxboot_image
            .as_deref()
            .map(|path| BundleArtifact::collect("kernel.bzimg", path))
            .transpose()?,
        uefi_image: pending
            .uefi_image
            .as_deref()
            .map(|path| BundleArtifact::collect("kernel.uefi.img", path))
            .transpose()?,
    })
}

fn can_reuse_bundle(
    context: &BuildContext,
    x86_inputs: Option<&X86BootInputs>,
    build_info: &BuildInfoRequest,
) -> Result<bool> {
    let manifest_path = context.bundle_dir.join("bundle.toml");
    let Ok(content) = fs::read_to_string(&manifest_path) else {
        return Ok(false);
    };
    let Ok(manifest) = toml::from_str::<BundleManifest>(&content) else {
        return Ok(false);
    };
    let metadata_matches = manifest.format_version == BUNDLE_FORMAT_VERSION
        && manifest.package == context.package_name
        && manifest.guest_ip == context.guest_ip.to_string()
        && manifest.gateway == context.gateway.to_string()
        && manifest.unittest == context.unittest
        && manifest.unittest_crate == context.unittest_crate
        && build_info.matches(&manifest.build_info)
        && manifest.kernel_elf.path == "kernel.elf"
        && manifest.kernel_debug_elf.path == "kernel.debug.elf"
        && manifest.kernel_image.path == "kernel.bin"
        && manifest
            .linuxboot_image
            .as_ref()
            .map(|artifact| artifact.path.as_str())
            == (context.config.arch() == KernelArch::X86_64).then_some("kernel.bzimg")
        && manifest
            .uefi_image
            .as_ref()
            .map(|artifact| artifact.path.as_str())
            == (context.config.arch() == KernelArch::X86_64).then_some("kernel.uefi.img");
    if !metadata_matches && context.verbosity > 0 {
        eprintln!("bundle manifest does not match the current build inputs: {manifest:?}");
    }
    if !metadata_matches {
        return Ok(false);
    }

    let mut latest_input_modified = fs::metadata(&context.cargo_elf)
        .with_path(&context.cargo_elf)?
        .modified()
        .with_path(&context.cargo_elf)?;
    if let Some(inputs) = x86_inputs {
        let Some(x86_modified) = inputs.latest_modified(context)? else {
            return Ok(false);
        };
        latest_input_modified = latest_input_modified.max(x86_modified);
    }
    let bundle_modified = fs::metadata(&manifest_path)
        .with_path(&manifest_path)?
        .modified()
        .with_path(&manifest_path)?;
    if latest_input_modified > bundle_modified
        || !manifest
            .kernel_elf
            .matches(&context.bundle_elf, bundle_modified)?
        || !manifest
            .kernel_debug_elf
            .matches(&context.bundle_debug_elf, bundle_modified)?
        || !manifest
            .kernel_image
            .matches(&context.bundle_bin, bundle_modified)?
        || !optional_artifact_matches(
            manifest.linuxboot_image.as_ref(),
            (context.config.arch() == KernelArch::X86_64)
                .then_some(context.bundle_linuxboot.as_path()),
            bundle_modified,
        )?
        || !optional_artifact_matches(
            manifest.uefi_image.as_ref(),
            (context.config.arch() == KernelArch::X86_64).then_some(context.bundle_uefi.as_path()),
            bundle_modified,
        )?
    {
        return Ok(false);
    }

    Ok(true)
}

impl BundleArtifact {
    fn collect(path: &str, source: &Path) -> Result<Self> {
        Ok(Self {
            path: path.to_string(),
            size: fs::metadata(source).with_path(source)?.len(),
        })
    }

    fn matches(&self, path: &Path, committed_at: SystemTime) -> Result<bool> {
        let Some(metadata) = file_metadata(path)? else {
            return Ok(false);
        };
        let modified = metadata.modified().with_path(path)?;
        Ok(metadata.is_file() && metadata.len() == self.size && modified <= committed_at)
    }
}

fn optional_artifact_matches(
    artifact: Option<&BundleArtifact>,
    path: Option<&Path>,
    committed_at: SystemTime,
) -> Result<bool> {
    match (artifact, path) {
        (Some(artifact), Some(path)) => artifact.matches(path, committed_at),
        (None, None) => Ok(true),
        _ => Ok(false),
    }
}

fn copy_is_current(source: &Path, destination: &Path) -> Result<bool> {
    let source_metadata = fs::metadata(source).with_path(source)?;
    let Some(destination_metadata) = file_metadata(destination)? else {
        return Ok(false);
    };
    Ok(destination_metadata.is_file()
        && destination_metadata.len() == source_metadata.len()
        && destination_metadata.modified().with_path(destination)?
            >= source_metadata.modified().with_path(source)?)
}

fn file_metadata(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn remove_if_present(path: &std::path::Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(crate::error::Error::Io {
            path: path.to_path_buf(),
            source: error,
        }),
    }
}

fn config_hash(context: &BuildContext) -> String {
    let mut hasher = Sha256::new();
    for (name, value) in context.config.values() {
        hasher.update(name.as_bytes());
        hasher.update([0]);
        hasher.update(value.as_bytes());
        hasher.update([0]);
    }
    hasher.update([context.unittest as u8]);
    if let Some(unittest_crate) = &context.unittest_crate {
        hasher.update(unittest_crate.as_bytes());
    }
    hasher.update([0]);
    hasher.update(context.guest_ip.octets());
    hasher.update(context.gateway.octets());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_platforms_publish_elf_debug_elf_and_bin() {
        assert_eq!(
            root_artifact_kinds(KernelArch::Riscv64),
            [
                RootArtifactKind::Elf,
                RootArtifactKind::DebugElf,
                RootArtifactKind::Bin,
            ]
        );
    }

    #[test]
    fn x86_publishes_both_boot_media() {
        assert_eq!(
            root_artifact_kinds(KernelArch::X86_64),
            [
                RootArtifactKind::Elf,
                RootArtifactKind::DebugElf,
                RootArtifactKind::Bin,
                RootArtifactKind::Linuxboot,
                RootArtifactKind::Uefi,
            ]
        );
    }

    #[test]
    fn bundle_artifact_rejects_size_changes() {
        let directory = tempfile::tempdir().expect("temporary directory must be created");
        let path = directory.path().join("kernel.elf");
        fs::write(&path, b"kernel").expect("test artifact must be written");
        let artifact = BundleArtifact::collect("kernel.elf", &path)
            .expect("test artifact metadata must be collected");
        let committed_at = SystemTime::now();

        assert!(artifact.matches(&path, committed_at).unwrap());
        fs::write(&path, b"changed kernel").expect("test artifact must be changed");
        assert!(!artifact.matches(&path, SystemTime::now()).unwrap());
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{
    autostart,
    cli::{InstallCommand, ListCommand, PrepareCommand},
    debugfs::{self, DebugfsScript},
    install,
    manifest::{self, Uapp},
    prepare::{self, PrepareContext},
};

pub fn list(command: ListCommand) -> Result<(), String> {
    let uapps = load_selected_uapps(&command.uapps_dir, &command.select)?;
    for uapp in &uapps {
        let description = uapp.manifest.package.description.as_deref().unwrap_or("");
        println!(
            "{}\torder={}\tenabled={}\t{}",
            uapp.name(),
            uapp.order(),
            uapp.is_enabled(),
            description
        );
    }
    Ok(())
}

pub fn prepare(command: PrepareCommand) -> Result<(), String> {
    let uapps = load_selected_uapps(&command.uapps_dir, &command.select)?;
    let context = PrepareContext::for_prepare(
        absolute_path(command.repo_root)?,
        absolute_path(command.disk_img)?,
        absolute_path(command.build_dir)?,
    );
    prepare::run_prepare_commands(&uapps, &context)
}

pub fn install(command: InstallCommand) -> Result<(), String> {
    if !command.dry_run && !command.disk_img.is_file() {
        return Err(format!(
            "disk image not found: {}",
            command.disk_img.display()
        ));
    }
    validate_autostart_target(&command.autostart_target)?;
    if !command.dry_run {
        let autostart_parent = debugfs::guest_parent(&command.autostart_target);
        debugfs::verify_directory(&command.disk_img, &autostart_parent).map_err(|err| {
            format!(
                "autostart parent directory must already exist in disk image: {autostart_parent}: \
                 {err}"
            )
        })?;
    }

    let uapps = load_selected_uapps(&command.uapps_dir, &command.select)?;
    let context = PrepareContext {
        repo_root: absolute_path(command.repo_root)?,
        disk_img: absolute_path(command.disk_img.clone())?,
        build_dir: absolute_path(command.build_dir)?,
        arch: command.arch,
        target: command.target,
        plat_name: command.plat_name,
        cross_compile: command.cross_compile,
    };
    prepare::run_prepare_commands(&uapps, &context)?;

    let install_files = install::collect_install_files(&uapps)?;
    let autostart_content = autostart::render(&uapps);
    let autostart_host_path = install::write_autostart_file(&autostart_content)?;

    let mut script = DebugfsScript::new();
    for file in &install_files {
        script.add_file(file);
    }
    install::add_autostart(
        &mut script,
        autostart_host_path.clone(),
        &command.autostart_target,
    );
    let debugfs_script_path = install::write_debugfs_script(&script)?;

    if command.dry_run {
        println!(
            "dry-run: debugfs command file written to {}",
            debugfs_script_path.display()
        );
        println!(
            "dry-run: autostart script written to {}",
            autostart_host_path.display()
        );
    } else {
        debugfs::run_debugfs(&command.disk_img, &debugfs_script_path)?;
        let mut verified_paths: Vec<String> = install_files
            .iter()
            .map(|file| file.guest_path.clone())
            .collect();
        verified_paths.push(command.autostart_target.clone());
        debugfs::verify_paths(&command.disk_img, &verified_paths)?;
        cleanup_temp_file(&autostart_host_path);
        cleanup_temp_file(&debugfs_script_path);
    }

    print_summary(
        &command.disk_img,
        &uapps,
        &install_files,
        autostart::count_entries(&uapps),
        script.command_count(),
        command.dry_run,
    );
    Ok(())
}

fn load_selected_uapps(uapps_dir: &Path, selection: &str) -> Result<Vec<Uapp>, String> {
    let uapps = manifest::discover(uapps_dir)?;
    manifest::select_uapps(uapps, selection)
}

fn validate_autostart_target(path: &str) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err("autostart target must not be empty".to_string());
    }
    if !path.starts_with('/') {
        return Err(format!(
            "autostart target must be an absolute guest path: {path}"
        ));
    }
    if path.contains('\0') {
        return Err("autostart target must not contain NUL bytes".to_string());
    }
    Ok(())
}

fn absolute_path(path: PathBuf) -> Result<PathBuf, String> {
    if path.is_absolute() {
        return Ok(path);
    }
    let current_dir = std::env::current_dir()
        .map_err(|err| format!("failed to read current directory: {err}"))?;
    Ok(current_dir.join(path))
}

fn cleanup_temp_file(path: &PathBuf) {
    let _ = fs::remove_file(path);
}

fn print_summary(
    disk_img: &Path,
    uapps: &[Uapp],
    install_files: &[debugfs::InstallFile],
    autostart_entry_count: usize,
    debugfs_command_count: usize,
    is_dry_run: bool,
) {
    println!("uapp install summary:");
    println!("  disk image: {}", disk_img.display());
    println!("  mode: {}", if is_dry_run { "dry-run" } else { "write" });
    println!("  apps:");
    for uapp in uapps {
        println!("    - {} ({})", uapp.name(), uapp.manifest_path.display());
    }
    println!("  files installed: {}", install_files.len());
    println!("  installed paths:");
    for file in install_files {
        println!(
            "    - {} <- {} (mode {:03o})",
            file.guest_path,
            file.host_path.display(),
            file.mode
        );
    }
    println!("  autostart entries: {autostart_entry_count}");
    println!("  debugfs commands: {debugfs_command_count}");
}

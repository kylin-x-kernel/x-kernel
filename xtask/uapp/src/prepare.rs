// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use crate::manifest::Uapp;

#[derive(Debug, Clone)]
pub struct PrepareContext {
    pub repo_root: PathBuf,
    pub disk_img: PathBuf,
    pub build_dir: PathBuf,
    pub arch: String,
    pub target: String,
    pub plat_name: String,
    pub cross_compile: String,
}

impl PrepareContext {
    pub fn for_prepare(repo_root: PathBuf, disk_img: PathBuf, build_dir: PathBuf) -> Self {
        Self {
            repo_root,
            disk_img,
            build_dir,
            arch: String::new(),
            target: String::new(),
            plat_name: String::new(),
            cross_compile: String::new(),
        }
    }
}

pub fn run_prepare_commands(uapps: &[Uapp], context: &PrepareContext) -> Result<(), String> {
    for uapp in uapps {
        let uapp_build_dir = context.build_dir.join(uapp.name());
        let uapp_out_dir = uapp_build_dir.join("out");
        fs::create_dir_all(&uapp_out_dir).map_err(|err| {
            format!(
                "failed to create uapp output directory {}: {err}",
                uapp_out_dir.display()
            )
        })?;

        for command in &uapp.manifest.prepare.commands {
            println!("uapp prepare [{}]: {}", uapp.name(), command);
            run_shell_command(uapp, context, &uapp_build_dir, &uapp_out_dir, command)?;
        }
    }
    Ok(())
}

fn run_shell_command(
    uapp: &Uapp,
    context: &PrepareContext,
    uapp_build_dir: &Path,
    uapp_out_dir: &Path,
    command: &str,
) -> Result<(), String> {
    let mut process = Command::new("sh");
    process
        .arg("-c")
        .arg(command)
        .current_dir(&uapp.dir)
        .env("REPO_ROOT", &context.repo_root)
        .env("UAPP_NAME", uapp.name())
        .env("UAPP_DIR", &uapp.dir)
        .env("UAPP_BUILD_DIR", uapp_build_dir)
        .env("UAPP_OUT_DIR", uapp_out_dir)
        .env("DISK_IMG", &context.disk_img)
        .env("K_ARCH", &context.arch)
        .env("K_TARGET", &context.target)
        .env("K_PLAT_NAME", &context.plat_name)
        .env("CROSS_COMPILE", &context.cross_compile);
    for entry in &uapp.manifest.prepare.env {
        let (name, value) = entry
            .split_once('=')
            .ok_or_else(|| format!("invalid prepare env entry for {}: {entry}", uapp.name()))?;
        process.env(name, value);
    }

    let status = process
        .status()
        .map_err(|err| format!("failed to run prepare command for {}: {err}", uapp.name()))?;

    if !status.success() {
        return Err(format!(
            "prepare command for {} exited with {status}: {command}",
            uapp.name()
        ));
    }
    Ok(())
}

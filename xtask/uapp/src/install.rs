// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    debugfs::{DebugfsScript, InstallFile},
    manifest::Uapp,
};

pub fn collect_install_files(uapps: &[Uapp]) -> Result<Vec<InstallFile>, String> {
    let mut files = Vec::new();
    for uapp in uapps {
        for entry in &uapp.manifest.install {
            let src = resolve_src(&uapp.dir, &entry.src);
            let mode = parse_mode(entry.mode.as_deref())?;
            append_install_entry(&mut files, &src, &entry.dest, mode)?;
        }
    }
    Ok(files)
}

pub fn write_autostart_file(content: &str) -> Result<PathBuf, String> {
    let path = temp_path("xkernel-uapp-99-autostart", "sh");
    fs::write(&path, content)
        .map_err(|err| format!("failed to write autostart script {}: {err}", path.display()))?;
    Ok(path)
}

pub fn write_debugfs_script(script: &DebugfsScript) -> Result<PathBuf, String> {
    let path = temp_path("xkernel-uapp-debugfs", "commands");
    script.write_to(&path)?;
    Ok(path)
}

pub fn add_autostart(script: &mut DebugfsScript, autostart_host_path: PathBuf, guest_path: &str) {
    script.add_existing_parent_file(&InstallFile {
        host_path: autostart_host_path,
        guest_path: guest_path.to_string(),
        mode: 0o755,
    });
}

fn append_install_entry(
    files: &mut Vec<InstallFile>,
    src: &Path,
    dest: &str,
    mode: u32,
) -> Result<(), String> {
    let metadata = fs::symlink_metadata(src)
        .map_err(|err| format!("install source not found: {}: {err}", src.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "install source must not be a symlink: {}",
            src.display()
        ));
    }

    if metadata.is_file() {
        files.push(InstallFile {
            host_path: fs::canonicalize(src)
                .map_err(|err| format!("failed to canonicalize {}: {err}", src.display()))?,
            guest_path: dest.to_string(),
            mode,
        });
        return Ok(());
    }

    if metadata.is_dir() {
        append_dir(files, src, src, dest, mode)?;
        return Ok(());
    }

    Err(format!(
        "install source must be a regular file or directory: {}",
        src.display()
    ))
}

fn append_dir(
    files: &mut Vec<InstallFile>,
    root: &Path,
    dir: &Path,
    dest: &str,
    mode: u32,
) -> Result<(), String> {
    let mut entries = Vec::new();
    for entry in
        fs::read_dir(dir).map_err(|err| format!("failed to read {}: {err}", dir.display()))?
    {
        let entry = entry.map_err(|err| format!("failed to read directory entry: {err}"))?;
        entries.push(entry.path());
    }
    entries.sort();

    for path in entries {
        let metadata = fs::symlink_metadata(&path)
            .map_err(|err| format!("failed to stat {}: {err}", path.display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "install source must not be a symlink: {}",
                path.display()
            ));
        }
        if metadata.is_dir() {
            append_dir(files, root, &path, dest, mode)?;
        } else if metadata.is_file() {
            let relative = path.strip_prefix(root).map_err(|err| {
                format!(
                    "failed to compute relative path for {} from {}: {err}",
                    path.display(),
                    root.display()
                )
            })?;
            let guest_path = join_guest_path(dest, relative)?;
            files.push(InstallFile {
                host_path: fs::canonicalize(&path)
                    .map_err(|err| format!("failed to canonicalize {}: {err}", path.display()))?,
                guest_path,
                mode,
            });
        } else {
            return Err(format!(
                "install source must be a regular file or directory: {}",
                path.display()
            ));
        }
    }

    Ok(())
}

fn resolve_src(uapp_dir: &Path, src: &str) -> PathBuf {
    let path = PathBuf::from(src);
    if path.is_absolute() {
        path
    } else {
        uapp_dir.join(path)
    }
}

fn join_guest_path(dest: &str, relative: &Path) -> Result<String, String> {
    let mut guest_path = dest.trim_end_matches('/').to_string();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(format!(
                "directory install path contains unsupported component: {}",
                relative.display()
            ));
        };
        guest_path.push('/');
        guest_path.push_str(&name.to_string_lossy());
    }
    Ok(guest_path)
}

fn parse_mode(mode: Option<&str>) -> Result<u32, String> {
    let Some(mode) = mode else {
        return Ok(0o644);
    };
    u32::from_str_radix(mode, 8).map_err(|err| format!("invalid mode {mode}: {err}"))
}

fn temp_path(prefix: &str, suffix: &str) -> PathBuf {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("{prefix}-{pid}-{nanos}.{suffix}"))
}

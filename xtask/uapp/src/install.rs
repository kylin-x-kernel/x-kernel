// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use tempfile::NamedTempFile;

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
    let mut file = new_temp_file("xkernel-uapp-autostart-", ".sh")?;
    file.write_all(content.as_bytes())
        .map_err(|err| format!("failed to write autostart script: {err}"))?;
    persist_temp_file(file)
}

pub fn write_debugfs_script(script: &DebugfsScript) -> Result<PathBuf, String> {
    let file = new_temp_file("xkernel-uapp-debugfs-", ".commands")?;
    let path = file.path().to_path_buf();
    script.write_to(&path)?;
    persist_temp_file(file)
}

/// Create a temp file with an unpredictable name and exclusive (`O_EXCL`)
/// creation, so neither the name nor an attacker-placed symlink can be used
/// to redirect the write at an arbitrary path.
fn new_temp_file(prefix: &str, suffix: &str) -> Result<NamedTempFile, String> {
    tempfile::Builder::new()
        .prefix(prefix)
        .suffix(suffix)
        .tempfile()
        .map_err(|err| format!("failed to create temp file: {err}"))
}

/// Persist a `NamedTempFile` to its final path so it can be referenced by
/// absolute path (e.g. handed to `debugfs -f`) and later removed explicitly.
fn persist_temp_file(file: NamedTempFile) -> Result<PathBuf, String> {
    let (_file, path) = file
        .keep()
        .map_err(|err| format!("failed to persist temp file: {err}"))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debugfs::DebugfsScript;

    /// Temp files must get unpredictable, unique names rather than the old
    /// `pid-nanos` scheme, so they cannot be pre-created by an attacker.
    #[test]
    fn temp_file_names_are_unique_and_retain_content() {
        let a = write_autostart_file("echo a").unwrap();
        let b = write_autostart_file("echo b").unwrap();
        assert_ne!(a, b, "temp file names must be unique");

        assert_eq!(std::fs::read_to_string(&a).unwrap(), "echo a");

        let _ = fs::remove_file(&a);
        let _ = fs::remove_file(&b);
    }

    #[test]
    fn debugfs_temp_script_retains_commands() {
        let mut script = DebugfsScript::new();
        script.add_file(&InstallFile {
            host_path: PathBuf::from("/tmp/x-kernel-uapp-test"),
            guest_path: "/hello".to_string(),
            mode: 0o644,
        });
        let path = write_debugfs_script(&script).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("write"));
        let _ = fs::remove_file(&path);
    }
}

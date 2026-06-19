// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Debug, Clone)]
pub struct InstallFile {
    pub host_path: PathBuf,
    pub guest_path: String,
    pub mode: u32,
}

#[derive(Debug, Default)]
pub struct DebugfsScript {
    commands: Vec<String>,
}

impl DebugfsScript {
    pub fn new() -> Self {
        Self {
            commands: Vec::new(),
        }
    }

    pub fn add_file(&mut self, file: &InstallFile) {
        self.add_file_with_parent_mode(file, ParentDirMode::Create);
    }

    pub fn add_existing_parent_file(&mut self, file: &InstallFile) {
        self.add_file_with_parent_mode(file, ParentDirMode::RequireExisting);
    }

    fn add_file_with_parent_mode(&mut self, file: &InstallFile, parent_mode: ParentDirMode) {
        let parent = guest_parent(&file.guest_path);
        let name = guest_basename(&file.guest_path);
        if parent_mode == ParentDirMode::Create {
            self.ensure_dir(&parent);
        }
        self.commands.push(format!("cd {}", debugfs_quote(&parent)));
        self.commands.push(format!("rm {}", debugfs_quote(name)));
        self.commands.push(format!(
            "write {} {}",
            debugfs_quote_path(&file.host_path),
            debugfs_quote(name)
        ));
        self.commands.push(format!(
            "set_inode_field {} mode 0100{:03o}",
            debugfs_quote(&file.guest_path),
            file.mode
        ));
    }

    pub fn ensure_dir(&mut self, guest_path: &str) {
        let mut current = String::new();
        for component in guest_path
            .split('/')
            .filter(|component| !component.is_empty())
        {
            let parent = if current.is_empty() {
                "/".to_string()
            } else {
                current.clone()
            };
            current.push('/');
            current.push_str(component);
            self.commands.push(format!("cd {}", debugfs_quote(&parent)));
            self.commands
                .push(format!("mkdir {}", debugfs_quote(component)));
        }
    }

    pub fn write_to(&self, path: &Path) -> Result<(), String> {
        let mut content = self.commands.join("\n");
        content.push('\n');
        fs::write(path, content)
            .map_err(|err| format!("failed to write debugfs script {}: {err}", path.display()))
    }

    pub fn command_count(&self) -> usize {
        self.commands.len()
    }
}

pub fn run_debugfs(disk_img: &Path, command_file: &Path) -> Result<(), String> {
    let status = Command::new("debugfs")
        .arg("-w")
        .arg("-f")
        .arg(command_file)
        .arg(disk_img)
        .status()
        .map_err(|err| format!("failed to run debugfs: {err}"))?;
    if !status.success() {
        return Err(format!("debugfs exited with {status}"));
    }
    Ok(())
}

pub fn verify_paths(disk_img: &Path, paths: &[String]) -> Result<(), String> {
    for path in paths {
        debugfs_stat(disk_img, path)?;
    }
    Ok(())
}

pub fn verify_directory(disk_img: &Path, path: &str) -> Result<(), String> {
    let output = debugfs_stat(disk_img, path)?;
    if !output.contains("Type: directory") {
        return Err(format!(
            "debugfs verification failed for {path}: not a directory"
        ));
    }
    Ok(())
}

pub fn guest_parent(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(index) => trimmed[..index].to_string(),
    }
}

fn debugfs_stat(disk_img: &Path, path: &str) -> Result<String, String> {
    let output = Command::new("debugfs")
        .arg("-R")
        .arg(format!("stat {}", debugfs_quote(path)))
        .arg(disk_img)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|err| format!("failed to verify {path} with debugfs: {err}"))?;
    let mut text = String::new();
    text.push_str(&String::from_utf8_lossy(&output.stdout));
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    if !output.status.success()
        || text.contains("File not found")
        || text.contains("while looking up")
        || text.contains("while trying to resolve")
    {
        return Err(format!("debugfs verification failed for {path}: {text}"));
    }
    Ok(text)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParentDirMode {
    Create,
    RequireExisting,
}

fn guest_basename(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
}

fn debugfs_quote_path(path: &Path) -> String {
    debugfs_quote(&path.to_string_lossy())
}

fn debugfs_quote(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::{guest_basename, guest_parent};

    #[test]
    fn guest_parent_handles_root_children() {
        assert_eq!(guest_parent("/hello"), "/");
    }

    #[test]
    fn guest_parent_handles_nested_paths() {
        assert_eq!(guest_parent("/usr/local/bin/hello"), "/usr/local/bin");
    }

    #[test]
    fn guest_basename_handles_nested_paths() {
        assert_eq!(guest_basename("/usr/local/bin/hello"), "hello");
    }
}

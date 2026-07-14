// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{
    collections::BTreeSet,
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

#[derive(Debug)]
pub struct DebugfsScript {
    commands: Vec<String>,
    known_directories: BTreeSet<String>,
}

impl DebugfsScript {
    pub fn new() -> Self {
        let mut known_directories = BTreeSet::new();
        known_directories.insert("/".to_string());
        Self {
            commands: Vec::new(),
            known_directories,
        }
    }

    /// Discovers and records ancestor directories that already exist in `disk_img`.
    ///
    /// Recorded directories are skipped when later file additions generate their
    /// directory-creation commands.
    ///
    /// # Errors
    ///
    /// Returns an error if `debugfs` cannot inspect a path or an existing ancestor
    /// is not a directory.
    pub fn discover_existing_directories(
        &mut self,
        disk_img: &Path,
        guest_paths: &[String],
    ) -> Result<(), String> {
        for guest_path in guest_paths {
            let mut current = String::new();
            for component in guest_path
                .split('/')
                .filter(|component| !component.is_empty())
            {
                current.push('/');
                current.push_str(component);
                if self.known_directories.contains(&current) {
                    continue;
                }

                let Some(output) = debugfs_stat_if_exists(disk_img, &current)? else {
                    // Descendants cannot exist when their parent is absent. They will be
                    // created in order when the command script is executed.
                    break;
                };
                if !output.contains("Type: directory") {
                    return Err(format!(
                        "debugfs path required as directory is not a directory: {current}"
                    ));
                }
                self.known_directories.insert(current.clone());
            }
        }
        Ok(())
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
            if self.known_directories.insert(current.clone()) {
                self.commands.push(format!("cd {}", debugfs_quote(&parent)));
                self.commands
                    .push(format!("mkdir {}", debugfs_quote(component)));
            }
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

impl Default for DebugfsScript {
    fn default() -> Self {
        Self::new()
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
        validate_guest_path(path)?;
        debugfs_stat(disk_img, path)?;
    }
    Ok(())
}

pub fn verify_directory(disk_img: &Path, path: &str) -> Result<(), String> {
    validate_guest_path(path)?;
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
    debugfs_stat_if_exists(disk_img, path)?
        .ok_or_else(|| format!("debugfs verification failed for {path}: path does not exist"))
}

fn debugfs_stat_if_exists(disk_img: &Path, path: &str) -> Result<Option<String>, String> {
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
    if text.contains("File not found")
        || text.contains("while looking up")
        || text.contains("while trying to resolve")
    {
        return Ok(None);
    }
    if !output.status.success() {
        return Err(format!("debugfs verification failed for {path}: {text}"));
    }
    Ok(Some(text))
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

/// Validate a guest path inside the disk image (CWE-22 / CWE-78 hardening).
///
/// Guest paths are embedded into `debugfs` requests (`stat "<path>"`) and
/// command scripts. Reject anything that could escape the intended location
/// (`..`), break request parsing (control characters, NUL), or arrive without
/// an absolute root. All values reaching [`debugfs_quote`] are expected to be
/// pre-validated here.
pub(crate) fn validate_guest_path(path: &str) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err(format!("guest path must not be empty: {path:?}"));
    }
    if !path.starts_with('/') {
        return Err(format!("guest path must be absolute: {path:?}"));
    }
    if path.contains('\0') {
        return Err(format!("guest path must not contain NUL bytes: {path:?}"));
    }
    if path.chars().any(char::is_control) {
        return Err(format!(
            "guest path must not contain control characters: {path:?}"
        ));
    }
    if path.split('/').any(|component| component == "..") {
        return Err(format!(
            "guest path must not contain parent-directory (..) components: {path:?}"
        ));
    }
    Ok(())
}

fn debugfs_quote_path(path: &Path) -> String {
    debugfs_quote(&path.to_string_lossy())
}

/// Escape `value` for a `debugfs` double-quoted argument.
///
/// `debugfs` is invoked without a shell (`Command::new("debugfs").arg(...)`),
/// so this only needs to defeat the `debugfs` request lexer: backslash and
/// double-quote are the only characters that can alter quoting. Guest paths
/// reaching here are pre-validated by [`validate_guest_path`].
fn debugfs_quote(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{DebugfsScript, InstallFile, guest_basename, guest_parent, validate_guest_path};

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

    #[test]
    fn validate_guest_path_accepts_absolute_paths() {
        assert!(validate_guest_path("/hello").is_ok());
        assert!(validate_guest_path("/usr/bin/hello").is_ok());
    }

    #[test]
    fn validate_guest_path_rejects_non_absolute() {
        assert!(validate_guest_path("hello").is_err());
        assert!(validate_guest_path("../hello").is_err());
    }

    #[test]
    fn validate_guest_path_rejects_parent_traversal() {
        assert!(validate_guest_path("/a/../b").is_err());
        assert!(validate_guest_path("/../etc/passwd").is_err());
    }

    #[test]
    fn validate_guest_path_rejects_control_chars_and_nul() {
        assert!(validate_guest_path("/a\nb").is_err());
        assert!(validate_guest_path("/a\0b").is_err());
    }

    #[test]
    fn directory_commands_skip_known_and_planned_directories() {
        let mut script = DebugfsScript::new();
        script.known_directories.insert("/usr".to_string());
        let first = InstallFile {
            host_path: PathBuf::from("/tmp/first"),
            guest_path: "/usr/tests/app/first".to_string(),
            mode: 0o755,
        };
        let second = InstallFile {
            host_path: PathBuf::from("/tmp/second"),
            guest_path: "/usr/tests/app/second".to_string(),
            mode: 0o755,
        };

        script.add_file(&first);
        script.add_file(&second);

        assert!(
            !script
                .commands
                .iter()
                .any(|command| command == "mkdir \"usr\"")
        );
        assert_eq!(
            script
                .commands
                .iter()
                .filter(|command| command.starts_with("mkdir "))
                .count(),
            2
        );
    }
}

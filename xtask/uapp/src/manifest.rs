// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct Uapp {
    pub dir: PathBuf,
    pub manifest_path: PathBuf,
    pub manifest: Manifest,
}

impl Uapp {
    pub fn name(&self) -> &str {
        &self.manifest.package.name
    }

    pub fn is_enabled(&self) -> bool {
        self.manifest.package.enabled.unwrap_or(true)
    }

    pub fn order(&self) -> u32 {
        self.manifest.package.order.unwrap_or(100)
    }

    fn matches_selection(&self, selection: &str) -> bool {
        if self.name() == selection {
            return true;
        }

        let selection_path = Path::new(selection);
        paths_match(selection_path, &self.dir) || paths_match(selection_path, &self.manifest_path)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub package: Package,
    #[serde(default)]
    pub prepare: Prepare,
    #[serde(default)]
    pub install: Vec<InstallEntry>,
    #[serde(default)]
    pub autostart: Vec<AutostartEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Package {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub order: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Prepare {
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub commands: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InstallEntry {
    pub src: String,
    pub dest: String,
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AutostartEntry {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub workdir: Option<String>,
    #[serde(default)]
    pub background: Option<bool>,
    #[serde(default)]
    pub check_alive: Option<bool>,
    #[serde(default)]
    pub exit: Option<bool>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

impl AutostartEntry {
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    pub fn runs_in_background(&self) -> bool {
        self.background.unwrap_or(false)
    }

    pub fn should_check_alive(&self) -> bool {
        self.check_alive
            .unwrap_or_else(|| self.runs_in_background())
    }

    pub fn should_exit(&self) -> bool {
        self.exit.unwrap_or(false)
    }
}

pub fn discover(uapps_dir: &Path) -> Result<Vec<Uapp>, String> {
    if !uapps_dir.is_dir() {
        return Err(format!(
            "uapps directory not found: {}",
            uapps_dir.display()
        ));
    }

    let mut uapps = Vec::new();
    for entry in fs::read_dir(uapps_dir)
        .map_err(|err| format!("failed to read {}: {err}", uapps_dir.display()))?
    {
        let entry = entry.map_err(|err| format!("failed to read uapp directory entry: {err}"))?;
        let file_type = entry.file_type().map_err(|err| {
            format!(
                "failed to read file type for {}: {err}",
                entry.path().display()
            )
        })?;
        if !file_type.is_dir() {
            continue;
        }

        let dir = entry.path();
        let manifest_path = dir.join("uapp.toml");
        if !manifest_path.exists() {
            continue;
        }

        let content = fs::read_to_string(&manifest_path)
            .map_err(|err| format!("failed to read {}: {err}", manifest_path.display()))?;
        let manifest: Manifest = toml::from_str(&content)
            .map_err(|err| format!("failed to parse {}: {err}", manifest_path.display()))?;

        validate_manifest(&dir, &manifest_path, &manifest)?;
        uapps.push(Uapp {
            dir,
            manifest_path,
            manifest,
        });
    }

    uapps.sort_by(|left, right| {
        left.order()
            .cmp(&right.order())
            .then_with(|| left.name().cmp(right.name()))
    });
    Ok(uapps)
}

pub fn select_uapps(uapps: Vec<Uapp>, selection: &str) -> Result<Vec<Uapp>, String> {
    let selection = selection.trim();
    if selection == "all" {
        return Ok(uapps.into_iter().filter(Uapp::is_enabled).collect());
    }
    if selection == "none" {
        return Ok(Vec::new());
    }

    let selections: Vec<&str> = selection
        .split(',')
        .map(str::trim)
        .filter(|selection| !selection.is_empty())
        .collect();
    if selections.is_empty() {
        return Err("uapp selection is empty".to_string());
    }

    let mut selected = Vec::new();
    for selection in selections {
        let Some(uapp) = uapps.iter().find(|uapp| uapp.matches_selection(selection)) else {
            return Err(format!("selected uapp not found: {selection}"));
        };
        if !uapp.is_enabled() {
            return Err(format!("selected uapp is disabled: {selection}"));
        }
        selected.push(uapp.clone());
    }
    Ok(selected)
}

fn paths_match(selection: &Path, path: &Path) -> bool {
    if selection == path {
        return true;
    }
    if let Ok(current_dir) = std::env::current_dir() {
        let absolute_selection = if selection.is_absolute() {
            selection.to_path_buf()
        } else {
            current_dir.join(selection)
        };
        let absolute_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            current_dir.join(path)
        };
        absolute_selection == absolute_path
    } else {
        false
    }
}

fn validate_manifest(dir: &Path, manifest_path: &Path, manifest: &Manifest) -> Result<(), String> {
    validate_non_empty("package.name", &manifest.package.name, manifest_path)?;
    if manifest.package.name.contains('/') {
        return Err(format!(
            "{}: package.name must not contain '/': {}",
            manifest_path.display(),
            manifest.package.name
        ));
    }
    if let Some(dir_name) = dir.file_name().and_then(|name| name.to_str())
        && dir_name != manifest.package.name
    {
        return Err(format!(
            "{}: package.name ({}) must match directory name ({dir_name})",
            manifest_path.display(),
            manifest.package.name
        ));
    }

    for entry in &manifest.install {
        validate_non_empty("install.src", &entry.src, manifest_path)?;
        validate_guest_path("install.dest", &entry.dest, manifest_path)?;
        if entry.dest == "/etc/profile.d/99-autostart.sh" {
            return Err(format!(
                "{}: install.dest must not target generated autostart script",
                manifest_path.display()
            ));
        }
        if let Some(mode) = &entry.mode {
            validate_mode(mode, manifest_path)?;
        }
    }

    for env in &manifest.prepare.env {
        validate_env_entry(env, manifest_path)?;
    }

    for entry in &manifest.autostart {
        validate_non_empty("autostart.name", &entry.name, manifest_path)?;
        validate_non_empty("autostart.command", &entry.command, manifest_path)?;
        if let Some(workdir) = &entry.workdir {
            validate_guest_path("autostart.workdir", workdir, manifest_path)?;
        }
    }

    Ok(())
}

fn validate_non_empty(field: &str, value: &str, manifest_path: &Path) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!(
            "{}: {field} must not be empty",
            manifest_path.display()
        ));
    }
    if value.contains('\0') {
        return Err(format!(
            "{}: {field} must not contain NUL bytes",
            manifest_path.display()
        ));
    }
    Ok(())
}

fn validate_guest_path(field: &str, value: &str, manifest_path: &Path) -> Result<(), String> {
    validate_non_empty(field, value, manifest_path)?;
    if !value.starts_with('/') {
        return Err(format!(
            "{}: {field} must be an absolute guest path: {value}",
            manifest_path.display()
        ));
    }
    Ok(())
}

fn validate_mode(mode: &str, manifest_path: &Path) -> Result<(), String> {
    if mode.len() != 4 || !mode.chars().all(|ch| matches!(ch, '0'..='7')) {
        return Err(format!(
            "{}: mode must be a four-digit octal string: {mode}",
            manifest_path.display()
        ));
    }
    Ok(())
}

fn validate_env_entry(env: &str, manifest_path: &Path) -> Result<(), String> {
    validate_non_empty("prepare.env", env, manifest_path)?;
    let Some((name, _value)) = env.split_once('=') else {
        return Err(format!(
            "{}: prepare.env entry must use KEY=value syntax: {env}",
            manifest_path.display()
        ));
    };
    if name.is_empty()
        || !name
            .chars()
            .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
        || name.chars().next().is_some_and(|ch| ch.is_ascii_digit())
    {
        return Err(format!(
            "{}: prepare.env has invalid variable name: {name}",
            manifest_path.display()
        ));
    }
    Ok(())
}

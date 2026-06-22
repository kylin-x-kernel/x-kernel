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
            run_prepare_command(uapp, context, &uapp_build_dir, &uapp_out_dir, command)?;
        }
    }
    Ok(())
}

fn run_prepare_command(
    uapp: &Uapp,
    context: &PrepareContext,
    uapp_build_dir: &Path,
    uapp_out_dir: &Path,
    command: &str,
) -> Result<(), String> {
    let argv = split_prepare_command(command).map_err(|err| {
        format!(
            "invalid prepare command for {}: {command}: {err}",
            uapp.name()
        )
    })?;
    let Some((program, args)) = argv.split_first() else {
        return Err(format!(
            "prepare command for {} must not be empty",
            uapp.name()
        ));
    };

    let mut process = Command::new(program);
    process
        .args(args)
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

fn split_prepare_command(command: &str) -> Result<Vec<String>, &'static str> {
    #[derive(Clone, Copy, Eq, PartialEq)]
    enum Quote {
        Single,
        Double,
    }

    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escape = false;
    let mut has_current_word = false;

    for ch in command.chars() {
        if escape {
            current.push(ch);
            escape = false;
            has_current_word = true;
            continue;
        }

        match quote {
            Some(Quote::Single) => {
                if ch == '\'' {
                    quote = None;
                } else {
                    current.push(ch);
                    has_current_word = true;
                }
            }
            Some(Quote::Double) => {
                if ch == '"' {
                    quote = None;
                } else if ch == '\\' {
                    escape = true;
                } else {
                    current.push(ch);
                    has_current_word = true;
                }
            }
            None => {
                if ch.is_whitespace() {
                    if has_current_word {
                        words.push(std::mem::take(&mut current));
                        has_current_word = false;
                    }
                } else if ch == '\'' {
                    quote = Some(Quote::Single);
                    has_current_word = true;
                } else if ch == '"' {
                    quote = Some(Quote::Double);
                    has_current_word = true;
                } else if ch == '\\' {
                    escape = true;
                    has_current_word = true;
                } else {
                    current.push(ch);
                    has_current_word = true;
                }
            }
        }
    }

    if escape {
        return Err("unterminated escape");
    }
    if quote.is_some() {
        return Err("unterminated quote");
    }
    if has_current_word {
        words.push(current);
    }

    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::split_prepare_command;

    #[test]
    fn split_prepare_command_preserves_shell_metacharacters_as_args() {
        let argv = split_prepare_command("echo hello; rm -rf /tmp/test").unwrap();

        assert_eq!(argv, ["echo", "hello;", "rm", "-rf", "/tmp/test"]);
    }

    #[test]
    fn split_prepare_command_supports_quoted_arguments_without_shell_execution() {
        let argv = split_prepare_command("printf '%s %s' foo bar").unwrap();

        assert_eq!(argv, ["printf", "%s %s", "foo", "bar"]);
    }

    #[test]
    fn split_prepare_command_rejects_unterminated_quotes() {
        assert!(split_prepare_command("printf 'broken").is_err());
    }

    #[test]
    fn split_prepare_command_keeps_empty_quoted_arguments() {
        let argv = split_prepare_command("printf '' \"\" tail").unwrap();

        assert_eq!(argv, ["printf", "", "", "tail"]);
    }
}

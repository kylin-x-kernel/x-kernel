// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel unit-test coverage extraction and report generation.

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use lcov2cobertura::{RustDemangler, coverage_to_file, parse_file};

use crate::{
    build::Bundle,
    cli::RunArgs,
    error::{Error, IoResultExt, Result},
    process::Process,
};

const GUEST_PROFRAW_PATH: &str = "/.llvm-cov/default.profraw";
const COVERAGE_IGNORE_REGEX: &str =
    "/target/|/api/kapi/src/syscall/|/api/linux_sysno/src/|/boot/kernel-boot/";

pub(crate) fn generate(bundle: &Bundle, args: &RunArgs) -> Result<()> {
    if !bundle.context.unittest {
        return Ok(());
    }

    let context = &bundle.context;
    let artifacts = CoverageArtifacts::new(bundle);
    if !context.dry_run {
        fs::create_dir_all(&artifacts.directory).with_path(&artifacts.directory)?;
        artifacts.remove_stale_files()?;
    }

    println!("Generating unit-test coverage reports...");
    extract_profraw(bundle, args, &artifacts)?;
    merge_profraw(bundle, &artifacts)?;
    write_text_report(bundle, &artifacts)?;
    export_lcov(bundle, &artifacts)?;
    if !context.dry_run {
        write_cobertura_report(bundle, &artifacts)?;
    } else {
        println!(
            "+ convert {} -> {}",
            artifacts.lcov.display(),
            artifacts.cobertura.display()
        );
    }

    if !context.dry_run {
        println!(
            "Coverage reports generated in {}",
            artifacts.directory.display()
        );
    }
    Ok(())
}

struct CoverageArtifacts {
    directory: PathBuf,
    profraw: PathBuf,
    profdata: PathBuf,
    text: PathBuf,
    lcov: PathBuf,
    cobertura: PathBuf,
}

impl CoverageArtifacts {
    fn new(bundle: &Bundle) -> Self {
        let context = &bundle.context;
        let directory = context
            .target_dir
            .join(context.config.target())
            .join(context.config.profile().as_str());
        Self {
            profraw: directory.join("default.profraw"),
            profdata: directory.join("default.profdata"),
            text: directory.join("coverage.txt"),
            lcov: directory.join("coverage.info"),
            cobertura: directory.join("coverage.xml"),
            directory,
        }
    }

    fn remove_stale_files(&self) -> Result<()> {
        for path in [
            &self.profraw,
            &self.profdata,
            &self.text,
            &self.lcov,
            &self.cobertura,
        ] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(Error::Io {
                        path: path.to_path_buf(),
                        source,
                    });
                }
            }
        }
        Ok(())
    }
}

fn extract_profraw(bundle: &Bundle, args: &RunArgs, artifacts: &CoverageArtifacts) -> Result<()> {
    let context = &bundle.context;
    let disk_image = context.workspace_root.join(&args.disk_image);
    let request = format!(
        "dump {} {}",
        debugfs_quote(GUEST_PROFRAW_PATH),
        debugfs_quote(&artifacts.profraw.to_string_lossy())
    );
    let mut command = Process::new("debugfs", context.dry_run, context.verbosity);
    command
        .current_dir(&context.workspace_root)
        .args(["-R", &request])
        .arg(&disk_image)
        .run()
        .map_err(|error| {
            Error::Message(format!(
                "failed to extract {GUEST_PROFRAW_PATH} from {}: {error}",
                disk_image.display()
            ))
        })?;
    if !context.dry_run {
        let size_bytes = fs::metadata(&artifacts.profraw)
            .with_path(&artifacts.profraw)?
            .len();
        if size_bytes == 0 {
            return Err(Error::Message(format!(
                "extracted coverage profile is empty: {}",
                artifacts.profraw.display()
            )));
        }
        println!(
            "Extracted coverage profile to {} ({size_bytes} bytes)",
            artifacts.profraw.display()
        );
    }
    Ok(())
}

fn merge_profraw(bundle: &Bundle, artifacts: &CoverageArtifacts) -> Result<()> {
    let context = &bundle.context;
    Process::new("rust-profdata", context.dry_run, context.verbosity)
        .current_dir(&context.workspace_root)
        .args(["merge", "-o"])
        .arg(&artifacts.profdata)
        .arg(&artifacts.profraw)
        .run()
}

fn write_text_report(bundle: &Bundle, artifacts: &CoverageArtifacts) -> Result<()> {
    let context = &bundle.context;
    let mut command = Process::new("rust-cov", context.dry_run, context.verbosity);
    command
        .current_dir(&context.workspace_root)
        .arg("report")
        .arg(&context.bundle_elf)
        .arg(format!("--instr-profile={}", artifacts.profdata.display()))
        .arg(format!("--ignore-filename-regex={COVERAGE_IGNORE_REGEX}"))
        .arg(&context.workspace_root)
        .stdout_to_file(&artifacts.text)
        .run()
}

fn export_lcov(bundle: &Bundle, artifacts: &CoverageArtifacts) -> Result<()> {
    let context = &bundle.context;
    let mut command = Process::new("rust-cov", context.dry_run, context.verbosity);
    command
        .current_dir(&context.workspace_root)
        .arg("export")
        .arg(&context.bundle_elf)
        .arg(format!("--instr-profile={}", artifacts.profdata.display()))
        .args(["--format=lcov"])
        .arg(format!("--ignore-filename-regex={COVERAGE_IGNORE_REGEX}"))
        .arg(&context.workspace_root)
        .stdout_to_file(&artifacts.lcov)
        .run()
}

fn write_cobertura_report(bundle: &Bundle, artifacts: &CoverageArtifacts) -> Result<()> {
    let context = &bundle.context;
    let coverage = parse_file(&artifacts.lcov, &context.workspace_root, &[]).map_err(|error| {
        Error::Message(format!(
            "failed to parse LCOV tracefile {}: {error}",
            artifacts.lcov.display()
        ))
    })?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| Error::Message(format!("system clock error: {error}")))?
        .as_secs();
    let pending_output = pending_output_path(&artifacts.cobertura)?;
    let _ = fs::remove_file(&pending_output);
    if let Err(error) =
        coverage_to_file(&pending_output, &coverage, timestamp, RustDemangler::new())
    {
        let _ = fs::remove_file(&pending_output);
        return Err(Error::Message(format!(
            "failed to write Cobertura XML to {}: {error}",
            artifacts.cobertura.display()
        )));
    }
    promote_output(&pending_output, &artifacts.cobertura)?;
    println!(
        "Converted {} to {}",
        artifacts.lcov.display(),
        artifacts.cobertura.display()
    );
    Ok(())
}

fn pending_output_path(output: &Path) -> Result<PathBuf> {
    let file_name = output.file_name().ok_or_else(|| {
        Error::Message(format!(
            "coverage output path has no file name: {}",
            output.display()
        ))
    })?;
    let mut pending_name = OsString::from(file_name);
    pending_name.push(format!(".tmp.{}", std::process::id()));
    Ok(output.with_file_name(pending_name))
}

fn promote_output(pending_output: &Path, output: &Path) -> Result<()> {
    match fs::remove_file(output) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            let _ = fs::remove_file(pending_output);
            return Err(Error::Io {
                path: output.to_path_buf(),
                source,
            });
        }
    }
    if let Err(source) = fs::rename(pending_output, output) {
        let _ = fs::remove_file(pending_output);
        return Err(Error::Io {
            path: output.to_path_buf(),
            source,
        });
    }
    Ok(())
}

fn debugfs_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('\"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::debugfs_quote;

    #[test]
    fn debugfs_request_values_escape_quotes_and_backslashes() {
        assert_eq!(debugfs_quote(r#"a\b"c"#), r#""a\\b\"c""#);
    }
}

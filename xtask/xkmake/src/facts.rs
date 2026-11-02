// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Packs the code facts that the Dylint scan exports into a versioned
//! artifact (`make dylint FACTS_OUT=<dir>`).
//!
//! The `facts_export` pass in `tools/lints` writes `<stem>.facts.jsonl` next
//! to each checked crate's metadata while [`FACTS_ENV`] is `1`. The scan runs
//! Cargo with JSON messages; every `compiler-artifact` message of a
//! workspace member, fresh or rebuilt, names one unit of this build, so the
//! files of exactly those units form the artifact. Files of other
//! configurations that share the target directory are never picked up.
//!
//! Output layout:
//!
//! ```text
//! <dir>/status.json             running | ready | partial | failed, reason
//! <dir>/manifest.json           build identity, crate list, totals
//! <dir>/crates/<stem>.jsonl.zst one zstd-compressed facts file per crate
//! ```

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    build::kernel_feature_flags,
    context::BuildContext,
    error::{Error, IoResultExt, Result},
    hygiene::DYLINT_VERSION,
};

/// Version of the manifest and of the per-crate records it lists. Must match
/// `SCHEMA` in `tools/lints/src/facts_export/mod.rs`.
pub(crate) const FACTS_SCHEMA: &str = "xkernel-facts/0.2";
/// Version of `status.json`.
const STATUS_SCHEMA: &str = "xkernel-facts-status/1";
/// Switches the exporter on. Its value is always `1`: rustc records it as an
/// environment dependency, so any other value would re-check every crate.
pub(crate) const FACTS_ENV: &str = "XKERNEL_FACTS";
/// Cargo message format of a facts scan: JSON messages on stdout for the
/// packer, diagnostics still rendered on stderr for the log.
pub(crate) const MESSAGE_FORMAT_ARG: &str = "--message-format=json-render-diagnostics";
/// Matches `FILE_EXTENSION` in the exporter.
const FACTS_EXTENSION: &str = "facts.jsonl";
const COMPRESSION_LEVEL: i32 = 3;
const STATUS_FILE: &str = "status.json";
const MANIFEST_FILE: &str = "manifest.json";
const CRATES_DIR: &str = "crates";
/// Inputs of the exporter build, relative to `tools/lints`; their digest
/// identifies the exporter that produced an artifact.
const EXPORTER_SOURCES: &[&str] = &["Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "src"];
/// `DYLINT_RUSTFLAGS` of every scan (`dylint::run`).
pub(crate) const SCAN_RUSTFLAGS: &[&str] = &["--cfg", "unittest"];

/// Artifact state in `status.json`.
///
/// `running` in a finished CI build means the scan was interrupted (stage
/// timeout or abort) before the packer ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum FactsState {
    Running,
    /// Every unit of the scan has facts.
    Ready,
    /// Some units have facts; see the reason.
    Partial,
    /// No facts.
    Failed,
}

/// Why an artifact is not `ready`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum FailureReason {
    /// The lint library or the scanned kernel failed to build, or a lint
    /// reported an error; units after the failure are absent.
    Build,
    /// The scan succeeded but facts files are missing or invalid, or the
    /// packer failed.
    Exporter,
}

/// Commit identity recorded in the artifact.
#[derive(Clone, Debug, Default)]
pub(crate) struct FactsIdentity {
    /// Commit of the scanned tree; `git rev-parse HEAD` when not given.
    pub(crate) commit: Option<String>,
    /// Merge base with the target branch, for pull-request builds.
    pub(crate) base: Option<String>,
}

/// One facts scan in progress; created before the scan, finished after it.
pub(crate) struct FactsRun {
    dir: PathBuf,
    messages: PathBuf,
    started: Instant,
    commit: Option<String>,
    base: Option<String>,
    target: String,
    platform: String,
}

#[derive(Serialize)]
struct StatusFile<'a> {
    schema: &'static str,
    status: FactsState,
    reason: Option<FailureReason>,
    detail: Option<&'a str>,
    commit: Option<&'a str>,
    target: &'a str,
    platform: &'a str,
    manifest_sha256: Option<&'a str>,
    updated: String,
}

#[derive(Serialize)]
struct Manifest {
    schema: &'static str,
    commit: Option<String>,
    base: Option<String>,
    target: String,
    platform: String,
    arch: String,
    profile: String,
    config_sha256: String,
    features: Vec<String>,
    rustflags: Vec<String>,
    unittest: bool,
    toolchain: String,
    rustc: Option<String>,
    exporter: ExporterInfo,
    status: FactsState,
    reason: Option<FailureReason>,
    crates: Vec<CrateEntry>,
    missing: Vec<MissingUnit>,
    counts: BTreeMap<String, u64>,
    created: String,
    /// Scan and packing, from the start of the run.
    seconds: f64,
    /// Packing alone.
    pack_seconds: f64,
}

#[derive(Serialize)]
struct ExporterInfo {
    /// Digest of the exporter's build inputs; see [`exporter_digest`].
    source_sha256: String,
    dylint: &'static str,
}

#[derive(Serialize)]
struct CrateEntry {
    name: String,
    crate_id: String,
    package: Option<String>,
    version: Option<String>,
    kind: String,
    stem: String,
    file: String,
    sha256: String,
    records: u64,
    bytes: u64,
    compressed_bytes: u64,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct MissingUnit {
    target: String,
    stem: String,
    problem: String,
}

/// One unit of the scan, from a Cargo `compiler-artifact` message.
#[derive(Debug, PartialEq, Eq)]
struct Unit {
    package_id: String,
    target: String,
    /// Output directory of the unit's metadata.
    dir: PathBuf,
    /// Crate name plus Cargo's `-C extra-filename`.
    stem: String,
}

#[derive(Deserialize)]
struct ArtifactMessage {
    reason: String,
    #[serde(default)]
    package_id: String,
    #[serde(default)]
    target: Option<ArtifactTarget>,
    #[serde(default)]
    filenames: Vec<PathBuf>,
}

#[derive(Deserialize)]
struct ArtifactTarget {
    name: String,
    kind: Vec<String>,
}

/// Header and totals of one facts file.
struct FactsSummary {
    crate_name: String,
    crate_id: String,
    package: Option<String>,
    version: Option<String>,
    kind: String,
    rustc: Option<String>,
    records: u64,
    counts: BTreeMap<String, u64>,
}

impl FactsRun {
    /// Removes the artifact of an earlier run from `dir` (status, manifest
    /// and crate files), so that a run failing before [`Self::begin`] leaves
    /// no status rather than a stale one. Other files are kept.
    pub(crate) fn clear(dir: &Path) -> Result<()> {
        for stale in [CRATES_DIR, MANIFEST_FILE, STATUS_FILE] {
            remove_if_present(&dir.join(stale))?;
        }
        Ok(())
    }

    /// Prepares `dir`: removes the previous artifact and marks it running.
    pub(crate) fn begin(
        context: &BuildContext,
        dir: &Path,
        identity: FactsIdentity,
    ) -> Result<Self> {
        let dir = context.workspace_root.join(dir);
        Self::clear(&dir)?;
        let crates = dir.join(CRATES_DIR);
        fs::create_dir_all(&crates).with_path(&crates)?;
        let commit = identity
            .commit
            .or_else(|| git_head(&context.workspace_root));
        let run = Self {
            messages: context
                .target_dir
                .join("dylint")
                .join("facts-messages.jsonl"),
            dir,
            started: Instant::now(),
            commit,
            base: identity.base,
            target: context.config.target().to_owned(),
            platform: context.config.platform().to_owned(),
        };
        run.write_status(FactsState::Running, None, None, None)?;
        Ok(run)
    }

    /// File that receives the scan's Cargo JSON messages.
    pub(crate) fn messages_path(&self) -> &Path {
        &self.messages
    }

    /// Records a failure before the scan produced any facts.
    pub(crate) fn fail(&self, reason: FailureReason, detail: &str) -> Result<()> {
        self.write_status(FactsState::Failed, Some(reason), Some(detail), None)
    }

    /// Packs the facts of the scan's units and writes the manifest and the
    /// final status.
    pub(crate) fn finish(
        &self,
        context: &BuildContext,
        config_path: &Path,
        toolchain: &str,
        scan_succeeded: bool,
    ) -> Result<FactsState> {
        match self.pack(context, config_path, toolchain, scan_succeeded) {
            Ok(state) => Ok(state),
            Err(error) => {
                let detail = format!("packing failed: {error}");
                self.fail(FailureReason::Exporter, &detail)?;
                Err(error)
            }
        }
    }

    fn pack(
        &self,
        context: &BuildContext,
        config_path: &Path,
        toolchain: &str,
        scan_succeeded: bool,
    ) -> Result<FactsState> {
        let pack_started = Instant::now();
        let members = workspace_members(context, toolchain)?;
        let messages = fs::read_to_string(&self.messages).with_path(&self.messages)?;
        let units = scan_units(&messages, &members);
        let crates_dir = self.dir.join(CRATES_DIR);
        let mut crates = Vec::new();
        let mut missing = Vec::new();
        let mut counts = BTreeMap::new();
        let mut rustc = None;
        for unit in units {
            let source = unit.dir.join(format!("{}.{FACTS_EXTENSION}", unit.stem));
            let data = match fs::read(&source) {
                Ok(data) => data,
                Err(error) => {
                    missing.push(MissingUnit {
                        target: unit.target,
                        stem: unit.stem,
                        problem: format!("no facts file: {error}"),
                    });
                    continue;
                }
            };
            let summary = match summarize(&data, &unit.stem) {
                Ok(summary) => summary,
                Err(problem) => {
                    missing.push(MissingUnit {
                        target: unit.target,
                        stem: unit.stem,
                        problem,
                    });
                    continue;
                }
            };
            let file = format!("{CRATES_DIR}/{}.jsonl.zst", unit.stem);
            let compressed = zstd::bulk::compress(&data, COMPRESSION_LEVEL)
                .map_err(|error| Error::Message(format!("zstd {}: {error}", source.display())))?;
            let target = crates_dir.join(format!("{}.jsonl.zst", unit.stem));
            fs::write(&target, &compressed).with_path(&target)?;
            for (key, value) in &summary.counts {
                *counts.entry(key.clone()).or_insert(0) += value;
            }
            if rustc.is_none() {
                rustc = summary.rustc.clone();
            }
            crates.push(CrateEntry {
                name: summary.crate_name,
                crate_id: summary.crate_id,
                package: summary.package,
                version: summary.version,
                kind: summary.kind,
                stem: unit.stem,
                file,
                sha256: hex_sha256(&compressed),
                records: summary.records,
                bytes: data.len() as u64,
                compressed_bytes: compressed.len() as u64,
            });
        }
        crates.sort_by(|a, b| a.stem.cmp(&b.stem));
        let (status, reason) = artifact_state(scan_succeeded, crates.len(), missing.len());
        let config = fs::read(config_path).with_path(config_path)?;
        let lint_dir = context.workspace_root.join("tools/lints");
        let manifest = Manifest {
            schema: FACTS_SCHEMA,
            commit: self.commit.clone(),
            base: self.base.clone(),
            target: self.target.clone(),
            platform: self.platform.clone(),
            arch: context.config.arch().as_str().to_owned(),
            profile: context.config.profile().as_str().to_owned(),
            config_sha256: hex_sha256(&config),
            features: kernel_feature_flags(context),
            rustflags: SCAN_RUSTFLAGS
                .iter()
                .map(|flag| (*flag).to_owned())
                .collect(),
            unittest: context.unittest,
            toolchain: toolchain.to_owned(),
            rustc,
            exporter: ExporterInfo {
                source_sha256: exporter_digest(&lint_dir)?,
                dylint: DYLINT_VERSION,
            },
            status,
            reason,
            crates,
            missing,
            counts,
            created: now_rfc3339(),
            seconds: self.started.elapsed().as_secs_f64(),
            pack_seconds: pack_started.elapsed().as_secs_f64(),
        };
        let manifest_json = serde_json::to_vec_pretty(&manifest)
            .map_err(|error| Error::Message(format!("cannot serialize facts manifest: {error}")))?;
        let manifest_path = self.dir.join(MANIFEST_FILE);
        write_atomic(&manifest_path, &manifest_json)?;
        let detail = describe(&manifest);
        self.write_status(
            status,
            reason,
            detail.as_deref(),
            Some(&hex_sha256(&manifest_json)),
        )?;
        println!(
            "facts: {} ({} crates, {} missing, packed in {:.1}s) -> {}",
            serde_json::to_string(&status)
                .unwrap_or_default()
                .trim_matches('"'),
            manifest.crates.len(),
            manifest.missing.len(),
            manifest.pack_seconds,
            self.dir.display()
        );
        Ok(status)
    }

    fn write_status(
        &self,
        status: FactsState,
        reason: Option<FailureReason>,
        detail: Option<&str>,
        manifest_sha256: Option<&str>,
    ) -> Result<()> {
        let file = StatusFile {
            schema: STATUS_SCHEMA,
            status,
            reason,
            detail,
            commit: self.commit.as_deref(),
            target: &self.target,
            platform: &self.platform,
            manifest_sha256,
            updated: now_rfc3339(),
        };
        let json = serde_json::to_vec_pretty(&file)
            .map_err(|error| Error::Message(format!("cannot serialize facts status: {error}")))?;
        write_atomic(&self.dir.join(STATUS_FILE), &json)
    }
}

/// Status and reason from the scan result and the packed crate counts.
fn artifact_state(
    scan_succeeded: bool,
    packed: usize,
    missing: usize,
) -> (FactsState, Option<FailureReason>) {
    let reason = if scan_succeeded {
        FailureReason::Exporter
    } else {
        FailureReason::Build
    };
    match (scan_succeeded && missing == 0, packed) {
        (true, _) => (FactsState::Ready, None),
        (false, 0) => (FactsState::Failed, Some(reason)),
        (false, _) => (FactsState::Partial, Some(reason)),
    }
}

fn describe(manifest: &Manifest) -> Option<String> {
    match manifest.reason? {
        FailureReason::Build => Some(format!(
            "the scan failed; {} crates exported before it stopped",
            manifest.crates.len()
        )),
        FailureReason::Exporter => Some(format!(
            "{} units without valid facts: {}",
            manifest.missing.len(),
            manifest
                .missing
                .iter()
                .map(|unit| unit.target.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Units of workspace members in Cargo's JSON messages, one per stem.
/// Build scripts are host programs and are skipped.
fn scan_units(messages: &str, members: &HashSet<String>) -> Vec<Unit> {
    let mut seen = BTreeSet::new();
    let mut units = Vec::new();
    for line in messages.lines() {
        let Ok(message) = serde_json::from_str::<ArtifactMessage>(line) else {
            continue;
        };
        if message.reason != "compiler-artifact" || !members.contains(&message.package_id) {
            continue;
        }
        let Some(target) = message.target else {
            continue;
        };
        if target.kind.iter().any(|kind| kind == "custom-build") {
            continue;
        }
        let Some((dir, stem)) = message.filenames.first().and_then(|path| unit_stem(path)) else {
            continue;
        };
        if seen.insert(stem.clone()) {
            units.push(Unit {
                package_id: message.package_id,
                target: target.name,
                dir,
                stem,
            });
        }
    }
    units
}

/// Splits `<dir>/lib<stem>.<ext>` (rmeta, or a proc-macro's shared library).
fn unit_stem(path: &Path) -> Option<(PathBuf, String)> {
    let dir = path.parent()?.to_path_buf();
    let name = path.file_name()?.to_str()?;
    let name = name.strip_prefix("lib").unwrap_or(name);
    let (stem, _) = name.rsplit_once('.')?;
    Some((dir, stem.to_owned()))
}

/// Validates a facts file and reads its header and totals.
fn summarize(data: &[u8], stem: &str) -> std::result::Result<FactsSummary, String> {
    let text = std::str::from_utf8(data).map_err(|_| "facts file is not UTF-8".to_owned())?;
    let mut lines = text.lines();
    let header: Value = lines
        .next()
        .and_then(|line| serde_json::from_str(line).ok())
        .ok_or_else(|| "facts file has no header".to_owned())?;
    let field = |name: &str| header.get(name).and_then(Value::as_str).map(str::to_owned);
    if field("t").as_deref() != Some("crate") {
        return Err("first record is not a crate header".into());
    }
    let schema = field("schema").unwrap_or_default();
    if schema != FACTS_SCHEMA {
        return Err(format!("schema {schema}, expected {FACTS_SCHEMA}"));
    }
    if field("stem").as_deref() != Some(stem) {
        return Err("header stem does not match the unit".into());
    }
    let footer: Value = text
        .lines()
        .last()
        .and_then(|line| serde_json::from_str(line).ok())
        .ok_or_else(|| "facts file has no totals".to_owned())?;
    if footer.get("t").and_then(Value::as_str) != Some("counts") {
        return Err("last record is not the totals".into());
    }
    let counts = footer
        .get("counts")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(key, value)| Some((key.clone(), value.as_u64()?)))
                .collect()
        })
        .unwrap_or_default();
    Ok(FactsSummary {
        crate_name: field("crate").unwrap_or_default(),
        crate_id: field("crate_id").unwrap_or_default(),
        package: field("package"),
        version: field("version"),
        kind: field("kind").unwrap_or_default(),
        rustc: field("rustc"),
        records: text.lines().count() as u64,
        counts,
    })
}

/// Package ids of the kernel workspace members. Vendored crates reached
/// through `[patch]` are path packages too but not members, and the Dylint
/// driver does not check them.
fn workspace_members(context: &BuildContext, toolchain: &str) -> Result<HashSet<String>> {
    let output = Command::new("cargo")
        .current_dir(&context.workspace_root)
        .arg(format!("+{toolchain}"))
        .args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--manifest-path",
        ])
        .arg(&context.app_manifest)
        .env_remove("RUSTUP_TOOLCHAIN")
        .output()
        .map_err(|source| Error::Spawn {
            program: "cargo".into(),
            source,
        })?;
    if !output.status.success() {
        return Err(Error::CommandFailed {
            program: "cargo metadata".into(),
            status: output.status,
        });
    }
    let metadata: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| Error::Message(format!("cannot parse cargo metadata: {error}")))?;
    Ok(metadata
        .get("workspace_members")
        .and_then(Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(|id| id.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default())
}

/// SHA-256 over the sorted `<path>\0<sha256 of content>\n` lines of the
/// exporter's build inputs ([`EXPORTER_SOURCES`] under `tools/lints`), so
/// that a consumer can recompute it from any tree without Git.
fn exporter_digest(lint_dir: &Path) -> Result<String> {
    let mut files = Vec::new();
    for source in EXPORTER_SOURCES {
        collect_files(&lint_dir.join(source), &mut files)?;
    }
    let mut lines: Vec<String> = Vec::new();
    for file in files {
        let data = fs::read(&file).with_path(&file)?;
        let relative = file.strip_prefix(lint_dir).unwrap_or(&file);
        lines.push(format!(
            "{}\0{}\n",
            relative.to_string_lossy(),
            hex_sha256(&data)
        ));
    }
    lines.sort();
    Ok(hex_sha256(lines.concat().as_bytes()))
}

fn collect_files(path: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    if path.is_file() {
        files.push(path.to_path_buf());
        return Ok(());
    }
    for entry in fs::read_dir(path).with_path(path)? {
        let entry = entry.with_path(path)?;
        collect_files(&entry.path(), files)?;
    }
    Ok(())
}

fn git_head(root: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let head = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!head.is_empty()).then_some(head)
}

fn hex_sha256(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

fn remove_if_present(path: &Path) -> Result<()> {
    let result = if path.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    match result {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Writes through a temporary file so that a reader (or an archiving step
/// after an interrupted run) never sees a partial file.
fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp, data).with_path(&tmp)?;
    fs::rename(&tmp, path).with_path(path)
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, path::Path};

    use super::{
        FACTS_SCHEMA, FactsRun, FactsState, FailureReason, artifact_state, exporter_digest,
        scan_units, summarize, unit_stem,
    };

    const MEMBER: &str = "path+file:///w/task/kspin#0.3.0-dev";

    fn facts(stem: &str) -> String {
        format!(
            "{{\"t\":\"crate\",\"schema\":\"{FACTS_SCHEMA}\",\"crate\":\"kspin\",\"crate_id\":\"\
             beb65a3770141685\",\"stem\":\"{stem}\",\"kind\":\"workspace\",\"rustc\":\"1.96.\
             0-nightly\"}}\n{{\"t\":\"item\",\"key\":\"kspin\"}}\n{{\"t\":\"counts\",\"counts\":\
             {{\"call_edges\":3,\"export_us\":10}}}}\n"
        )
    }

    #[test]
    fn splits_metadata_and_proc_macro_names() {
        assert_eq!(
            unit_stem(Path::new("/t/deps/libkspin-c32f804f0ee67712.rmeta")),
            Some((
                Path::new("/t/deps").to_path_buf(),
                "kspin-c32f804f0ee67712".to_owned()
            ))
        );
        assert_eq!(
            unit_stem(Path::new("/t/deps/libmacros-5215aeff5168a231.so")).map(|(_, stem)| stem),
            Some("macros-5215aeff5168a231".to_owned())
        );
    }

    #[test]
    fn selects_workspace_units_once() {
        let members: HashSet<String> = [MEMBER.to_owned()].into();
        let messages = format!(
            "not json\n\
             {{\"reason\":\"compiler-artifact\",\"package_id\":\"{MEMBER}\",\
             \"target\":{{\"name\":\"kspin\",\"kind\":[\"lib\"]}},\
             \"filenames\":[\"/t/deps/libkspin-aa.rmeta\"],\"fresh\":true}}\n\
             {{\"reason\":\"compiler-artifact\",\"package_id\":\"{MEMBER}\",\
             \"target\":{{\"name\":\"build-script-build\",\"kind\":[\"custom-build\"]}},\
             \"filenames\":[\"/t/build/kspin-bb/build-script-build\"]}}\n\
             {{\"reason\":\"compiler-artifact\",\"package_id\":\"registry+https://x#log@0.4.28\",\
             \"target\":{{\"name\":\"log\",\"kind\":[\"lib\"]}},\
             \"filenames\":[\"/t/deps/liblog-cc.rmeta\"]}}\n\
             {{\"reason\":\"compiler-artifact\",\"package_id\":\"{MEMBER}\",\
             \"target\":{{\"name\":\"kspin\",\"kind\":[\"lib\"]}},\
             \"filenames\":[\"/t/deps/libkspin-aa.rmeta\"]}}\n\
             {{\"reason\":\"build-finished\",\"success\":true}}\n"
        );
        let units = scan_units(&messages, &members);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].stem, "kspin-aa");
        assert_eq!(units[0].target, "kspin");
        assert_eq!(units[0].package_id, MEMBER);
    }

    #[test]
    fn validates_facts_files() {
        let summary = summarize(facts("kspin-aa").as_bytes(), "kspin-aa").unwrap();
        assert_eq!(summary.crate_name, "kspin");
        assert_eq!(summary.records, 3);
        assert_eq!(summary.counts.get("call_edges"), Some(&3));
        assert!(summarize(facts("kspin-aa").as_bytes(), "kspin-bb").is_err());
        let old = facts("kspin-aa").replace(FACTS_SCHEMA, "xkernel-facts/0.1");
        assert!(summarize(old.as_bytes(), "kspin-aa").is_err());
        let truncated = facts("kspin-aa")
            .lines()
            .take(2)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(summarize(truncated.as_bytes(), "kspin-aa").is_err());
    }

    #[test]
    fn derives_artifact_state() {
        assert_eq!(artifact_state(true, 3, 0), (FactsState::Ready, None));
        assert_eq!(
            artifact_state(true, 3, 1),
            (FactsState::Partial, Some(FailureReason::Exporter))
        );
        assert_eq!(
            artifact_state(false, 2, 0),
            (FactsState::Partial, Some(FailureReason::Build))
        );
        assert_eq!(
            artifact_state(false, 0, 0),
            (FactsState::Failed, Some(FailureReason::Build))
        );
    }

    #[test]
    fn clear_removes_the_previous_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("crates")).unwrap();
        std::fs::write(root.join("crates/kspin-aa.jsonl.zst"), "x").unwrap();
        std::fs::write(root.join("manifest.json"), "{}").unwrap();
        std::fs::write(root.join("status.json"), "{\"status\":\"ready\"}").unwrap();
        std::fs::write(root.join("notes.txt"), "kept").unwrap();
        FactsRun::clear(root).unwrap();
        for stale in ["crates", "manifest.json", "status.json"] {
            assert!(!root.join(stale).exists(), "{stale} left behind");
        }
        assert!(root.join("notes.txt").is_file());
        // A first run has nothing to clear.
        FactsRun::clear(&root.join("absent")).unwrap();
    }

    #[test]
    fn exporter_digest_follows_sources() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/facts_export")).unwrap();
        for (path, content) in [
            ("Cargo.toml", "[package]\n"),
            ("Cargo.lock", "# lock\n"),
            ("rust-toolchain.toml", "[toolchain]\n"),
            ("src/lib.rs", "mod facts_export;\n"),
            ("src/facts_export/mod.rs", "// a\n"),
        ] {
            std::fs::write(root.join(path), content).unwrap();
        }
        let first = exporter_digest(root).unwrap();
        // Untracked build output does not count.
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("target/x"), "x").unwrap();
        assert_eq!(exporter_digest(root).unwrap(), first);
        std::fs::write(root.join("src/facts_export/mod.rs"), "// b\n").unwrap();
        assert_ne!(exporter_digest(root).unwrap(), first);
        std::fs::write(root.join("src/facts_export/mod.rs"), "// a\n").unwrap();
        assert_eq!(exporter_digest(root).unwrap(), first);
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Finalization of metadata embedded in kernel ELF images.

use std::{env, fmt::Write, fs, ops::Range, path::Path, process::Command};

use goblin::{
    container::{Container, Ctx},
    elf::{
        Elf,
        program_header::{PT_LOAD, ProgramHeader},
        section_header::{SHF_ALLOC, SHT_NOTE, SectionHeader},
    },
};
use kernel_image_metadata::{
    BUILD_ID_SECTION_NAME, BUILD_ID_SIZE, BUILD_INFO_DESCRIPTOR_SIZE, BUILD_INFO_NOTE_OWNER,
    BUILD_INFO_NOTE_TYPE, BUILD_INFO_SECTION_NAME, encode_build_info,
};
use scroll::{Pread, Pwrite};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    context::BuildContext,
    error::{Error, IoResultExt, Result},
};

const ELF_NOTE_HEADER_SIZE: usize = 12;
const GNU_BUILD_ID_OWNER: &[u8; 4] = b"GNU\0";
const GNU_BUILD_ID_TYPE: u32 = 3;

/// Semantic build provenance stored in the image and bundle manifest.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) struct BuildInfo {
    #[serde(flatten)]
    provenance: BuildProvenance,
    build_time: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
struct BuildProvenance {
    pub(crate) arch: String,
    pub(crate) platform: String,
    pub(crate) target: String,
    pub(crate) build_mode: String,
    pub(crate) build_machine: String,
    pub(crate) default_log_level: String,
    pub(crate) backtrace: bool,
    pub(crate) nr_cpus_max: usize,
    pub(crate) vmm_enabled: bool,
    pub(crate) config_sha256: String,
    pub(crate) git_commit: String,
    pub(crate) git_dirty: String,
}

#[derive(Clone, Debug)]
pub(crate) struct BuildInfoRequest {
    provenance: BuildProvenance,
    build_time: BuildTimePolicy,
}

#[derive(Clone, Debug)]
enum BuildTimePolicy {
    Automatic,
    Explicit(String),
}

impl BuildInfoRequest {
    pub(crate) fn collect(context: &BuildContext, config_sha256: String) -> Result<Self> {
        let build_machine = single_line("build machine", build_machine())?;
        let (git_commit, git_dirty) = git_provenance(&context.workspace_root);
        Ok(Self {
            provenance: BuildProvenance {
                arch: context.config.arch().as_str().to_string(),
                platform: context.config.platform().to_string(),
                target: context.config.target().to_string(),
                build_mode: context.config.profile().as_str().to_string(),
                build_machine,
                default_log_level: configured_log_level(context).to_string(),
                backtrace: context.config.is_enabled("KFEAT_DWARF"),
                nr_cpus_max: context.config.nr_cpus(),
                vmm_enabled: context.config.is_enabled("KFEAT_VMM"),
                config_sha256,
                git_commit,
                git_dirty,
            },
            build_time: build_time_policy()?,
        })
    }

    pub(crate) fn materialize(&self) -> Result<BuildInfo> {
        Ok(BuildInfo {
            provenance: self.provenance.clone(),
            build_time: self.build_time.resolve()?,
        })
    }

    pub(crate) fn matches(&self, build_info: &BuildInfo) -> bool {
        self.provenance == build_info.provenance && self.build_time.matches(&build_info.build_time)
    }
}

impl BuildInfo {
    pub(crate) fn payload(&self) -> String {
        let mut payload = String::new();
        writeln!(payload, "arch = {}", self.provenance.arch).unwrap();
        writeln!(payload, "platform = {}", self.provenance.platform).unwrap();
        writeln!(payload, "target = {}", self.provenance.target).unwrap();
        writeln!(payload, "build_mode = {}", self.provenance.build_mode).unwrap();
        writeln!(payload, "build_machine = {}", self.provenance.build_machine).unwrap();
        writeln!(payload, "build_time = {}", self.build_time).unwrap();
        writeln!(payload, "log_level = {}", self.provenance.default_log_level).unwrap();
        writeln!(payload, "backtrace = {}", self.provenance.backtrace).unwrap();
        writeln!(payload, "nr_cpus_max = {}", self.provenance.nr_cpus_max).unwrap();
        writeln!(payload, "vmm = {}", on_off(self.provenance.vmm_enabled)).unwrap();
        writeln!(payload, "config_sha256 = {}", self.provenance.config_sha256).unwrap();
        writeln!(payload, "git_commit = {}", self.provenance.git_commit).unwrap();
        writeln!(payload, "git_dirty = {}", self.provenance.git_dirty).unwrap();
        payload
    }
}

struct NoteLayout {
    section_start: usize,
    descriptor: Range<usize>,
    section_header: SectionHeader,
    section_header_offset: usize,
}

/// Writes build information and the final SHA-256 GNU build ID into `path`.
pub(crate) fn finalize(path: &Path, build_info: &BuildInfo) -> Result<String> {
    let mut bytes = fs::read(path).with_path(path)?;
    let (build_info_note, build_id_note, program_headers, context) = {
        let elf = Elf::parse(&bytes).map_err(|error| metadata_error(path, error))?;
        let context = elf_context(&elf).map_err(|error| metadata_error(path, error))?;
        let build_info_note = note_layout(
            &bytes,
            &elf,
            BUILD_INFO_SECTION_NAME,
            BUILD_INFO_NOTE_OWNER,
            BUILD_INFO_NOTE_TYPE,
            BUILD_INFO_DESCRIPTOR_SIZE,
            BUILD_INFO_DESCRIPTOR_SIZE,
        )
        .map_err(|error| metadata_error(path, error))?;
        let build_id_note = note_layout(
            &bytes,
            &elf,
            BUILD_ID_SECTION_NAME,
            GNU_BUILD_ID_OWNER,
            GNU_BUILD_ID_TYPE,
            BUILD_ID_SIZE,
            BUILD_ID_SIZE,
        )
        .map_err(|error| metadata_error(path, error))?;
        (
            build_info_note,
            build_id_note,
            elf.program_headers.clone(),
            context,
        )
    };

    let encoded_build_info_size = encode_build_info(
        &build_info.payload(),
        &mut bytes[build_info_note.descriptor.clone()],
    )
    .map_err(|error| metadata_error(path, error))?;
    finalize_note_header(
        &mut bytes,
        &build_info_note,
        encoded_build_info_size,
        context,
    )
    .map_err(|error| metadata_error(path, error))?;
    finalize_note_header(&mut bytes, &build_id_note, BUILD_ID_SIZE, context)
        .map_err(|error| metadata_error(path, error))?;

    bytes[build_id_note.descriptor.clone()].fill(0);
    let build_id = hash_load_segments(&bytes, &program_headers)
        .map_err(|error| metadata_error(path, error))?;
    bytes[build_id_note.descriptor].copy_from_slice(&build_id);
    fs::write(path, bytes).with_path(path)?;
    Ok(hex(&build_id))
}

fn note_layout(
    bytes: &[u8],
    elf: &Elf<'_>,
    section_name: &str,
    expected_owner: &[u8],
    expected_type: u32,
    minimum_descriptor_size: usize,
    maximum_descriptor_size: usize,
) -> std::result::Result<NoteLayout, String> {
    let (section_index, section) = elf
        .section_headers
        .iter()
        .enumerate()
        .find(|(_, section)| elf.shdr_strtab.get_at(section.sh_name) == Some(section_name))
        .ok_or_else(|| format!("missing ELF section `{section_name}`"))?;
    if section.sh_flags & u64::from(SHF_ALLOC) == 0 {
        return Err(format!("ELF section `{section_name}` is not allocated"));
    }
    let section_range = file_range(
        bytes.len(),
        section.sh_offset,
        section.sh_size,
        section_name,
    )?;
    validate_loaded_section(section, &section_range, &elf.program_headers, section_name)?;

    let endian = elf.header.endianness().map_err(|error| error.to_string())?;
    let namesz = bytes
        .pread_with::<u32>(section_range.start, endian)
        .map_err(|error| error.to_string())? as usize;
    let descsz = bytes
        .pread_with::<u32>(section_range.start + 4, endian)
        .map_err(|error| error.to_string())? as usize;
    let note_type = bytes
        .pread_with::<u32>(section_range.start + 8, endian)
        .map_err(|error| error.to_string())?;
    if namesz != expected_owner.len()
        || !(minimum_descriptor_size..=maximum_descriptor_size).contains(&descsz)
        || note_type != expected_type
    {
        return Err(format!("ELF note header for `{section_name}` is invalid"));
    }
    let owner_start = section_range.start + ELF_NOTE_HEADER_SIZE;
    let owner_end = owner_start
        .checked_add(namesz)
        .ok_or_else(|| format!("ELF note owner range for `{section_name}` overflows"))?;
    if bytes.get(owner_start..owner_end) != Some(expected_owner) {
        return Err(format!("ELF note owner for `{section_name}` is invalid"));
    }
    let descriptor_start = align_up(owner_end, 4)
        .ok_or_else(|| format!("ELF note descriptor for `{section_name}` overflows"))?;
    let descriptor_end = descriptor_start
        .checked_add(descsz)
        .ok_or_else(|| format!("ELF note descriptor for `{section_name}` overflows"))?;
    if descriptor_end > section_range.end {
        return Err(format!(
            "ELF note descriptor for `{section_name}` is out of bounds"
        ));
    }
    let reserved_descriptor_end = descriptor_start
        .checked_add(maximum_descriptor_size)
        .ok_or_else(|| format!("ELF note reserve for `{section_name}` overflows"))?;
    if reserved_descriptor_end > bytes.len() {
        return Err(format!(
            "ELF note reserve for `{section_name}` is out of file bounds"
        ));
    }
    let descriptor_address = section
        .sh_addr
        .checked_add((descriptor_start - section_range.start) as u64)
        .ok_or_else(|| format!("ELF note address for `{section_name}` overflows"))?;
    validate_loaded_range(
        descriptor_start..reserved_descriptor_end,
        descriptor_address,
        maximum_descriptor_size as u64,
        &elf.program_headers,
        section_name,
    )?;
    let section_header_offset = (elf.header.e_shoff as usize)
        .checked_add(section_index * usize::from(elf.header.e_shentsize))
        .ok_or_else(|| "ELF section-header offset overflows".to_string())?;
    Ok(NoteLayout {
        section_start: section_range.start,
        descriptor: descriptor_start..descriptor_end,
        section_header: section.clone(),
        section_header_offset,
    })
}

fn validate_loaded_section(
    section: &SectionHeader,
    section_range: &Range<usize>,
    program_headers: &[ProgramHeader],
    section_name: &str,
) -> std::result::Result<(), String> {
    let section_memory_end = section
        .sh_addr
        .checked_add(section.sh_size)
        .ok_or_else(|| format!("ELF section `{section_name}` memory range overflows"))?;
    let is_loaded = program_headers.iter().any(|segment| {
        if segment.p_type != PT_LOAD {
            return false;
        }
        let Some(file_end) = segment.p_offset.checked_add(segment.p_filesz) else {
            return false;
        };
        let Some(memory_end) = segment.p_vaddr.checked_add(segment.p_memsz) else {
            return false;
        };
        section_range.start >= segment.p_offset as usize
            && section_range.end <= file_end as usize
            && section.sh_addr >= segment.p_vaddr
            && section_memory_end <= memory_end
    });
    if !is_loaded {
        return Err(format!(
            "ELF section `{section_name}` is not inside a PT_LOAD segment"
        ));
    }
    Ok(())
}

fn validate_loaded_range(
    file_range: Range<usize>,
    virtual_address: u64,
    memory_size: u64,
    program_headers: &[ProgramHeader],
    description: &str,
) -> std::result::Result<(), String> {
    let virtual_end = virtual_address
        .checked_add(memory_size)
        .ok_or_else(|| format!("ELF range `{description}` memory range overflows"))?;
    let is_loaded = program_headers.iter().any(|segment| {
        if segment.p_type != PT_LOAD {
            return false;
        }
        let Some(segment_file_end) = segment.p_offset.checked_add(segment.p_filesz) else {
            return false;
        };
        let Some(segment_memory_end) = segment.p_vaddr.checked_add(segment.p_memsz) else {
            return false;
        };
        file_range.start >= segment.p_offset as usize
            && file_range.end <= segment_file_end as usize
            && virtual_address >= segment.p_vaddr
            && virtual_end <= segment_memory_end
    });
    if !is_loaded {
        return Err(format!(
            "ELF range `{description}` is not inside a PT_LOAD segment"
        ));
    }
    Ok(())
}

fn finalize_note_header(
    bytes: &mut [u8],
    layout: &NoteLayout,
    descriptor_size: usize,
    context: Ctx,
) -> std::result::Result<(), String> {
    let descriptor_size_u32: u32 = descriptor_size
        .try_into()
        .map_err(|_| "ELF note descriptor size does not fit u32".to_string())?;
    bytes
        .pwrite_with(descriptor_size_u32, layout.section_start + 4, context.le)
        .map_err(|error| error.to_string())?;
    let descriptor_offset = layout
        .descriptor
        .start
        .checked_sub(layout.section_start)
        .ok_or_else(|| "ELF note descriptor precedes its section".to_string())?;
    let visible_size = descriptor_offset
        .checked_add(
            align_up(descriptor_size, 4)
                .ok_or_else(|| "ELF note descriptor alignment overflows".to_string())?,
        )
        .ok_or_else(|| "ELF note section size overflows".to_string())?;
    let mut section = layout.section_header.clone();
    section.sh_type = SHT_NOTE;
    section.sh_size = visible_size as u64;
    bytes
        .pwrite_with(section, layout.section_header_offset, context)
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn hash_load_segments(
    bytes: &[u8],
    program_headers: &[ProgramHeader],
) -> std::result::Result<[u8; BUILD_ID_SIZE], String> {
    let mut hasher = Sha256::new();
    for segment in program_headers
        .iter()
        .filter(|segment| segment.p_type == PT_LOAD)
    {
        let range = file_range(bytes.len(), segment.p_offset, segment.p_filesz, "PT_LOAD")?;
        hasher.update(segment.p_vaddr.to_le_bytes());
        hasher.update(segment.p_filesz.to_le_bytes());
        hasher.update(&bytes[range]);
    }
    Ok(hasher.finalize().into())
}

fn file_range(
    file_len: usize,
    offset: u64,
    size: u64,
    description: &str,
) -> std::result::Result<Range<usize>, String> {
    let start: usize = offset
        .try_into()
        .map_err(|_| format!("{description} offset does not fit usize"))?;
    let size: usize = size
        .try_into()
        .map_err(|_| format!("{description} size does not fit usize"))?;
    let end = start
        .checked_add(size)
        .ok_or_else(|| format!("{description} file range overflows"))?;
    if end > file_len {
        return Err(format!("{description} file range is out of bounds"));
    }
    Ok(start..end)
}

fn elf_context(elf: &Elf<'_>) -> std::result::Result<Ctx, String> {
    let container = match elf.header.container().map_err(|error| error.to_string())? {
        Container::Big => Container::Big,
        Container::Little => return Err("only ELF64 kernel images are supported".to_string()),
    };
    let endian = elf.header.endianness().map_err(|error| error.to_string())?;
    Ok(Ctx::new(container, endian))
}

fn align_up(value: usize, alignment: usize) -> Option<usize> {
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
}

fn metadata_error(path: &Path, error: impl std::fmt::Display) -> Error {
    Error::Message(format!(
        "failed to finalize image metadata in {}: {error}",
        path.display()
    ))
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(output, "{byte:02x}").unwrap();
    }
    output
}

fn configured_log_level(context: &BuildContext) -> &'static str {
    [
        ("LOG_LEVEL_ERROR", "error"),
        ("LOG_LEVEL_WARN", "warn"),
        ("LOG_LEVEL_INFO", "info"),
        ("LOG_LEVEL_DEBUG", "debug"),
        ("LOG_LEVEL_TRACE", "trace"),
    ]
    .into_iter()
    .find_map(|(symbol, level)| context.config.is_enabled(symbol).then_some(level))
    .unwrap_or("off")
}

fn build_machine() -> String {
    if let Ok(value) = env::var("KBUILD_BUILD_MACHINE") {
        return value;
    }
    let user = env::var("USER").unwrap_or_else(|_| "unknown".to_string());
    let host = env::var("HOSTNAME")
        .ok()
        .or_else(command_hostname)
        .unwrap_or_else(|| "unknown".to_string());
    format!("{user}@{host}")
}

fn command_hostname() -> Option<String> {
    let output = Command::new("hostname").output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

impl BuildTimePolicy {
    fn resolve(&self) -> Result<String> {
        match self {
            Self::Automatic => format_build_time(OffsetDateTime::now_utc()),
            Self::Explicit(value) => Ok(value.clone()),
        }
    }

    fn matches(&self, build_time: &str) -> bool {
        match self {
            Self::Automatic => OffsetDateTime::parse(build_time, &Rfc3339).is_ok(),
            Self::Explicit(expected) => expected == build_time,
        }
    }
}

fn build_time_policy() -> Result<BuildTimePolicy> {
    if let Ok(value) = env::var("KBUILD_BUILD_TIME") {
        return Ok(BuildTimePolicy::Explicit(single_line("build time", value)?));
    }
    if let Ok(value) = env::var("SOURCE_DATE_EPOCH") {
        let seconds = value.parse::<i64>().map_err(|error| {
            Error::Message(format!(
                "SOURCE_DATE_EPOCH is not a valid Unix timestamp: {error}"
            ))
        })?;
        let timestamp = OffsetDateTime::from_unix_timestamp(seconds).map_err(|error| {
            Error::Message(format!(
                "SOURCE_DATE_EPOCH is outside the supported range: {error}"
            ))
        })?;
        return Ok(BuildTimePolicy::Explicit(format_build_time(timestamp)?));
    }
    Ok(BuildTimePolicy::Automatic)
}

fn format_build_time(timestamp: OffsetDateTime) -> Result<String> {
    timestamp
        .format(&Rfc3339)
        .map_err(|error| Error::Message(format!("failed to format build time: {error}")))
}

fn git_provenance(workspace_root: &Path) -> (String, String) {
    let commit =
        git_output(workspace_root, &["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let dirty = Command::new("git")
        .current_dir(workspace_root)
        .args(["status", "--porcelain=v1", "--untracked-files=normal"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| {
            if output.stdout.is_empty() {
                "false"
            } else {
                "true"
            }
        })
        .unwrap_or("unknown")
        .to_string();
    (commit, dirty)
}

fn git_output(workspace_root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .current_dir(workspace_root)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn single_line(field: &str, value: String) -> Result<String> {
    if value.chars().any(|character| character.is_control()) {
        return Err(Error::Message(format!(
            "{field} contains control characters"
        )));
    }
    Ok(value)
}

const fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_multiline_provenance() {
        assert!(single_line("build machine", "builder\nhost".to_string()).is_err());
    }

    #[test]
    fn alignment_is_checked() {
        assert_eq!(align_up(17, 4), Some(20));
        assert_eq!(align_up(usize::MAX, 4), None);
    }

    #[test]
    fn automatic_build_time_does_not_invalidate_existing_metadata() {
        assert!(BuildTimePolicy::Automatic.matches("2026-07-28T12:00:00Z"));
        assert!(!BuildTimePolicy::Automatic.matches("unknown"));
    }

    #[test]
    fn explicit_build_time_is_a_build_input() {
        let policy = BuildTimePolicy::Explicit("2026-07-28T12:00:00Z".to_string());
        assert!(policy.matches("2026-07-28T12:00:00Z"));
        assert!(!policy.matches("2026-07-28T12:00:01Z"));
    }

    #[test]
    fn formats_build_time_as_rfc3339() {
        let timestamp = OffsetDateTime::from_unix_timestamp(0)
            .expect("the Unix epoch must be a supported timestamp");
        assert_eq!(
            format_build_time(timestamp).unwrap(),
            "1970-01-01T00:00:00Z"
        );
    }
}

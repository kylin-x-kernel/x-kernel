// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;

use alloc::{
    string::{String, ToString},
    vec::Vec,
};

use fs_context::init_fs;
use hashbrown::HashMap;
use kerrno::{KError, KResult};
use kvfs::{Filename, NodePermission};
use tee_raw_sys::ta_head;
use uuid as uuid_crate;

/// Length of a TA basename `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx.ta` (hyphenated UUID + `.ta`).
const TA_UUID_DOT_TA_EXAMPLE: &str = "936da01f-9abd-4d9d-80c7-02af85c822a8.ta";

/// Canonical install directory for signed TA ELF images.
const TA_INSTALL_ROOT: &str = "/tee/ta/";

/// Normalize an absolute path and reject traversal or non-canonical components.
fn normalize_absolute_path(path: &str) -> Option<String> {
    if path.is_empty() || path.as_bytes().contains(&0) || !path.starts_with('/') {
        return None;
    }

    let mut parts = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => return None,
            part => parts.push(part),
        }
    }

    let mut normalized = String::from("/");
    normalized.push_str(&parts.join("/"));
    Some(normalized)
}

fn is_valid_ta_basename(name: &str) -> bool {
    if name.len() != TA_UUID_DOT_TA_EXAMPLE.len() || !name.ends_with(".ta") {
        return false;
    }
    let Some(stem) = name.strip_suffix(".ta") else {
        return false;
    };
    uuid_crate::Uuid::parse_str(stem).is_ok()
}

/// Return the canonical `/tee/ta/{uuid}.ta` path when `path` names an installed TA.
fn canonical_ta_install_path(path: &str) -> Option<String> {
    let normalized = normalize_absolute_path(path)?;
    if !normalized.starts_with(TA_INSTALL_ROOT) {
        return None;
    }
    let basename = normalized.strip_prefix(TA_INSTALL_ROOT)?;
    if basename.contains('/') || !is_valid_ta_basename(basename) {
        return None;
    }
    Some(normalized)
}

/// True when `path` is an absolute `{uuid}.ta` basename with no `..` components.
///
/// Used to decide whether exec must run TA ELF signature verification. Paths
/// containing `..` are rejected (same as [`TeeTaCtx::is_ta`]). TEE runtime
/// context still requires a canonical `/tee/ta/` install path.
pub fn looks_like_ta(path: &str) -> bool {
    let Some(normalized) = normalize_absolute_path(path) else {
        return false;
    };
    let basename = match normalized.rsplit('/').next() {
        Some(name) if !name.is_empty() => name,
        _ => return false,
    };
    is_valid_ta_basename(basename)
}

/// Identity of a manager-owned inter-TA session, stored in `TeeTaCtx.open_sessions`.
///
/// The map key is the **process-local session handle** returned to the caller
/// (plan 004 §6.2): the manager's global session id never crosses the kernel
/// boundary, so a caller cannot guess or probe another process's sessions.
#[derive(Clone, Debug)]
pub struct SessionIdentity {
    /// Target TA UUID (hyphenated lowercase form, as routed on the wire).
    pub target_uuid: String,
    /// Manager-side global session id; opaque to user space.
    pub manager_session_id: u32,
}

/// Global TA context shared across all sessions of one TA instance.
#[derive(Debug)]
pub struct TeeTaCtx {
    /// Next candidate for local session-handle allocation (plan 004 §6.2).
    /// Starts at 1, skips 0 forever, wraps at `u32::MAX` back to 1.
    pub next_session_handle: u32,
    pub open_sessions: HashMap<u32, SessionIdentity>,
    /// A Close transport failure invalidated a handle without confirming remote cleanup.
    owner_release_required: bool,
    pub uuid: String,
    pub ta_head: ta_head,
}

impl Default for TeeTaCtx {
    fn default() -> Self {
        TeeTaCtx {
            next_session_handle: 1,
            open_sessions: HashMap::new(),
            owner_release_required: false,
            uuid: uuid_crate::Uuid::default().to_string(),
            ta_head: ta_head::default(),
        }
    }
}

/// Parse `.ta_head` section bytes from an in-memory ELF image.
pub fn read_ta_head_from_image(image: &[u8]) -> KResult<Option<Vec<u8>>> {
    let elf = xmas_elf::ElfFile::new(image).map_err(|_| KError::InvalidExecutable)?;
    let section = elf.find_section_by_name(".ta_head");
    let Some(section) = section else {
        return Ok(None);
    };
    let offset = usize::try_from(section.offset()).map_err(|_| KError::InvalidData)?;
    let size = usize::try_from(section.size()).map_err(|_| KError::InvalidData)?;
    let end = offset.checked_add(size).ok_or(KError::InvalidData)?;
    if end > image.len() {
        return Err(KError::InvalidData);
    }
    if size != core::mem::size_of::<ta_head>() {
        return Err(KError::InvalidData);
    }
    Ok(Some(image[offset..end].to_vec()))
}

/// When `path` names a TA, read the ELF through the opened file and return raw `.ta_head` bytes (no signature check).
pub fn read_ta_head_if_applicable(path: &str) -> KResult<Option<Vec<u8>>> {
    let Some(normalized) = canonical_ta_install_path(path) else {
        return Ok(None);
    };
    let fs_guard = init_fs();
    let fs = fs_guard.lock();
    let file = Filename::new(normalized.as_str()).open_with_flags_at(
        fs.root(),
        fs.pwd(),
        0,
        NodePermission::empty(),
        NodePermission::empty(),
        kcred::initial_cred(),
    )?;
    drop(fs);
    let len = file.size();
    let len = usize::try_from(len).map_err(|_| KError::InvalidData)?;
    let mut image = alloc::vec![0u8; len];
    let mut pos = 0;
    let n = file
        .read_from(&mut image[..], &mut pos)
        .map_err(|_| KError::InvalidData)?;
    if n != len {
        return Err(KError::InvalidData);
    }
    read_ta_head_from_image(&image)
}

pub fn bytes_to_ta_head(data: &[u8]) -> KResult<ta_head> {
    if data.len() != core::mem::size_of::<ta_head>() {
        return Err(KError::InvalidData);
    }
    Ok(bytemuck::pod_read_unaligned(data))
}

/// Returns the next handle candidate after `handle` per §6.2: 0 is skipped
/// forever and `u32::MAX` wraps to 1.
fn advance_handle_candidate(handle: u32) -> u32 {
    match handle.checked_add(1) {
        // 0 is never a legal handle, and the overflow past u32::MAX wraps.
        Some(0) | None => 1,
        Some(next) => next,
    }
}

/// Scans for a free local session handle starting at `start` (§6.2).
///
/// `occupied` is the number of live entries; the probe budget is
/// `occupied + 1` so a full table returns `None` without scanning the whole
/// `u32` space. Probes treat 0 as occupied regardless of the map.
fn next_free_handle(start: u32, occupied: u32, is_occupied: impl Fn(u32) -> bool) -> Option<u32> {
    let mut candidate = start;
    for _ in 0..occupied.saturating_add(1) {
        let probed = candidate;
        candidate = advance_handle_candidate(candidate);
        if probed != 0 && !is_occupied(probed) {
            return Some(probed);
        }
    }
    None
}

impl TeeTaCtx {
    /// Inserts a session and returns its new process-local handle.
    ///
    /// Plan 004 §6.2 handle rules: never 0, scan from `next_session_handle`
    /// for a free slot, wrap at `u32::MAX` to 1, and return `None` (caller
    /// maps to `TEE_ERROR_OUT_OF_MEMORY`) only when no slot is free. The
    /// pigeonhole bound keeps the scan at `entries + 1` probes: with N
    /// occupied slots, N+1 distinct candidates must contain a free one.
    pub fn insert_session(&mut self, target_uuid: String, manager_session_id: u32) -> Option<u32> {
        let handle = next_free_handle(
            self.next_session_handle,
            self.open_sessions.len() as u32,
            |h| self.open_sessions.contains_key(&h),
        )?;
        self.next_session_handle = advance_handle_candidate(handle);
        self.open_sessions.insert(
            handle,
            SessionIdentity {
                target_uuid,
                manager_session_id,
            },
        );
        Some(handle)
    }

    /// Looks up a session by process-local handle.
    pub fn session(&self, handle: u32) -> Option<&SessionIdentity> {
        self.open_sessions.get(&handle)
    }

    /// Removes a session by process-local handle, returning it.
    pub fn remove_session(&mut self, handle: u32) -> Option<SessionIdentity> {
        self.open_sessions.remove(&handle)
    }

    /// True when at least one inter-TA session is open.
    ///
    /// The process-exit release path uses this to skip control-plane I/O for
    /// processes with nothing to release (plan 004 §8.2).
    pub fn has_open_sessions(&self) -> bool {
        !self.open_sessions.is_empty()
    }

    /// Remembers uncertain remote state after invalidating a local handle.
    ///
    /// ReleaseOwner addresses the process, so retaining individual remote IDs is
    /// unnecessary. This flag never makes an invalid handle usable again.
    pub fn require_owner_release(&mut self) {
        self.owner_release_required = true;
    }

    /// Whether process exit must contact the manager, including uncertain Close results.
    pub fn needs_owner_release(&self) -> bool {
        self.has_open_sessions() || self.owner_release_required
    }

    /// Drops all inter-TA sessions and resets handle allocation.
    ///
    /// Process-exit cleanup (plan 004 §8.2): manager-side close is best
    /// effort, and no local state may survive the process.
    pub fn clear_sessions(&mut self) {
        self.open_sessions.clear();
        self.owner_release_required = false;
        self.next_session_handle = 1;
    }

    pub fn set_uuid(&mut self, path: &str) {
        let Some(normalized) = canonical_ta_install_path(path) else {
            log::debug!("set_uuid: rejected non-canonical TA path: {path}");
            return;
        };
        let uuid = match normalized
            .rsplit('/')
            .next()
            .and_then(|name| name.strip_suffix(".ta"))
        {
            Some(v) => v,
            None => return,
        };

        if uuid_crate::Uuid::parse_str(uuid).is_ok() {
            self.uuid = uuid.to_string();
        }
    }

    /// True when `path` canonicalizes to `/tee/ta/{uuid}.ta` with a hyphenated UUID basename.
    ///
    /// Used for TEE runtime context (UUID, `ta_head`, storage namespace). Exec-time
    /// signature verification uses [`looks_like_ta`] instead.
    pub fn is_ta(path: &str) -> bool {
        canonical_ta_install_path(path).is_some()
    }

    /// Alias for [`looks_like_ta`].
    pub fn looks_like_ta(path: &str) -> bool {
        super::looks_like_ta(path)
    }

    pub fn new(path: &str) -> Self {
        let mut ctx = Self::default();
        ctx.set_uuid(path);
        ctx
    }

    pub fn init_ta_ctx(&mut self, path: &str, ta_head: &[u8]) {
        if Self::is_ta(path) {
            self.set_uuid(path);
            if !ta_head.is_empty() {
                match bytes_to_ta_head(ta_head) {
                    Ok(head) => {
                        self.ta_head = head;
                        log::info!("ta_head: {:X?}", self.ta_head);
                    }
                    Err(err) => {
                        log::warn!("parse ta_head failed: {:?}", err);
                    }
                }
            }
        }
    }
}

// Guest coverage for TA identity, header parsing, and session lifecycle.
unittest::path_mod_test!(pub mod tests_ta_ctx, "../ut-tests/ta_ctx/tests_ta_ctx.rs");

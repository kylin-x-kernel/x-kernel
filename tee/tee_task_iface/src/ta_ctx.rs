// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;

use alloc::{
    string::{String, ToString},
    vec::Vec,
};

use hashbrown::HashMap;
use kerrno::{KError, KResult};
use kfs::{CachedFile, kernel_fs_context};
use tee_raw_sys::ta_head;
use uuid as uuid_crate;

/// Length of a TA basename `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx.ta` (hyphenated UUID + `.ta`).
const TA_UUID_DOT_TA_EXAMPLE: &str = "936da01f-9abd-4d9d-80c7-02af85c822a8.ta";

/// Identity of a TA session, stored in `TeeTaCtx.open_sessions`.
#[derive(Debug, Clone)]
pub struct SessionIdentity {
    pub uuid: String,
    pub session_id: u32,
}

/// Global TA context shared across all sessions of one TA instance.
#[derive(Debug)]
pub struct TeeTaCtx {
    pub session_dispatch_irq: u32,
    pub open_sessions: HashMap<u32, SessionIdentity>,
    pub uuid: String,
    pub ta_head: ta_head,
}

impl Default for TeeTaCtx {
    fn default() -> Self {
        TeeTaCtx {
            session_dispatch_irq: 0,
            open_sessions: HashMap::new(),
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

/// When `path` names a TA, read the ELF via `CachedFile` and return raw `.ta_head` bytes (no signature check).
pub fn read_ta_head_if_applicable(path: &str) -> KResult<Option<Vec<u8>>> {
    if !TeeTaCtx::is_ta(path) {
        return Ok(None);
    }
    let loc = kernel_fs_context().lock().resolve(path)?;
    let cache = CachedFile::get_or_create(loc)?;
    let len = cache.location().len().map_err(|_| KError::InvalidData)?;
    let len = usize::try_from(len).map_err(|_| KError::InvalidData)?;
    let mut image = alloc::vec![0u8; len];
    let n = cache
        .read_at(&mut image[..], 0)
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

impl TeeTaCtx {
    pub fn set_uuid(&mut self, path: &str) {
        // get the path basic string
        let uuid = match path
            .rsplit('/')
            .next()
            .and_then(|name| name.rsplit_once('.').map(|(base, _)| base))
        {
            Some(v) => v,
            None => return,
        };

        if uuid_crate::Uuid::parse_str(uuid).is_ok() {
            self.uuid = uuid.to_string();
        }
    }

    /// True when the path's final component is `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx.ta`
    /// (same character length as `936da01f-9abd-4d9d-80c7-02af85c822a8.ta`, hyphenated UUID) — not arbitrary `*.ta` names.
    pub fn is_ta(path: &str) -> bool {
        let Some(name) = path.rsplit('/').next() else {
            return false;
        };
        if name.len() != TA_UUID_DOT_TA_EXAMPLE.len() || !name.ends_with(".ta") {
            return false;
        }
        let Some(stem) = name.strip_suffix(".ta") else {
            return false;
        };
        uuid_crate::Uuid::parse_str(stem).is_ok()
    }

    pub fn new(path: &str) -> Self {
        let mut ctx = Self::default();
        ctx.set_uuid(path);
        ctx
    }

    #[cfg(feature = "tee_ta_sign")]
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

// Test module for TEE session functionality
// Only compiled when the tee_test feature is enabled
#[unittest::mod_test]
pub mod tests_ta_ctx {
    use unittest::{assert, assert_eq};

    use super::*;

    #[unittest::def_test]
    fn test_bytes_to_ta_head() {
        let ta_head = ta_head {
            uuid: Default::default(),
            stack_size: 1024,
            flags: 1,
            depr_entry: u64::MAX,
        };
        let data = bytemuck::bytes_of(&ta_head);
        let ta_head_from_bytes = bytes_to_ta_head(data).unwrap();
        assert_eq!(ta_head_from_bytes, ta_head);
    }

    // Test function for basic ta_ctx operations
    #[unittest::def_test]
    fn test_ta_ctx() {
        let mut ta_ctx = TeeTaCtx::default();
        ta_ctx.set_uuid("/tee/ta/936da01f-9abd-4d9d-80c7-02af85c822a8.ta");
        assert_eq!(ta_ctx.uuid, "936da01f-9abd-4d9d-80c7-02af85c822a8");
        ta_ctx.uuid.clear();
        assert!(ta_ctx.uuid.is_empty());
        ta_ctx.set_uuid("/tee/ta/936da01f-9abd-4d9d-80c7-02af85c822a.ta");
        assert!(ta_ctx.uuid.is_empty());
        ta_ctx.set_uuid("/tee/ta/936da01f-9abd-4d9d-80c7-02af85c822a8");
        assert!(ta_ctx.uuid.is_empty());
        ta_ctx.set_uuid("/tee/ta/936DA01F-9ABD-4D9D-80C7-02AF85C822A8.ta");
        assert_eq!(ta_ctx.uuid, "936DA01F-9ABD-4D9D-80C7-02AF85C822A8");

        assert!(TeeTaCtx::is_ta(
            "/tee/ta/936da01f-9abd-4d9d-80c7-02af85c822a8.ta"
        ));
        assert!(TeeTaCtx::is_ta(
            "/tee/ta/936DA01F-9ABD-4D9D-80C7-02AF85C822A8.ta"
        ));
        assert!(!TeeTaCtx::is_ta("/tee/ta/foo.ta"));
        assert!(!TeeTaCtx::is_ta(
            "/tee/ta/936da01f9abd4d9d80c702af85c822a8.ta"
        ));
    }
}

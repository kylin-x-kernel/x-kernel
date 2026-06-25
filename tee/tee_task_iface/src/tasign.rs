// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Optional verification of signed TA ELF images when loading from the VFS.

use alloc::{collections::VecDeque, string::String, vec::Vec};
use core::convert::AsRef;

use hashbrown::HashMap;
use kerrno::{KError, KResult};
use kfs::{File, OpenOptions, kernel_fs_context};
use klazy::Lazy;
use ksync::Mutex;
use kvfs::{LookupFlags, LookupIntent, lookup_location};
use log::{error, info};

use crate::ta_ctx::{self, looks_like_ta};

const TA_HEAD_FIFO_CAP: usize = 32;
#[cfg(feature = "ta_verify_with_root")]
const TA_VERIFY_CA_PEM: &[u8] = include_bytes!("../certs/tee-ta-sign-root.pem");

struct TaHeadFifoCache {
    map: HashMap<String, Option<Vec<u8>>>,
    order: VecDeque<String>,
}

impl TaHeadFifoCache {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&self, key: &str) -> Option<Option<Vec<u8>>> {
        self.map.get(key).cloned()
    }

    fn insert(&mut self, key: String, value: Option<Vec<u8>>) {
        if self.map.contains_key(&key) {
            self.map.insert(key, value);
            return;
        }
        if self.order.len() >= TA_HEAD_FIFO_CAP
            && let Some(old) = self.order.pop_front()
        {
            self.map.remove(&old);
        }
        self.order.push_back(key.clone());
        self.map.insert(key, value);
    }

    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }
}

static TA_HEAD_CACHE: Lazy<Mutex<TaHeadFifoCache>> =
    Lazy::new(|| Mutex::new(TaHeadFifoCache::new()));

/// When `exec_path` ends with `{uuid}.ta`, read the full image and verify `.ta_signature`.
///
/// Returns the raw `.ta_head` section bytes on success; callers persist that value in the global FIFO
/// map (`verify_ta_elf_on_load_and_cache_ta_head` / `get_ta_head_cached`). Non-TA-shaped paths return
/// `Ok(None)` without error. TEE context binding still requires a canonical `/tee/ta/` path via
/// [`crate::TeeTaCtx::is_ta`].
pub fn verify_ta_elf_signature_if_applicable(
    exec_path: &str,
    file: &File,
) -> KResult<Option<Vec<u8>>> {
    if !looks_like_ta(exec_path) {
        return Ok(None);
    }
    let len = file.location().len().map_err(|_| KError::InvalidData)?;
    let len = usize::try_from(len).map_err(|_| KError::InvalidData)?;
    let mut image = alloc::vec![0u8; len];
    let n = file
        .read_at(&mut image[..], 0)
        .map_err(|_| KError::InvalidData)?;
    if n != len {
        return Err(KError::InvalidData);
    }
    info!(
        "verify_elf_signature_with_limits: image.len: {}",
        image.len()
    );
    cfg_if::cfg_if! {
        if #[cfg(feature = "ta_verify_with_root")] {
            let ca_pem = Some(TA_VERIFY_CA_PEM);
        } else {
            let ca_pem = None;
        }
    }
    tasign::verify_elf_signature(image.as_slice(), ca_pem).map_err(|e| {
        error!("verify ta elf signature failed: {e}");
        KError::InvalidExecutable
    })?;
    ta_ctx::read_ta_head_from_image(image.as_slice())
}

/// Clears the bounded FIFO cache of `.ta_head` bytes (pair with ELF loader cache flush).
pub fn clear_ta_head_cache() {
    TA_HEAD_CACHE.lock().clear();
}

/// After a successful ELF parse in the loader: **verify** TA ELF signature (primary), then store
/// the returned `ta_head` in the FIFO map (secondary; same moment as the former `ElfCacheEntry.ta_head` field).
pub fn verify_ta_elf_on_load_and_cache_ta_head(file: &File) -> KResult<()> {
    let abs = file.location().absolute_path()?;
    let key: String = String::from(AsRef::<str>::as_ref(&abs));
    let ta_head = verify_ta_elf_signature_if_applicable(key.as_str(), file)?;
    TA_HEAD_CACHE.lock().insert(key, ta_head);
    Ok(())
}

/// Cache lookup by canonical absolute path; on miss, verifies the image and
/// stores the resulting `ta_head`.
///
/// Callers should pass the resolved absolute executable path rather than the
/// raw user-provided exec path so lookup stays independent of per-process cwd
/// or chroot state.
pub fn get_ta_head_cached(path: &str) -> KResult<Option<Vec<u8>>> {
    let fs = kernel_fs_context().lock();
    let loc = lookup_location(
        &fs.lookup_context(),
        path,
        LookupIntent::Open,
        LookupFlags::follow(),
    )?;
    drop(fs);
    let abs = loc.absolute_path()?;
    let key: String = String::from(AsRef::<str>::as_ref(&abs));

    {
        let guard = TA_HEAD_CACHE.lock();
        if let Some(hit) = guard.get(&key) {
            return Ok(hit);
        }
    }

    let file = OpenOptions::new().read(true).open_loc(loc)?;
    let ta_head = verify_ta_elf_signature_if_applicable(key.as_str(), &file)?;

    let mut guard = TA_HEAD_CACHE.lock();
    if let Some(hit) = guard.get(&key) {
        return Ok(hit);
    }
    let out = ta_head.clone();
    guard.insert(key, ta_head);
    Ok(out)
}

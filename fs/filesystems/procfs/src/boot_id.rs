// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-boot identifier exposed as `/proc/sys/kernel/random/boot_id`.
//!
//! systemd reads `boot_id` while building a service's exec context (it seeds
//! invocation IDs from it); a missing node makes the spawn fail with `ENOENT`.
//! The value is a random UUID drawn once from the kernel entropy pool and kept
//! for the lifetime of the boot, matching Linux's per-boot semantics.

use alloc::string::String;

use ksync::Mutex;

// `Mutex::new` binds a per-init-site lock class when `lock_stat` is enabled, so
// it is not a constant expression there; `static_lock!` is the workspace's
// portable spelling for a static lock and is a plain item otherwise.
ksync::static_lock! {
    static BOOT_ID: Mutex<Option<String>> = Mutex::new(None);
}

/// Returns the boot's UUID, generating it on first use.
pub(crate) fn boot_id() -> String {
    let mut guard = BOOT_ID.lock();
    if let Some(id) = guard.as_ref() {
        return id.clone();
    }
    let mut bytes = [0u8; 16];
    entropy::fill_random(&mut bytes);
    // RFC 4122 version 4 / variant bits, as Linux reports.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let id = alloc::format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:\
         02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15],
    );
    *guard = Some(id.clone());
    id
}

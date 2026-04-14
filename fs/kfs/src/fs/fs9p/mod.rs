// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! 9P filesystem adapter.
mod fs;
mod inode;
mod util;

use alloc::{format, string::String};

pub use fs::*;
pub use inode::*;
use kdriver::Virtio9pDevice;
use kspin::SpinNoPreempt as Mutex;

/// Virtio transport adapter bridging `kdriver::Virtio9pDevice` to `fs9p::Transport`.
///
/// The fs9p `Transport` trait requires `&self` (shared reference), whereas
/// `Virtio9pDevice::request` requires `&mut self`, so we wrap the device in
/// a mutex to provide interior mutability.
struct VirtioTransport(Mutex<Virtio9pDevice>);

impl fs9p::Transport for VirtioTransport {
    fn request(&self, req: &[u8], resp: &mut [u8]) -> Result<usize, String> {
        let mut dev = self.0.lock();
        dev.request(req, resp)
            .map_err(|e| format!("virtio-9p error: {:?}", e))
    }
}

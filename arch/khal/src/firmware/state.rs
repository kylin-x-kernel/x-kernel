// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use lazyinit::LazyInit;

use super::CMDLINE_BUF_SIZE;

static CMDLINE: LazyInit<([u8; CMDLINE_BUF_SIZE], usize)> = LazyInit::new();
static DTB_CAPTURE: LazyInit<(usize, usize, usize)> = LazyInit::new();

pub fn cmdline() -> Option<&'static str> {
    if let Some((buf, len)) = CMDLINE.get() {
        if *len > 0 {
            return core::str::from_utf8(&buf[..*len]).ok();
        }
        return None;
    }
    of::chosen_bootargs()
}

pub fn dtb_capture_region() -> Option<(usize, usize, usize)> {
    DTB_CAPTURE.get().copied()
}

#[cfg(target_os = "none")]
pub(super) fn init_cmdline(buf: [u8; CMDLINE_BUF_SIZE], len: usize) {
    CMDLINE.init_once((buf, len));
}

#[cfg(target_os = "none")]
pub(super) fn init_dtb_capture(paddr: usize, vaddr: usize, size: usize) {
    if paddr != 0 && vaddr != 0 && size != 0 {
        DTB_CAPTURE.init_once((paddr, vaddr, size));
    }
}

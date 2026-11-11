// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test]
fn signal_context_accepts_installed_user_segments_and_masks_privileged_flags() {
    let mut user = UserContext::new(0x1000, memaddr::VirtAddr::from_usize(0x2000), 0);
    let mut frame = MContext::new(&user);
    assert!(frame.validate().is_ok());
    frame.gregs[MContext::REG_EFL] = -1;
    frame.restore(&mut user);
    assert_eq!(user.cs, UserContext::USER_CODE_SELECTOR);
    assert_eq!(user.ss, UserContext::USER_DATA_SELECTOR);
    assert_eq!(user.rflags & (3 << 12), 0); // IOPL remains kernel-controlled.
    assert_eq!(user.orig_rax, u64::MAX);
    frame.gregs[MContext::REG_CSGSFS] = 8;
    assert!(frame.validate().is_err());
}

#[def_test]
fn signal_context_rejects_kernel_or_noncanonical_return_addresses() {
    let user = UserContext::new(0x1000, memaddr::VirtAddr::from_usize(0x2000), 0);
    for register in [MContext::REG_RIP, MContext::REG_RSP] {
        for address in [-1, 1 << 47] {
            let mut frame = MContext::new(&user);
            frame.gregs[register] = address;
            assert!(frame.validate().is_err());
        }
    }
}

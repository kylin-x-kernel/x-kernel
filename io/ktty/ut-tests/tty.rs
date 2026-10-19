// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kerrno::KError;
use unittest::def_test;

use super::{Tty, TtyKind, pty::create_pty_pair};
use crate::port::{ProcessMode, TtyConfig, TtyRead, TtyWrite};

#[derive(Clone)]
struct EmptyIo;

impl TtyRead for EmptyIo {
    fn read(&mut self, _buf: &mut [u8]) -> usize {
        0
    }
}

impl TtyWrite for EmptyIo {
    fn write(&self, buf: &[u8]) -> kerrno::KResult<usize> {
        Ok(buf.len())
    }
}

#[def_test(user, serial)]
fn pty_master_cannot_become_controlling_terminal() {
    let (master, _) = create_pty_pair();
    let process = kprocess::current_user_process();

    assert_eq!(master.bind_to(&process), Err(KError::NotATty));
}

#[def_test(user, serial)]
fn master_identity_does_not_depend_on_input_processing() {
    let master = Tty::new(
        Arc::default(),
        TtyKind::PtyMaster,
        TtyConfig {
            reader: EmptyIo,
            writer: EmptyIo,
            process_mode: ProcessMode::Manual,
        },
    );
    let process = kprocess::current_user_process();
    assert_eq!(master.bind_to(&process), Err(KError::NotATty));
}

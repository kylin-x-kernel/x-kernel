// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;

use iov_iter::{IovSource, iov_iter_source};
use kerrno::KError;
use unittest::{assert_eq, def_test};

use super::{CompleteCommandOptions, EmptyCommandPolicy, copy_complete_command};

struct DataSource {
    data: Vec<u8>,
    pos: usize,
}

impl IovSource for DataSource {
    fn count(&self) -> usize {
        self.data.len() - self.pos
    }

    fn copy_from_iter(&mut self, dst: &mut [u8]) -> kerrno::KResult<usize> {
        let count = dst.len().min(self.count());
        dst[..count].copy_from_slice(&self.data[self.pos..self.pos + count]);
        self.pos += count;
        Ok(count)
    }
}

#[def_test]
fn command_transport_copies_once_and_observes_empty_policy() {
    let options = CompleteCommandOptions {
        max_bytes: 16,
        empty: EmptyCommandPolicy::NoOp,
    };
    let mut empty = DataSource {
        data: Vec::new(),
        pos: 0,
    };
    let mut iter = iov_iter_source(&mut empty);
    assert_eq!(copy_complete_command(&mut iter, options).unwrap(), None);

    let mut data = DataSource {
        data: b"command".to_vec(),
        pos: 0,
    };
    let mut iter = iov_iter_source(&mut data);
    assert_eq!(
        copy_complete_command(&mut iter, options).unwrap().unwrap(),
        b"command".to_vec()
    );
}

#[def_test]
fn command_transport_callback_empty_and_bounds() {
    let options = CompleteCommandOptions {
        max_bytes: 4,
        empty: EmptyCommandPolicy::Callback,
    };
    let mut empty = DataSource {
        data: Vec::new(),
        pos: 0,
    };
    let mut iter = iov_iter_source(&mut empty);
    assert_eq!(
        copy_complete_command(&mut iter, options).unwrap().unwrap(),
        Vec::<u8>::new()
    );

    let mut data = DataSource {
        data: b"12345".to_vec(),
        pos: 0,
    };
    let mut iter = iov_iter_source(&mut data);
    assert_eq!(
        copy_complete_command(&mut iter, options),
        Err(crate::VfsError::FileTooLarge)
    );
}

struct FaultSource {
    pos: usize,
}

impl IovSource for FaultSource {
    fn count(&self) -> usize {
        4
    }

    fn copy_from_iter(&mut self, dst: &mut [u8]) -> kerrno::KResult<usize> {
        if self.pos == 0 {
            self.pos = 2;
            dst[0] = b'a';
            dst[1] = b'b';
            Ok(2)
        } else {
            Err(KError::BadAddress)
        }
    }

    fn revert(&mut self, count: usize) -> kerrno::KResult<()> {
        self.pos = self.pos.checked_sub(count).ok_or(KError::InvalidInput)?;
        Ok(())
    }
}

#[def_test]
fn command_transport_copy_fault_reverts_transport() {
    let options = CompleteCommandOptions {
        max_bytes: 16,
        empty: EmptyCommandPolicy::NoOp,
    };
    let mut source = FaultSource { pos: 0 };
    let mut iter = iov_iter_source(&mut source);
    assert_eq!(
        copy_complete_command(&mut iter, options),
        Err(crate::VfsError::BadAddress)
    );
    assert_eq!(iter.count(), 4);
}

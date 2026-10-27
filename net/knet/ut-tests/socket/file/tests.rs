// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, vec};

use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::unix::{DgramTransport, UnixDomainSocket, UnixSocketType};

#[def_test(user)]
fn unix_datagram_file_io_preserves_empty_and_large_messages() {
    let (sender, receiver) = DgramTransport::new_pair(7);
    let files = [sender, receiver].map(|transport| {
        sock_alloc_file(
            Socket::Unix(Box::new(UnixDomainSocket::new(
                transport,
                UnixSocketType::Datagram,
            ))),
            (OpenFlags::READ_WRITE | OpenFlags::NONBLOCK).bits(),
            kprocess::current_cred(),
        )
        .unwrap()
    });

    for len in [0, 4097, 8192] {
        let payload = vec![0x5a; len];
        assert_eq!(files[0].write(&payload).unwrap(), len);
        assert!(files[1].poll().contains(IoEvents::IN));
        assert_eq!(files[1].read(&mut []), Ok(0));
        assert!(files[1].poll().contains(IoEvents::IN));

        // Extra capacity must not cause a second receive after this message.
        let mut received = vec![0; len + 1];
        assert_eq!(files[1].read(&mut received).unwrap(), len);
        assert_eq!(&received[..len], &payload);
        assert!(!files[1].poll().contains(IoEvents::IN));
        assert_eq!(files[1].read(&mut received), Err(KError::WouldBlock));
    }
}

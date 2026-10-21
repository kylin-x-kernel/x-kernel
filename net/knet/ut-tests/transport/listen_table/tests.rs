// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use smoltcp::wire::{IpAddress, Ipv4Address};
use unittest::{assert, def_test};

use super::*;

fn pending_conn() -> PendingConn {
    PendingConn {
        accepted: AcceptedTcp {
            handle: SocketHandle::default(),
            local_endpoint: IpEndpoint::new(
                IpAddress::Ipv4(Ipv4Address::new(10, 0, 2, 15)),
                8080,
            ),
            remote_endpoint: IpEndpoint::new(
                IpAddress::Ipv4(Ipv4Address::new(10, 0, 2, 2)),
                49152,
            ),
        },
    }
}

#[def_test]
fn pending_handshake_remains_refreshable_after_packet_mark_is_consumed() {
    let mut entry = ListenTableEntry::new(1);
    entry.touched = true;
    assert!(entry.needs_refresh());

    entry.syn_queue.push_back(pending_conn());
    entry.touched = false;
    assert!(entry.needs_refresh());

    entry.syn_queue.clear();
    assert!(!entry.needs_refresh());

    entry.syn_queue.push_back(pending_conn());
    entry.closed = true;
    assert!(!entry.needs_refresh());
}

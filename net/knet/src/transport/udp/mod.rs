// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! UDP socket implementation.

mod input;
mod output;
mod pcb;
mod registry;
mod relay;
mod socket;
mod state;
mod wait;

pub(crate) use self::{
    input::{InputDisposition, deliver_ipv4_packet, prepare_ipv4_packet},
    pcb::PreparedUdpPacket,
    registry::{init_udp_registry, lookup_udp_error_state},
    state::UdpSocketQueuedError,
};
#[cfg(unittest)]
pub(crate) use self::{
    registry::{clear_udp_registry_for_test, register_udp_state_for_test},
    state::UdpSocketState,
};
pub use self::{relay::UdpDatagramRelay, socket::UdpSocket};

const IPV4_HEADER_LEN: usize = 20;
const IPV4_DEFAULT_TTL: u8 = 64;
const UDP_HEADER_LEN: usize = 8;
const UDP_RX_QUEUE_CAPACITY: usize = 1024;
const UDP_MAX_PAYLOAD_LEN: usize = u16::MAX as usize - IPV4_HEADER_LEN - UDP_HEADER_LEN;

unittest::path_mod_test!(tests, "../../../ut-tests/transport/udp/mod/tests.rs");

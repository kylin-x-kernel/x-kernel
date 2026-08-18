// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network stack constants and configuration defaults.
macro_rules! env_or_default {
    ($key:literal) => {
        match option_env!($key) {
            Some(val) => val,
            None => "",
        }
    };
}

pub const IP: &str = env_or_default!("K_IP");
pub const GATEWAY: &str = env_or_default!("K_GW");
pub const IP_PREFIX: u8 = 24;

pub const STANDARD_MTU: usize = 1500;

pub const TCP_RX_BUF_LEN: usize = 64 * 1024;
pub const TCP_TX_BUF_LEN: usize = 64 * 1024;
pub const UDP_RX_BUF_LEN: usize = 64 * 1024;
pub const UDP_TX_BUF_LEN: usize = 64 * 1024;
pub const RAW_RX_BUF_LEN: usize = 64 * 1024;
pub const RAW_TX_BUF_LEN: usize = 64 * 1024;
pub const LISTEN_QUEUE_SIZE: usize = 512;

pub const SOCKET_BUFFER_SIZE: usize = 64;
pub const ETHERNET_MAX_PENDING_PACKETS: usize = 128;

/// Period for sampling the atomic protocol/deferred-close deadline.
///
/// This is independent of the dynamic schedule timer. 10 ms matches the
/// historical 100 Hz tick sampling interval so TCP/orphan timers are not
/// delayed by coarser polling.
pub const TIMER_SAMPLE_PERIOD: core::time::Duration = core::time::Duration::from_millis(10);

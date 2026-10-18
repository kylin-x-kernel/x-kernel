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

pub(crate) const IP: &str = env_or_default!("K_IP");
pub(crate) const GATEWAY: &str = env_or_default!("K_GW");
pub(crate) const IP_PREFIX: u8 = 24;

pub(crate) const STANDARD_MTU: usize = 1500;

pub(crate) const TCP_RX_BUF_LEN: usize = 64 * 1024;
pub(crate) const TCP_TX_BUF_LEN: usize = 64 * 1024;
pub(crate) const UDP_RX_BUF_LEN: usize = 64 * 1024;
pub(crate) const UDP_TX_BUF_LEN: usize = 64 * 1024;
pub(crate) const RAW_RX_BUF_LEN: usize = 64 * 1024;
pub(crate) const RAW_TX_BUF_LEN: usize = 64 * 1024;
pub(crate) const LISTEN_QUEUE_SIZE: usize = 512;

/// Default TCP keep-alive idle time in seconds, matching Linux tcp_keepalive_time.
pub(crate) const TCP_KEEPIDLE_DEFAULT_SECS: u32 = 7200;
/// Upper bound for TCP_KEEPIDLE, matching Linux MAX_TCP_KEEPIDLE.
pub(crate) const TCP_KEEPIDLE_MAX_SECS: u32 = 32767;

pub(crate) const SOCKET_BUFFER_SIZE: usize = 64;
pub(crate) const ETHERNET_MAX_PENDING_PACKETS: usize = 128;

/// Period for sampling the atomic protocol/deferred-close deadline.
///
/// This is independent of the dynamic schedule timer. 10 ms matches the
/// historical 100 Hz tick sampling interval so TCP/orphan timers are not
/// delayed by coarser polling.
pub(crate) const TIMER_SAMPLE_PERIOD: ktime_types::TimeSpan =
    ktime_types::TimeSpan::from_millis(10);

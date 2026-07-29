// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use bitflags::bitflags;
use linux_raw_sys::general::*;

bitflags! {
    /// I/O events.
    #[derive(Debug, Clone, Copy)]
    pub struct IoEvents: u32 {
        /// Available for read.
        const IN = POLLIN;
        /// Urgent data for read.
        const PRI = POLLPRI;
        /// Available for write.
        const OUT = POLLOUT;
        /// Error condition.
        const ERR = POLLERR;
        /// Hang up.
        const HUP = POLLHUP;
        /// Invalid request.
        const NVAL = POLLNVAL;
        /// Equivalent to [`IN`](Self::IN).
        const RDNORM = POLLRDNORM;
        /// Priority band data can be read.
        const RDBAND = POLLRDBAND;
        /// Equivalent to [`OUT`](Self::OUT).
        const WRNORM = POLLWRNORM;
        /// Priority data can be written.
        const WRBAND = POLLWRBAND;
        /// Message.
        const MSG = POLLMSG;
        /// Remove.
        const REMOVE = POLLREMOVE;
        /// Stream socket peer closed its write half.
        const RDHUP = POLLRDHUP;
        /// Events reported even when callers did not request them.
        const ALWAYS_POLL = Self::ERR.bits() | Self::HUP.bits();
    }
}

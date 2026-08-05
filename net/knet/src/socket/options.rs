// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Socket option types and configuration helpers.
use alloc::boxed::Box;

use enum_dispatch::enum_dispatch;
use kerrno::{KError, KResult, LinuxError};
use ktime_types::TimeSpan;

macro_rules! define_options {
    ($($name:ident($value:ty),)*) => {
        /// Operation to get a socket option.
        ///
        /// See [`Configurable::get_option`].
        pub enum GetSocketOption<'a> {
            $(
                $name(&'a mut $value),
            )*
        }

        /// Operation to set a socket option.
        ///
        /// See [`Configurable::set_option`].
        #[derive(Clone, Copy)]
        pub enum SetSocketOption<'a> {
            $(
                $name(&'a $value),
            )*
        }
    };
}

/// Credentials delivered over Unix-domain sockets.
#[repr(C)]
#[derive(Default, Debug, Clone)]
pub struct UnixCredentials {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
}

impl UnixCredentials {
    pub fn new(pid: u32) -> Self {
        UnixCredentials {
            pid,
            uid: 0,
            gid: 0,
        }
    }
}

#[derive(Default, Debug, Clone, Copy)]
pub struct PacketStatistics {
    pub packets: u32,
    pub drops: u32,
}

#[derive(Default, Debug, Clone, Copy)]
pub struct PacketMembership {
    pub ifindex: i32,
    pub membership_type: u16,
    pub addr_len: u16,
    pub addr: [u8; 8],
}

define_options! {
    // ---- Socket-wide options ----
    ReuseAddress(bool),
    Error(i32),
    DontRoute(bool),
    SendBuffer(usize),
    ReceiveBuffer(usize),
    KeepAlive(bool),
    SendTimeout(TimeSpan),
    ReceiveTimeout(TimeSpan),
    SendBufferForce(usize),
    PassCredentials(bool),
    PeerCredentials(UnixCredentials),

    // ---- TCP options ----
    NoDelay(bool),
    MaxSegment(usize),
    TcpInfo(()),

    // ---- IP options ----
    Ttl(u8),
    RecvErr(bool),
    MtuDiscover(u8),

    // ---- Packet socket options (PACKET_*) ----
    PacketStatistics(PacketStatistics),
    PacketAddMembership(PacketMembership),
    PacketDropMembership(PacketMembership),

    // ---- Extra options ----
    NonBlocking(bool),
}

/// Whether a socket option is handled by a specific socket implementation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OptionHandled {
    Yes,
    No,
}

impl OptionHandled {
    pub fn is_yes(self) -> bool {
        self == Self::Yes
    }
}

/// Trait for configurable socket-like objects.
#[enum_dispatch]
pub trait Configurable {
    /// Get a socket option if the socket supports it.
    fn get_option_inner(&self, opt: &mut GetSocketOption) -> KResult<OptionHandled>;
    /// Set a socket option if the socket supports it.
    fn set_option_inner(&self, opt: SetSocketOption) -> KResult<OptionHandled>;

    fn get_option(&self, mut opt: GetSocketOption) -> KResult {
        match self.get_option_inner(&mut opt)? {
            OptionHandled::Yes => Ok(()),
            OptionHandled::No => Err(KError::from(LinuxError::ENOPROTOOPT)),
        }
    }
    fn set_option(&self, opt: SetSocketOption) -> KResult {
        match self.set_option_inner(opt)? {
            OptionHandled::Yes => Ok(()),
            OptionHandled::No => Err(KError::from(LinuxError::ENOPROTOOPT)),
        }
    }
}

impl<T: Configurable + ?Sized> Configurable for Box<T> {
    fn get_option_inner(&self, opt: &mut GetSocketOption) -> KResult<OptionHandled> {
        (**self).get_option_inner(opt)
    }

    fn set_option_inner(&self, opt: SetSocketOption) -> KResult<OptionHandled> {
        (**self).set_option_inner(opt)
    }
}

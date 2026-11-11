// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Stable network interface identifiers.

use kerrno::LinuxError;

/// A network interface index, independent of its position in the device table.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct NetDeviceId(i32);

impl NetDeviceId {
    pub(crate) const LOOPBACK: Self = Self(1);

    pub(crate) const fn new(ifindex: i32) -> Option<Self> {
        if ifindex > 0 {
            Some(Self(ifindex))
        } else {
            None
        }
    }

    /// Converts an optional interface index: zero is unspecified and a negative
    /// index is `ENODEV`. A positive ID does not establish device existence.
    pub(crate) fn from_optional_ifindex(ifindex: i32) -> Result<Option<Self>, LinuxError> {
        if ifindex == 0 {
            Ok(None)
        } else {
            Self::new(ifindex).map(Some).ok_or(LinuxError::ENODEV)
        }
    }

    pub(crate) const fn ifindex(self) -> i32 {
        self.0
    }
}

/// Interface selection for receive waiter registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RxDeviceSelection {
    All,
    Interface(NetDeviceId),
}

impl RxDeviceSelection {
    pub(crate) fn includes(self, id: NetDeviceId) -> bool {
        match self {
            Self::All => true,
            Self::Interface(selected) => selected == id,
        }
    }
}

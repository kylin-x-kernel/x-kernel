// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Stable network interface identifiers.

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

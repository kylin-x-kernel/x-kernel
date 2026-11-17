// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The port-I/O capability used by firmware emulation and legacy devices.

use crate::ResResult;

/// Access width of one port-I/O operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PortWidth {
    /// One byte (`inb`/`outb`).
    U8,
    /// One 16-bit word (`inw`/`outw`).
    U16,
    /// One 32-bit word (`inl`/`outl`).
    U32,
}

impl PortWidth {
    /// Width in bytes.
    pub const fn bytes(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::U16 => 2,
            Self::U32 => 4,
        }
    }

    /// Mask selecting the bits a value of this width may contain.
    pub const fn value_mask(self) -> u32 {
        match self {
            Self::U8 => u8::MAX as u32,
            Self::U16 => u16::MAX as u32,
            Self::U32 => u32::MAX,
        }
    }

    /// Returns whether `value` fits this width without truncation.
    pub const fn accepts(self, value: u32) -> bool {
        value & !self.value_mask() == 0
    }
}

/// Port-space capability.
///
/// A host implements this over its architecture's port instructions. Hosts
/// without port space return [`ResError::Unsupported`](crate::ResError::Unsupported).
///
/// The whole access must fit in the 16-bit port space: `port + width` may not
/// wrap. A write value must fit the selected width; implementations reject a
/// wider value rather than silently truncating it.
pub trait PortOp: Sync {
    /// Read `width` bytes starting at `port`, zero-extended into a `u32`.
    fn read_port(&self, port: u16, width: PortWidth) -> ResResult<u32>;

    /// Write the low `width` bytes of `value` to `port`.
    fn write_port(&self, port: u16, width: PortWidth, value: u32) -> ResResult<()>;
}
